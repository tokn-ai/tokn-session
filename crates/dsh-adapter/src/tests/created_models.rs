use super::*;

fn choice() -> ModelIdentity {
  ModelIdentity {
    model_provider: Some("provider".into()),
    model_id: "model".into(),
    display_name: Some("Model".into()),
  }
}

async fn create(adapter: &DshAdapter) -> CreatedSession {
  adapter
    .create_session(CreateSessionOptions {
      session_id: new_session_id(),
      working_directory: "/workspace".into(),
      agent_preset: None,
    })
    .await
    .unwrap()
}

async fn calls(fake: &Fake, method: &str) -> usize {
  fake
    .calls
    .lock()
    .await
    .iter()
    .filter(|call| call["method"] == method)
    .count()
}

fn command(adapter: &DshAdapter, created: &CreatedSession, command: Command) -> InteractionRequest {
  let mut request = request(adapter, command);
  request.target = created.target.clone();
  request
}

#[tokio::test]
async fn creation_choice_is_applied_confirmed_once_and_keeps_native_default_behavior() {
  let server = server(Vec::new()).await;
  let adapter = attach(&server).await;
  let created = create(&adapter).await;
  let id = created.target.session_id.as_deref().unwrap();
  assert_eq!(
    *server.fake.default_selection.lock().await,
    json!({"provider":"provider","model":"default-model"})
  );
  assert_eq!(calls(&server.fake, "session.selectModel").await, 0);
  adapter.prepare_created_model(id, &choice()).await.unwrap();
  let Reply::State(state) = adapter
    .execute(command(&adapter, &created, Command::State))
    .await
    .unwrap()
  else {
    panic!()
  };
  assert_eq!(state.model.unwrap().model_id, "model");
  assert_eq!(
    *server.fake.default_selection.lock().await,
    json!({"provider":"provider","model":"model"})
  );
  adapter.prepare_created_model(id, &choice()).await.unwrap();
  assert_eq!(calls(&server.fake, "session.selectModel").await, 1);
  assert_eq!(calls(&server.fake, "session.models").await, 1);
  let recorded = server.fake.calls.lock().await;
  let methods = recorded
    .iter()
    .map(|call| call["method"].as_str().unwrap())
    .collect::<Vec<_>>();
  assert_eq!(
    methods,
    [
      "session.create",
      "session.list",
      "llm.models",
      "session.list",
      "session.selectModel",
      "session.models",
      "session.list"
    ]
  );
  let setter = recorded
    .iter()
    .find(|call| call["method"] == "session.selectModel")
    .unwrap();
  assert_eq!(
    setter["payload"],
    json!({"sessionId":id,"provider":"provider","model":"model"})
  );
  drop(recorded);
  assert!(!adapter.descriptor().capabilities.atomic_model_on_send);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn foreign_or_nonblank_sessions_never_receive_creation_model_mutations() {
  let server = server(Vec::new()).await;
  let adapter = attach(&server).await;
  assert_eq!(
    adapter
      .prepare_created_model("session", &choice())
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Unsupported
  );
  assert!(server.fake.calls.lock().await.is_empty());
  for field in ["running", "blank"] {
    let created = create(&adapter).await;
    let id = created.target.session_id.as_deref().unwrap();
    {
      let mut sessions = server.fake.created_sessions.lock().await;
      let row = sessions.iter_mut().find(|row| row["sessionId"] == id).unwrap();
      row[field] = json!(field == "running");
    }
    assert_eq!(
      adapter.prepare_created_model(id, &choice()).await.unwrap_err().kind,
      ErrorKind::Conflict
    );
  }
  assert_eq!(calls(&server.fake, "session.selectModel").await, 0);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn unavailable_or_ambiguous_model_rejects_before_native_setter() {
  let server = server(Vec::new()).await;
  let adapter = attach(&server).await;
  for models in [
    json!([]),
    json!([{"id":"model","name":"Model"},{"id":"model","name":"Duplicate"}]),
  ] {
    let created = create(&adapter).await;
    *server.fake.model_catalog.lock().await =
      json!({"groups":[{"id":"provider","name":"Provider","models":models}],"failures":[]});
    assert_eq!(
      adapter
        .prepare_created_model(created.target.session_id.as_deref().unwrap(), &choice())
        .await
        .unwrap_err()
        .kind,
      ErrorKind::InvalidRequest
    );
  }
  assert_eq!(calls(&server.fake, "session.selectModel").await, 0);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn rejected_or_uncertain_selection_is_retained_without_setter_or_prompt_replay() {
  for (fault, kind) in [
    (1, ErrorKind::Rejected),
    (2, ErrorKind::Uncertain),
    (3, ErrorKind::Uncertain),
    (4, ErrorKind::Uncertain),
  ] {
    let server = server(Vec::new()).await;
    let adapter = attach(&server).await;
    let created = create(&adapter).await;
    let id = created.target.session_id.as_deref().unwrap();
    server.fake.model_fault.store(fault, Ordering::Relaxed);
    let failure = adapter.prepare_created_model(id, &choice()).await.unwrap_err();
    assert_eq!(failure.kind, kind);
    assert!(!failure.message.contains("private-model-diagnostic"));
    server.fake.model_fault.store(0, Ordering::Relaxed);
    assert_eq!(adapter.prepare_created_model(id, &choice()).await.unwrap_err(), failure);
    assert_eq!(calls(&server.fake, "session.selectModel").await, 1);
    assert_eq!(calls(&server.fake, "session.create").await, 1);
    assert_eq!(calls(&server.fake, "session.prompt").await, 0);
    assert!(
      matches!(adapter.execute(command(&adapter, &created, Command::State)).await.unwrap(), Reply::State(state) if state.model.is_none())
    );
    adapter.close().await.unwrap();
  }
}

#[tokio::test]
async fn caller_cancellation_cannot_release_or_repeat_in_flight_selection() {
  let server = server(Vec::new()).await;
  let adapter = Arc::new(attach(&server).await);
  let created = create(&adapter).await;
  let id = created.target.session_id.clone().unwrap();
  let permits = server.fake.selection_gate.acquire_many(64).await.unwrap();
  let selected = adapter.clone();
  let target = id.clone();
  let task = tokio::spawn(async move { selected.prepare_created_model(&target, &choice()).await });
  tokio::time::timeout(Duration::from_secs(2), async {
    while calls(&server.fake, "session.selectModel").await == 0 {
      tokio::task::yield_now().await;
    }
  })
  .await
  .unwrap();
  let failure = adapter
    .execute(command(
      &adapter,
      &created,
      Command::Message {
        input: MessageInput {
          text: "must wait".into(),
          ..Default::default()
        },
        delivery: DeliveryMode::StartOrSteer,
      },
    ))
    .await
    .unwrap_err();
  assert_eq!(failure.kind, ErrorKind::Conflict);
  assert_eq!(calls(&server.fake, "session.prompt").await, 0);
  task.abort();
  assert!(task.await.unwrap_err().is_cancelled());
  drop(permits);
  tokio::time::timeout(Duration::from_secs(2), adapter.prepare_created_model(&id, &choice()))
    .await
    .unwrap()
    .unwrap();
  assert_eq!(calls(&server.fake, "session.selectModel").await, 1);
  assert_eq!(calls(&server.fake, "session.prompt").await, 0);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn first_prompt_retires_model_preparation_without_changing_defaults() {
  let server = server(Vec::new()).await;
  let adapter = attach(&server).await;
  let created = create(&adapter).await;
  adapter
    .execute(command(
      &adapter,
      &created,
      Command::Message {
        input: MessageInput {
          text: "hello".into(),
          ..Default::default()
        },
        delivery: DeliveryMode::StartOrSteer,
      },
    ))
    .await
    .unwrap();
  assert_eq!(
    adapter
      .prepare_created_model(created.target.session_id.as_deref().unwrap(), &choice())
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Unsupported
  );
  assert_eq!(calls(&server.fake, "session.selectModel").await, 0);
  assert_eq!(
    *server.fake.default_selection.lock().await,
    json!({"provider":"provider","model":"default-model"})
  );
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn later_native_model_observations_outrank_preparation_confirmation() {
  let server = server(Vec::new()).await;
  let adapter = attach(&server).await;
  let created = create(&adapter).await;
  let id = created.target.session_id.as_deref().unwrap();
  adapter.prepare_created_model(id, &choice()).await.unwrap();
  events::test_ingest(
    &adapter.inner,
    frame(
      "live-model",
      "session/event",
      json!({
        "type":"session/event","sessionId":id,
        "event":{"type":"request/context","data":{"provider":"provider","model":"later-model"}}
      }),
    ),
  )
  .await
  .unwrap();
  adapter.prepare_created_model(id, &choice()).await.unwrap();
  let Reply::State(state) = adapter
    .execute(command(&adapter, &created, Command::State))
    .await
    .unwrap()
  else {
    panic!()
  };
  assert_eq!(state.model.unwrap().model_id, "later-model");
  assert_eq!(calls(&server.fake, "session.selectModel").await, 1);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn concurrent_native_model_observation_prevents_confirmation_from_overwriting_it() {
  let server = server(Vec::new()).await;
  let adapter = Arc::new(attach(&server).await);
  let created = create(&adapter).await;
  let id = created.target.session_id.clone().unwrap();
  let permits = server.fake.selection_gate.acquire_many(64).await.unwrap();
  let selecting = adapter.clone();
  let target = id.clone();
  let task = tokio::spawn(async move { selecting.prepare_created_model(&target, &choice()).await });
  tokio::time::timeout(Duration::from_secs(2), async {
    while calls(&server.fake, "session.selectModel").await == 0 {
      tokio::task::yield_now().await;
    }
  })
  .await
  .unwrap();
  events::test_ingest(
    &adapter.inner,
    frame(
      "concurrent-model",
      "session/event",
      json!({
        "type":"session/event","sessionId":id,
        "event":{"type":"request/context","data":{"provider":"provider","model":"concurrent-model"}}
      }),
    ),
  )
  .await
  .unwrap();
  drop(permits);
  assert_eq!(task.await.unwrap().unwrap_err().kind, ErrorKind::Uncertain);
  let Reply::State(state) = adapter
    .execute(command(&adapter, &created, Command::State))
    .await
    .unwrap()
  else {
    panic!()
  };
  assert_eq!(state.model.unwrap().model_id, "concurrent-model");
  assert_eq!(calls(&server.fake, "session.selectModel").await, 1);
  assert_eq!(calls(&server.fake, "session.prompt").await, 0);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn matching_native_model_observation_still_rejects_activity_during_preparation() {
  let server = server(Vec::new()).await;
  let adapter = Arc::new(attach(&server).await);
  let created = create(&adapter).await;
  let id = created.target.session_id.clone().unwrap();
  let permits = server.fake.selection_gate.acquire_many(64).await.unwrap();
  let selecting = adapter.clone();
  let target = id.clone();
  let task = tokio::spawn(async move { selecting.prepare_created_model(&target, &choice()).await });
  tokio::time::timeout(Duration::from_secs(2), async {
    while calls(&server.fake, "session.selectModel").await == 0 {
      tokio::task::yield_now().await;
    }
  })
  .await
  .unwrap();
  events::test_ingest(
    &adapter.inner,
    frame(
      "matching-model",
      "session/event",
      json!({
        "type":"session/event","sessionId":id,
        "event":{"type":"request/context","data":{"provider":"provider","model":"model"}}
      }),
    ),
  )
  .await
  .unwrap();
  drop(permits);
  assert_eq!(task.await.unwrap().unwrap_err().kind, ErrorKind::Uncertain);
  let Reply::State(state) = adapter
    .execute(command(&adapter, &created, Command::State))
    .await
    .unwrap()
  else {
    panic!()
  };
  assert_eq!(state.model.unwrap().model_id, "model");
  assert_eq!(calls(&server.fake, "session.selectModel").await, 1);
  assert_eq!(calls(&server.fake, "session.prompt").await, 0);
  adapter.close().await.unwrap();
}
