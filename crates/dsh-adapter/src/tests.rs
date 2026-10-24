use super::*;
use axum::{
  Router,
  body::Bytes,
  extract::{
    Path, State,
    ws::{Message, WebSocket, WebSocketUpgrade},
  },
  response::Response,
  routing::{get, post},
};

struct Fake {
  calls: Mutex<Vec<Value>>,
  mux: broadcast::Sender<String>,
  initial: Vec<Value>,
  wrong_response_id: AtomicBool,
  silent_downlinks: AtomicBool,
  pong_count: AtomicU64,
  created_sessions: Mutex<Vec<Value>>,
  creation_fault: AtomicU64,
}

struct Server {
  endpoint: String,
  fake: Arc<Fake>,
  task: JoinHandle<()>,
}

impl Drop for Server {
  fn drop(&mut self) {
    self.task.abort();
  }
}

async fn server(initial: Vec<Value>) -> Server {
  let (mux, _) = broadcast::channel(32);
  let fake = Arc::new(Fake {
    calls: Mutex::new(Vec::new()),
    mux,
    initial,
    wrong_response_id: AtomicBool::new(false),
    silent_downlinks: AtomicBool::new(false),
    pong_count: AtomicU64::new(0),
    created_sessions: Mutex::new(Vec::new()),
    creation_fault: AtomicU64::new(0),
  });
  let router = Router::new()
    .route("/api/events.mux", get(mux_stream))
    .route("/api/events.host", get(host_stream))
    .route("/api/{method}", post(call))
    .with_state(fake.clone());
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let endpoint = format!("http://{}", listener.local_addr().unwrap());
  let task = tokio::spawn(async move {
    axum::serve(listener, router).await.unwrap();
  });
  Server { endpoint, fake, task }
}

async fn attach(server: &Server) -> DshAdapter {
  let adapter = DshAdapter::attach(ApiProxyConfig::new(&server.endpoint)).await.unwrap();
  server.fake.calls.lock().await.clear();
  adapter
}

async fn mux_stream(State(fake): State<Arc<Fake>>, upgrade: WebSocketUpgrade) -> Response {
  if fake.silent_downlinks.load(Ordering::Acquire) {
    return upgrade.on_upgrade(silent_downlink);
  }
  let receiver = fake.mux.subscribe();
  let initial = fake.initial.clone();
  upgrade.on_upgrade(move |socket| downlink(socket, initial, receiver, fake))
}

async fn host_stream(State(fake): State<Arc<Fake>>, upgrade: WebSocketUpgrade) -> Response {
  if fake.silent_downlinks.load(Ordering::Acquire) {
    return upgrade.on_upgrade(silent_downlink);
  }
  let receiver = fake.mux.subscribe();
  upgrade.on_upgrade(move |socket| downlink(socket, Vec::new(), receiver, fake))
}

async fn silent_downlink(socket: WebSocket) {
  std::future::pending::<()>().await;
  drop(socket);
}

async fn downlink(
  mut socket: WebSocket,
  initial: Vec<Value>,
  mut receiver: broadcast::Receiver<String>,
  fake: Arc<Fake>,
) {
  for native in initial {
    if socket.send(Message::Text(native.to_string().into())).await.is_err() {
      return;
    }
  }
  loop {
    tokio::select! {
      next = receiver.recv() => match next {
        Ok(text) => if socket.send(Message::Text(text.into())).await.is_err() { return; },
        Err(_) => return,
      },
      next = socket.recv() => match next {
        Some(Ok(Message::Ping(data))) => {
          fake.pong_count.fetch_add(1, Ordering::Relaxed);
          if socket.send(Message::Pong(data)).await.is_err() { return; }
        },
        Some(Ok(Message::Pong(_))) => {},
        _ => return,
      },
    }
  }
}

async fn call(State(fake): State<Arc<Fake>>, Path(method): Path<String>, body: Bytes) -> axum::Json<Value> {
  let request: Value = serde_json::from_slice(&body).unwrap();
  fake.calls.lock().await.push(request.clone());
  if method == "respond" {
    return axum::Json(json!({"accepted":true}));
  }
  let value = match method.as_str() {
    "host.describe" => {
      json!({"version":"test-version","cwd":"/workspace","provider":"deepseek-official","model":"default-only","attachedSessions":1,"canOpenPath":false,"future":true})
    }
    "session.list" => {
      let mut items = vec![json!({"sessionId":"session","running":true,"cwd":"/workspace"})];
      items.extend(fake.created_sessions.lock().await.iter().cloned());
      json!({"items":items})
    }
    "session.create" => {
      let fault = fake.creation_fault.load(Ordering::Relaxed);
      if fault == 6 {
        return axum::Json(
          json!({"type":"server-response","rpcId":request["rpcId"],"result":{"ok":false,"error":{"code":"agent-preset-not-found","message":"unknown preset"}}}),
        );
      }
      let payload = &request["payload"];
      let preset = payload.get("agentPreset").cloned().unwrap_or(json!("standard"));
      let cwd = if fault == 2 {
        json!("/another/workspace")
      } else {
        payload["cwd"].clone()
      };
      fake
        .created_sessions
        .lock()
        .await
        .push(json!({"sessionId":payload["sessionId"], "cwd":cwd, "running":false, "agentPreset":preset}));
      if fault == 5 {
        return axum::Json(
          json!({"type":"server-response","rpcId":request["rpcId"],"result":{"ok":false,"error":{"code":"internal","message":"failed after publication"}}}),
        );
      }
      if fault == 4 {
        json!({"agentPreset":preset})
      } else {
        json!({"sessionId": if fault == 1 { json!("another-session") } else { payload["sessionId"].clone() },"agentPreset": if fault == 3 { json!("another-preset") } else { preset }})
      }
    }
    "llm.models" => {
      json!({"groups":[{"id":"provider","name":"Provider","models":[{"id":"model","name":"Model","future":true}]}],"failures":[{"id":"failed-provider","message":"offline"}]})
    }
    "session.prompt" | "session.cancel" => json!({"accepted":true}),
    _ => panic!("unexpected native method: {method}"),
  };
  let rpc_id = if fake.wrong_response_id.load(Ordering::Acquire) {
    json!("wrong-id")
  } else {
    request["rpcId"].clone()
  };
  axum::Json(json!({"type":"server-response","rpcId":rpc_id,"result":{"ok":true,"value":value}}))
}

#[tokio::test]
async fn creates_native_idle_sessions_with_effective_default_or_explicit_preset() {
  let server = server(Vec::new()).await;
  let adapter = attach(&server).await;
  for requested in [None, Some("custom".to_owned())] {
    let id = new_session_id();
    let created = adapter
      .create_session(CreateSessionOptions {
        session_id: id.clone(),
        working_directory: "/workspace".into(),
        agent_preset: requested.clone(),
      })
      .await
      .unwrap();
    assert_eq!(created.target.session_id.as_deref(), Some(id.as_str()));
    assert_eq!(created.working_directory, PathBuf::from("/workspace"));
    assert_eq!(
      created.agent_preset.as_deref(),
      Some(requested.as_deref().unwrap_or("standard"))
    );
  }
  let calls = server.fake.calls.lock().await;
  assert_eq!(
    calls.iter().filter(|call| call["method"] == "session.create").count(),
    2
  );
  assert_eq!(calls.iter().filter(|call| call["method"] == "session.list").count(), 2);
  assert!(
    calls
      .iter()
      .all(|call| call["method"] != "session.prompt" && call["method"] != "session.models")
  );
  assert!(calls[0]["payload"].get("agentPreset").is_none());
  drop(calls);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn creation_faults_after_mutation_remain_uncertain_and_are_not_retried() {
  for fault in 1..=5 {
    let server = server(Vec::new()).await;
    let adapter = attach(&server).await;
    server.fake.creation_fault.store(fault, Ordering::Relaxed);
    let error = adapter
      .create_session(CreateSessionOptions {
        session_id: new_session_id(),
        working_directory: "/workspace".into(),
        agent_preset: Some("standard".into()),
      })
      .await
      .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Uncertain, "fault {fault}");
    assert_eq!(
      server
        .fake
        .calls
        .lock()
        .await
        .iter()
        .filter(|call| call["method"] == "session.create")
        .count(),
      1
    );
    adapter.close().await.unwrap();
  }
}

#[tokio::test]
async fn invalid_creation_is_rejected_before_native_write() {
  let server = server(Vec::new()).await;
  let adapter = attach(&server).await;
  for (session_id, directory) in [("../escape", "/workspace"), ("safe-session", "relative")] {
    let error = adapter
      .create_session(CreateSessionOptions {
        session_id: session_id.into(),
        working_directory: directory.into(),
        agent_preset: None,
      })
      .await
      .unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidRequest);
  }
  assert!(server.fake.calls.lock().await.is_empty());
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn native_creation_configuration_refusal_remains_a_definite_rejection() {
  let server = server(Vec::new()).await;
  let adapter = attach(&server).await;
  server.fake.creation_fault.store(6, Ordering::Relaxed);
  let error = adapter
    .create_session(CreateSessionOptions {
      session_id: new_session_id(),
      working_directory: "/workspace".into(),
      agent_preset: Some("missing".into()),
    })
    .await
    .unwrap_err();
  assert_eq!(error.kind, ErrorKind::Rejected);
  assert!(server.fake.created_sessions.lock().await.is_empty());
  adapter.close().await.unwrap();
}

fn request(adapter: &DshAdapter, command: Command) -> InteractionRequest {
  let descriptor = adapter.descriptor();
  InteractionRequest {
    request_id: uuid::Uuid::new_v4().to_string(),
    target: SessionTarget {
      runtime_id: descriptor.runtime_id,
      instance_id: descriptor.instance_id,
      session_id: Some("session".into()),
    },
    command,
  }
}

fn frame(rpc_id: &str, method: &str, payload: Value) -> Value {
  json!({"type":"server-request","rpcId":rpc_id,"method":method,"payload":payload,"futureEnvelope":true})
}

fn question_frame() -> Value {
  frame(
    "question-native-id",
    "question/requested",
    json!({"type":"question/requested","sessionId":"session",
    "questions":[{"id":"q","question":"Choose?","multiSelect":true,"options":[{"label":"A"}],"detail":"Native detail retained"}]}),
  )
}

async fn wait_pending(adapter: &DshAdapter, count: usize) -> Vec<PendingInteraction> {
  tokio::time::timeout(Duration::from_secs(2), async {
    loop {
      if let Reply::Interactions(pending) = adapter
        .execute(request(adapter, Command::PendingInteractions))
        .await
        .unwrap()
      {
        if pending.len() == count {
          return pending;
        }
      }
      tokio::task::yield_now().await;
    }
  })
  .await
  .unwrap()
}

#[tokio::test]
async fn reads_metadata_state_and_catalog_without_resuming_sessions() {
  let server = server(Vec::new()).await;
  let adapter = attach(&server).await;
  let Reply::Metadata(metadata) = adapter.execute(request(&adapter, Command::Metadata)).await.unwrap() else {
    panic!()
  };
  assert_eq!(metadata.version.as_deref(), Some("test-version"));
  assert!(
    metadata.model.is_none(),
    "host defaults must not masquerade as the session model"
  );
  let Reply::State(state) = adapter.execute(request(&adapter, Command::State)).await.unwrap() else {
    panic!()
  };
  assert_eq!(state.status, ExecutionState::Running);
  let Reply::Models(models) = adapter.execute(request(&adapter, Command::Models)).await.unwrap() else {
    panic!()
  };
  assert_eq!(models[0].model.model_provider.as_deref(), Some("provider"));
  assert!(
    !models[0].supports_images,
    "native catalog does not establish image modality"
  );
  let calls = server.fake.calls.lock().await;
  assert_eq!(
    calls
      .iter()
      .map(|call| call["method"].as_str().unwrap())
      .collect::<Vec<_>>(),
    ["host.describe", "session.list", "llm.models"]
  );
  drop(calls);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn native_message_modes_preserve_rpc_identity_and_reject_unfenced_model_intent() {
  let server = server(Vec::new()).await;
  let adapter = attach(&server).await;
  let input = MessageInput {
    text: "hello".into(),
    ..MessageInput::default()
  };
  let send = request(
    &adapter,
    Command::Message {
      input: input.clone(),
      delivery: DeliveryMode::StartOrSteer,
    },
  );
  let expected_id = send.request_id.clone();
  let Reply::Admission(receipt) = adapter.execute(send).await.unwrap() else {
    panic!()
  };
  assert_eq!(receipt.disposition, InputDisposition::Accepted);
  let calls = server.fake.calls.lock().await;
  assert_eq!(calls[0]["rpcId"], expected_id);
  assert_eq!(calls[0]["payload"]["mode"], "steer");
  drop(calls);
  for delivery in [DeliveryMode::Start, DeliveryMode::Steer] {
    assert_eq!(
      adapter
        .execute(request(
          &adapter,
          Command::Message {
            input: input.clone(),
            delivery
          }
        ))
        .await
        .unwrap_err()
        .kind,
      ErrorKind::Unsupported
    );
  }
  let model_input = MessageInput {
    model: Some(ModelIdentity {
      model_provider: Some("provider".into()),
      model_id: "model".into(),
      display_name: None,
    }),
    ..input
  };
  assert_eq!(
    adapter
      .execute(request(
        &adapter,
        Command::Message {
          input: model_input,
          delivery: DeliveryMode::StartOrSteer
        }
      ))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Unsupported
  );
  assert_eq!(server.fake.calls.lock().await.len(), 1);
}

#[tokio::test]
async fn recovers_native_pending_questions_and_replies_with_their_correlation() {
  let server = server(vec![question_frame()]).await;
  let adapter = attach(&server).await;
  let pending = wait_pending(&adapter, 1).await;
  assert_eq!(
    pending[0].native["payload"]["questions"][0]["detail"],
    "Native detail retained"
  );
  events::test_ingest(&adapter.inner, question_frame()).await.unwrap();
  assert_eq!(
    wait_pending(&adapter, 1).await.len(),
    1,
    "stable replay must not duplicate pending interactions"
  );
  let answer = InteractionResponse::Answers {
    answers: vec![QuestionAnswer {
      question_id: "q".into(),
      answers: vec!["A".into()],
      custom: Some("extra".into()),
    }],
  };
  adapter
    .execute(request(
      &adapter,
      Command::Respond {
        interaction_id: "question-native-id".into(),
        response: answer,
      },
    ))
    .await
    .unwrap();
  let calls = server.fake.calls.lock().await;
  assert_eq!(calls[0]["rpcId"], "question-native-id");
  assert_eq!(calls[0]["type"], "client-response");
  assert_eq!(
    calls[0]["result"]["value"]["answer"]["answers"][0],
    json!({"id":"q","selected":["A"],"custom":"extra"})
  );
  drop(calls);
  wait_pending(&adapter, 0).await;
}

#[tokio::test]
async fn permission_approval_uses_audit_id_and_rejects_permanent_grants() {
  let native = frame(
    "approval-rpc",
    "approval/requested",
    json!({"type":"approval/requested","sessionId":"session","approvalId":"audit-id","toolName":"shell","callId":"tool-call"}),
  );
  let server = server(vec![native]).await;
  let adapter = attach(&server).await;
  wait_pending(&adapter, 1).await;
  let error = adapter
    .execute(request(
      &adapter,
      Command::Respond {
        interaction_id: "approval-rpc".into(),
        response: InteractionResponse::Permission {
          decision: PermissionDecision::AllowAlways,
        },
      },
    ))
    .await
    .unwrap_err();
  assert_eq!(error.kind, ErrorKind::Unsupported);
  adapter
    .execute(request(
      &adapter,
      Command::Respond {
        interaction_id: "approval-rpc".into(),
        response: InteractionResponse::Permission {
          decision: PermissionDecision::AllowOnce,
        },
      },
    ))
    .await
    .unwrap();
  assert_eq!(
    server.fake.calls.lock().await[0]["result"]["value"],
    json!({"sessionId":"session","approvalId":"audit-id","outcome":"allowed-once"})
  );
}

#[tokio::test]
async fn stale_targets_and_uncorrelated_mutation_responses_never_claim_success() {
  let server = server(Vec::new()).await;
  let adapter = attach(&server).await;
  let mut stale = request(&adapter, Command::CancelTurn);
  stale.target.instance_id = "old-instance".into();
  assert_eq!(adapter.execute(stale).await.unwrap_err().kind, ErrorKind::StaleTarget);
  assert!(server.fake.calls.lock().await.is_empty());
  server.fake.wrong_response_id.store(true, Ordering::Release);
  assert_eq!(
    adapter
      .execute(request(&adapter, Command::CancelTurn))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Uncertain
  );
  assert_eq!(server.fake.calls.lock().await.len(), 1, "mutations are never retried");
}

#[tokio::test]
async fn event_loss_disables_mutations_and_close_only_detaches() {
  let server = server(Vec::new()).await;
  let adapter = attach(&server).await;
  let mut events = adapter.subscribe();
  server
    .fake
    .mux
    .send(
      frame(
        "stream-error",
        "stream/error",
        json!({"type":"stream/error","error":{"code":"internal","message":"test"}}),
      )
      .to_string(),
    )
    .unwrap();
  tokio::time::timeout(Duration::from_secs(2), async {
    loop {
      if matches!(events.recv().await.unwrap().kind, RuntimeEventKind::Disconnected { .. }) {
        break;
      }
    }
  })
  .await
  .unwrap();
  assert_eq!(
    adapter
      .execute(request(&adapter, Command::CancelTurn))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Unavailable
  );
  assert_eq!(
    adapter
      .execute(request(&adapter, Command::State))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Unavailable
  );
  adapter.close().await.unwrap();
  let second = attach(&server).await;
  second.execute(request(&second, Command::Metadata)).await.unwrap();
  assert_eq!(
    server.fake.calls.lock().await.len(),
    1,
    "detach never posts shutdown or cancels native work"
  );
}

#[tokio::test]
async fn publication_sequences_match_delivery_order_across_concurrent_producers() {
  let server = server(Vec::new()).await;
  let adapter = attach(&server).await;
  let mut receiver = adapter.subscribe();
  let mut producers = Vec::new();
  for producer in 0..8 {
    let inner = adapter.inner.clone();
    producers.push(tokio::task::spawn_blocking(move || {
      for item in 0..16 {
        inner.emit(None, RuntimeEventKind::Native(json!({"producer":producer,"item":item})));
      }
    }));
  }
  for producer in producers {
    producer.await.unwrap();
  }
  let mut previous = None;
  for _ in 0..128 {
    let event = receiver.recv().await.unwrap();
    if let Some(previous) = previous {
      assert!(event.sequence > previous);
    }
    previous = Some(event.sequence);
  }
}

#[tokio::test]
async fn websocket_control_heartbeats_keep_a_healthy_idle_attachment_live() {
  let server = server(Vec::new()).await;
  let mut config = ApiProxyConfig::new(&server.endpoint);
  config.heartbeat_interval = Duration::from_millis(20);
  config.heartbeat_timeout = Duration::from_secs(2);
  let adapter = DshAdapter::attach(config).await.unwrap();
  tokio::time::timeout(Duration::from_secs(4), async {
    while server.fake.pong_count.load(Ordering::Acquire) < 4 {
      tokio::time::sleep(Duration::from_millis(5)).await;
    }
  })
  .await
  .unwrap();
  assert!(!adapter.inner.disconnected.load(Ordering::Acquire));
  adapter.execute(request(&adapter, Command::State)).await.unwrap();
}

#[tokio::test]
async fn missing_pong_invalidates_the_whole_attachment_and_state_reads() {
  let server = server(Vec::new()).await;
  server.fake.silent_downlinks.store(true, Ordering::Release);
  let mut config = ApiProxyConfig::new(&server.endpoint);
  config.heartbeat_interval = Duration::from_millis(20);
  config.heartbeat_timeout = Duration::from_millis(100);
  let adapter = DshAdapter::attach(config).await.unwrap();
  tokio::time::timeout(Duration::from_secs(2), async {
    while !adapter.inner.disconnected.load(Ordering::Acquire) {
      tokio::time::sleep(Duration::from_millis(5)).await;
    }
  })
  .await
  .unwrap();
  assert!(*adapter.inner.shutdown.borrow(), "failure must stop the peer downlink");
  assert_eq!(
    adapter
      .execute(request(&adapter, Command::State))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Unavailable
  );
  assert_eq!(
    adapter
      .execute(request(&adapter, Command::CancelTurn))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Unavailable
  );
  adapter.execute(request(&adapter, Command::Metadata)).await.unwrap();
}
