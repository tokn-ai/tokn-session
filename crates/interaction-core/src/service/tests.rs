use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Notify, oneshot};

struct FakeAdapter {
  descriptor: RuntimeDescriptor,
  events: broadcast::Sender<RuntimeEvent>,
  calls: AtomicUsize,
  closed: AtomicUsize,
  waiting: Mutex<Option<oneshot::Receiver<()>>>,
  started: Notify,
  query_waiting: Mutex<Option<oneshot::Receiver<()>>>,
  close_waiting: Mutex<Option<oneshot::Receiver<()>>>,
  close_started: Notify,
  close_finished: AtomicUsize,
}
impl FakeAdapter {
  fn new() -> Arc<Self> {
    Arc::new(Self {
      descriptor: RuntimeDescriptor {
        runtime_id: "test".into(),
        instance_id: "instance".into(),
        provider: Provider::Pi,
        ownership: Ownership::Attached,
        capabilities: Capabilities::default()
          .with(Operation::Metadata)
          .with(Operation::State)
          .with(Operation::Message)
          .with(Operation::Respond)
          .with(Operation::CancelTurn)
          .with(Operation::PendingInteractions),
      },
      events: broadcast::channel(8).0,
      calls: AtomicUsize::new(0),
      closed: AtomicUsize::new(0),
      waiting: Mutex::new(None),
      started: Notify::new(),
      query_waiting: Mutex::new(None),
      close_waiting: Mutex::new(None),
      close_started: Notify::new(),
      close_finished: AtomicUsize::new(0),
    })
  }
}
impl InteractionAdapter for FakeAdapter {
  fn descriptor(&self) -> RuntimeDescriptor {
    self.descriptor.clone()
  }
  fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
    self.events.subscribe()
  }
  fn close(&self) -> AdapterFuture<'_, Result<(), InteractionError>> {
    Box::pin(async {
      self.closed.fetch_add(1, Ordering::SeqCst);
      let wait = self.close_waiting.lock().unwrap().take();
      self.close_started.notify_one();
      if let Some(wait) = wait {
        let _ = wait.await;
      }
      self.close_finished.fetch_add(1, Ordering::SeqCst);
      Ok(())
    })
  }
  fn execute(&self, request: InteractionRequest) -> AdapterFuture<'_, Outcome> {
    Box::pin(async move {
      self.calls.fetch_add(1, Ordering::SeqCst);
      match request.command {
        Command::Metadata => Ok(Reply::Metadata(Metadata {
          name: Some("fake".into()),
          ..Default::default()
        })),
        Command::Message { .. } => {
          let wait = self.waiting.lock().unwrap().take();
          self.started.notify_one();
          if let Some(wait) = wait {
            let _ = wait.await;
          }
          Ok(Reply::Admission(InputReceipt {
            disposition: InputDisposition::Steered,
            model: None,
            model_ignored: false,
            native_id: None,
          }))
        }
        Command::State => Ok(Reply::State(SessionState {
          session_id: request.target.session_id,
          status: ExecutionState::Idle,
          model: None,
          pending_inputs: Some(0),
          revision: None,
        })),
        Command::PendingInteractions => {
          let wait = self.query_waiting.lock().unwrap().take();
          if let Some(wait) = wait {
            self.started.notify_one();
            let _ = wait.await;
          }
          Ok(Reply::Interactions(vec![question("first"), question("second")]))
        }
        _ => Ok(Reply::Acknowledged),
      }
    })
  }
}
fn request(id: &str, command: Command) -> InteractionRequest {
  InteractionRequest {
    request_id: id.into(),
    target: SessionTarget {
      runtime_id: "test".into(),
      instance_id: "instance".into(),
      session_id: Some("session".into()),
    },
    command,
  }
}
fn message(id: &str, text: &str) -> InteractionRequest {
  request(
    id,
    Command::Message {
      input: MessageInput {
        text: text.into(),
        ..Default::default()
      },
      delivery: DeliveryMode::StartOrSteer,
    },
  )
}
fn question(id: &str) -> PendingInteraction {
  PendingInteraction {
    interaction_id: id.into(),
    session_id: Some("session".into()),
    kind: InteractionKind::Question,
    title: "Pick".into(),
    questions: vec![QuestionItem {
      question_id: "q".into(),
      prompt: "Choose?".into(),
      options: vec![],
      allows_free_text: true,
      allows_option_notes: false,
      is_secret: false,
      multiple: false,
    }],
    native: serde_json::Value::Null,
  }
}

#[test]
fn selected_option_notes_require_the_explicit_native_capability() {
  let mut pending = question("notes");
  pending.questions[0].options.push(QuestionOption {
    label: "Chosen".into(),
    description: None,
  });
  let response = InteractionResponse::Answers {
    answers: vec![QuestionAnswer {
      question_id: "q".into(),
      answers: vec!["Chosen".into()],
      custom: Some("Additional detail".into()),
    }],
  };
  assert_eq!(
    validate_response(&pending, &response).unwrap_err().kind,
    ErrorKind::InvalidRequest
  );
  pending.questions[0].allows_option_notes = true;
  assert!(validate_response(&pending, &response).is_ok());
  pending.questions[0].allows_free_text = false;
  assert!(validate_response(&pending, &response).is_err());
  let legacy: QuestionItem = serde_json::from_value(serde_json::json!({
    "question_id":"q","prompt":"Choose?","options":[],"allows_free_text":true,"multiple":false
  }))
  .unwrap();
  assert!(!legacy.allows_option_notes);
  assert!(!legacy.is_secret);
}
fn answer(id: &str, interaction_id: &str) -> InteractionRequest {
  request(
    id,
    Command::Respond {
      interaction_id: interaction_id.into(),
      response: InteractionResponse::Answers {
        answers: vec![QuestionAnswer {
          question_id: "q".into(),
          answers: vec![],
          custom: Some("reply".into()),
        }],
      },
    },
  )
}

#[tokio::test]
async fn inspection_and_stale_targets_never_start_or_mutate_a_runtime() {
  let service = InteractionService::default();
  let adapter = FakeAdapter::new();
  service.register(adapter.clone()).unwrap();
  assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
  assert!(service.snapshot("test").unwrap().states.is_empty());
  let mut stale = request("query", Command::State);
  stale.target.instance_id = "old".into();
  assert_eq!(service.execute(stale).await.unwrap_err().kind, ErrorKind::StaleTarget);
  assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
  assert!(matches!(
    service.execute(request("metadata", Command::Metadata)).await.unwrap(),
    Reply::Metadata(_)
  ));
  let mut unsupported = message("model", "hello");
  if let Command::Message { input, .. } = &mut unsupported.command {
    input.model = Some(ModelIdentity {
      model_provider: None,
      model_id: "other".into(),
      display_name: None,
    });
  }
  assert_eq!(
    service.execute(unsupported).await.unwrap_err().kind,
    ErrorKind::Unsupported
  );
  assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
  service.detach("test", "instance").await.unwrap();
  assert_eq!(adapter.closed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn caller_cancellation_keeps_admission_and_identity_bound_receipts_alive() {
  let service = InteractionService::default();
  let adapter = FakeAdapter::new();
  service.register(adapter.clone()).unwrap();
  let (complete, wait) = oneshot::channel();
  *adapter.waiting.lock().unwrap() = Some(wait);
  let sending_service = service.clone();
  let sending = tokio::spawn(async move { sending_service.execute(message("send", "hello")).await });
  adapter.started.notified().await;
  sending.abort();
  assert!(matches!(
    service.execute(message("send", "hello")).await.unwrap(),
    Reply::Pending
  ));
  assert_eq!(
    service.execute(message("send", "different")).await.unwrap_err().kind,
    ErrorKind::Conflict
  );
  assert_eq!(
    service.execute(message("other", "hello")).await.unwrap_err().kind,
    ErrorKind::Conflict
  );
  assert!(matches!(
    service.execute(request("stop", Command::CancelTurn)).await.unwrap(),
    Reply::Acknowledged
  ));
  assert_eq!(
    service
      .execute(message("after-cancel", "second message"))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Conflict,
    "cancellation does not release the message admission gate"
  );
  complete.send(()).unwrap();
  tokio::time::timeout(std::time::Duration::from_secs(1), async {
    loop {
      if matches!(
        service.execute(message("send", "hello")).await.unwrap(),
        Reply::Admission(_)
      ) {
        break;
      }
      tokio::task::yield_now().await;
    }
  })
  .await
  .unwrap();
  assert_eq!(
    adapter.calls.load(Ordering::SeqCst),
    2,
    "one input and one cancellation"
  );
}

#[tokio::test]
async fn pending_interactions_are_runtime_authoritative_and_independent_of_message_gate() {
  let service = InteractionService::default();
  let adapter = FakeAdapter::new();
  service.register(adapter.clone()).unwrap();
  assert_eq!(
    service.execute(answer("unknown", "first")).await.unwrap_err().kind,
    ErrorKind::StaleTarget
  );
  service
    .execute(request("pending", Command::PendingInteractions))
    .await
    .unwrap();
  assert_eq!(service.snapshot("test").unwrap().pending_interactions.len(), 2);
  let (complete, wait) = oneshot::channel();
  *adapter.waiting.lock().unwrap() = Some(wait);
  let sending_service = service.clone();
  let sending = tokio::spawn(async move { sending_service.execute(message("send", "hello")).await });
  adapter.started.notified().await;
  assert!(matches!(
    service.execute(answer("reply", "first")).await.unwrap(),
    Reply::Acknowledged
  ));
  assert_eq!(service.snapshot("test").unwrap().pending_interactions.len(), 1);
  assert_eq!(
    service
      .execute(message("after-answer", "second message"))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Conflict,
    "an answer does not release the message admission gate"
  );
  assert!(matches!(
    service.execute(answer("reply", "first")).await.unwrap(),
    Reply::Acknowledged
  ));
  assert_eq!(
    service.execute(answer("new-id", "first")).await.unwrap_err().kind,
    ErrorKind::StaleTarget
  );
  complete.send(()).unwrap();
  sending.await.unwrap().unwrap();
}

#[test]
fn event_gaps_invalidate_pending_proof_and_wrong_instances_never_update_cache() {
  let adapter = FakeAdapter::new();
  let entry = RuntimeEntry {
    descriptor: adapter.descriptor(),
    closing: AtomicBool::new(false),
    adapter,
    cache: Mutex::new(Cache::default()),
    pump: Mutex::new(None),
  };
  let event = RuntimeEvent {
    runtime_id: "test".into(),
    instance_id: "instance".into(),
    session_id: Some("session".into()),
    sequence: 1,
    kind: RuntimeEventKind::InteractionRequested(question("q")),
  };
  assert!(apply_event(&entry, &event));
  assert_eq!(entry.cache.lock().unwrap().pending.len(), 1);
  let mut wrong = event.clone();
  wrong.instance_id = "other".into();
  wrong.sequence = 2;
  assert!(!apply_event(&entry, &wrong));
  assert_eq!(entry.cache.lock().unwrap().pending.len(), 1);
  let mut gap = event;
  gap.sequence = 3;
  gap.kind = RuntimeEventKind::Native(serde_json::json!({"unknown":true}));
  assert!(apply_event(&entry, &gap));
  let cache = entry.cache.lock().unwrap();
  assert!(cache.pending.is_empty());
  assert!(cache.recovery_required);
}

#[tokio::test]
async fn runtime_wide_pending_baseline_includes_sessions_and_requires_state_for_recovery() {
  let service = InteractionService::default();
  let adapter = FakeAdapter::new();
  service.register(adapter).unwrap();
  let mut query = request("all-pending", Command::PendingInteractions);
  query.target.session_id = None;
  service.execute(query).await.unwrap();
  let snapshot = service.snapshot("test").unwrap();
  assert_eq!(snapshot.pending_interactions.len(), 2);
  assert!(snapshot.recovery_required);
  service.execute(request("state", Command::State)).await.unwrap();
  assert!(!service.snapshot("test").unwrap().recovery_required);
  let mut unbound = message("unbound", "hello");
  unbound.target.session_id = None;
  assert_eq!(
    service.execute(unbound).await.unwrap_err().kind,
    ErrorKind::InvalidRequest
  );
  assert!(service.shutdown().await.is_empty());
  assert!(service.snapshots().is_empty());
  assert!(service.register(FakeAdapter::new()).is_err());
}

#[tokio::test]
async fn baseline_queries_cannot_resurrect_an_interaction_after_newer_events() {
  let service = InteractionService::default();
  let adapter = FakeAdapter::new();
  service.register(adapter.clone()).unwrap();
  let (complete, wait) = oneshot::channel();
  *adapter.query_waiting.lock().unwrap() = Some(wait);
  let querying = service.clone();
  let query = tokio::spawn(async move { querying.execute(request("pending", Command::PendingInteractions)).await });
  adapter.started.notified().await;
  adapter
    .events
    .send(RuntimeEvent {
      runtime_id: "test".into(),
      instance_id: "instance".into(),
      session_id: Some("session".into()),
      sequence: 1,
      kind: RuntimeEventKind::InteractionRequested(question("newer")),
    })
    .unwrap();
  tokio::time::timeout(std::time::Duration::from_secs(1), async {
    loop {
      if service.snapshot("test").unwrap().last_sequence == Some(1) {
        break;
      }
      tokio::task::yield_now().await;
    }
  })
  .await
  .unwrap();
  complete.send(()).unwrap();
  assert_eq!(query.await.unwrap().unwrap_err().kind, ErrorKind::Unavailable);
  let snapshot = service.snapshot("test").unwrap();
  assert_eq!(snapshot.pending_interactions.len(), 1);
  assert_eq!(snapshot.pending_interactions[0].interaction_id, "newer");
}

#[test]
fn reply_and_event_session_binding_are_checked_before_authorizing_responses() {
  let mut wrong = question("wrong");
  wrong.session_id = Some("other-session".into());
  assert!(
    validate_reply(
      &request("pending", Command::PendingInteractions),
      &Reply::Interactions(vec![wrong.clone()])
    )
    .is_err()
  );
  let adapter = FakeAdapter::new();
  let entry = RuntimeEntry {
    descriptor: adapter.descriptor(),
    closing: AtomicBool::new(false),
    adapter,
    cache: Mutex::new(Cache::default()),
    pump: Mutex::new(None),
  };
  let event = RuntimeEvent {
    runtime_id: "test".into(),
    instance_id: "instance".into(),
    session_id: Some("session".into()),
    sequence: 1,
    kind: RuntimeEventKind::InteractionRequested(wrong),
  };
  assert!(!apply_event(&entry, &event));
  assert!(entry.cache.lock().unwrap().pending.is_empty());
}

#[tokio::test]
async fn shutdown_survives_caller_cancellation_and_reuses_one_completion() {
  let service = InteractionService::default();
  let adapter = FakeAdapter::new();
  service.register(adapter.clone()).unwrap();
  let (complete, wait) = oneshot::channel();
  *adapter.close_waiting.lock().unwrap() = Some(wait);
  let first_service = service.clone();
  let first = tokio::spawn(async move { first_service.shutdown().await });
  adapter.close_started.notified().await;
  first.abort();
  assert!(first.await.unwrap_err().is_cancelled());

  let next_service = service.clone();
  let next = tokio::spawn(async move { next_service.shutdown().await });
  let concurrent_service = service.clone();
  let concurrent = tokio::spawn(async move { concurrent_service.shutdown().await });
  tokio::task::yield_now().await;
  assert!(!next.is_finished(), "later shutdown must await existing cleanup");
  assert!(!concurrent.is_finished());
  assert_eq!(adapter.closed.load(Ordering::SeqCst), 1);
  assert_eq!(adapter.close_finished.load(Ordering::SeqCst), 0);
  complete.send(()).expect("caller cancellation must not cancel close");
  assert!(
    tokio::time::timeout(std::time::Duration::from_secs(1), next)
      .await
      .unwrap()
      .unwrap()
      .is_empty()
  );
  assert!(concurrent.await.unwrap().is_empty());
  assert_eq!(adapter.close_finished.load(Ordering::SeqCst), 1);
  assert!(service.shutdown().await.is_empty());
  assert_eq!(adapter.closed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn recovery_scope_overflow_is_bounded_and_requires_fresh_attachment() {
  let service = InteractionService::default();
  let adapter = FakeAdapter::new();
  service.register(adapter.clone()).unwrap();
  let entry = service.0.runtimes.lock().unwrap().get("test").unwrap().clone();
  {
    let mut cache = entry.cache.lock().unwrap();
    for index in 0..MAX_SESSIONS {
      let mut item = question("pending");
      item.session_id = Some(format!("session-{index}"));
      cache
        .pending
        .insert((item.session_id.clone(), item.interaction_id.clone()), item);
      cache.invalidate();
    }
    assert_eq!(cache.required_scopes.len(), MAX_SESSIONS);
    assert!(!cache.reattach_required);
    let item = question("overflow");
    cache
      .pending
      .insert((item.session_id.clone(), item.interaction_id.clone()), item);
    cache.invalidate();
    assert_eq!(cache.required_scopes.len(), MAX_SESSIONS);
    assert!(cache.reattach_required);
    assert!(cache.pending.is_empty());
  }
  assert!(service.snapshot("test").unwrap().reattach_required);
  assert_eq!(
    service.execute(message("blocked", "hello")).await.unwrap_err().kind,
    ErrorKind::Unavailable
  );
  assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
  service
    .execute(request("pending", Command::PendingInteractions))
    .await
    .unwrap();
  let snapshot = service.snapshot("test").unwrap();
  assert!(snapshot.recovery_required);
  assert!(snapshot.reattach_required);
  assert!(
    snapshot.pending_interactions.is_empty(),
    "reads cannot restore truncated pending proof"
  );
}
