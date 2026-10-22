use super::*;
use tokio::io::{AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};

struct Peer {
  reader: framing::RpcReader<ReadHalf<DuplexStream>>,
  writer: WriteHalf<DuplexStream>,
}

impl Peer {
  async fn next(&mut self) -> Value {
    tokio::time::timeout(Duration::from_secs(3), self.reader.read_frame())
      .await
      .expect("adapter did not send the expected native message")
      .unwrap()
      .expect("adapter disconnected")
  }

  async fn send(&mut self, value: Value) {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    self.writer.write_all(&bytes).await.unwrap();
  }

  async fn answer(&mut self, request: &Value, result: Value) {
    self.send(json!({"id":request["id"],"result":result})).await;
  }
}

async fn attach() -> (CodexAdapter, Peer) {
  let (adapter, native) = tokio::io::duplex(1024 * 1024);
  let (reader, writer) = tokio::io::split(adapter);
  let (native_reader, native_writer) = tokio::io::split(native);
  let mut peer = Peer {
    reader: framing::RpcReader::new(native_reader),
    writer: native_writer,
  };
  let initialize =
    tokio::spawn(async move { CodexAdapter::attach(EndpointOptions::new("test-codex"), reader, writer).await });
  let request = peer.next().await;
  assert_eq!(request["method"], "initialize");
  assert_eq!(request["params"]["clientInfo"]["name"], "tokn_session");
  assert_eq!(request["params"]["capabilities"]["experimentalApi"], true);
  peer
    .answer(&request, json!({"userAgent":"test-codex/1.0","future":"preserved"}))
    .await;
  let initialized = peer.next().await;
  assert_eq!(initialized["method"], "initialized");
  assert!(initialized.get("id").is_none());
  let adapter = initialize.await.unwrap().unwrap();
  assert_eq!(adapter.descriptor().provider, Provider::Codex);
  assert_eq!(adapter.descriptor().ownership, Ownership::Attached);
  (adapter, peer)
}

async fn attach_bound() -> (CodexAdapter, Peer) {
  let (adapter, mut peer) = attach().await;
  let copy = adapter.clone();
  let resume = tokio::spawn(async move { copy.resume_thread("thread-one").await });
  let request = peer.next().await;
  assert_eq!(request["method"], "thread/resume");
  peer
    .answer(
      &request,
      json!({"thread":thread("thread-one","idle"),"model":"native-model","modelProvider":"local"}),
    )
    .await;
  assert_eq!(resume.await.unwrap().unwrap().session_id.as_deref(), Some("thread-one"));
  (adapter, peer)
}

fn request(adapter: &CodexAdapter, id: &str, session: Option<&str>, command: Command) -> InteractionRequest {
  let descriptor = adapter.descriptor();
  InteractionRequest {
    request_id: id.into(),
    target: SessionTarget {
      runtime_id: descriptor.runtime_id,
      instance_id: descriptor.instance_id,
      session_id: session.map(str::to_owned),
    },
    command,
  }
}

fn execute(adapter: &CodexAdapter, request: InteractionRequest) -> JoinHandle<Result<Reply, InteractionError>> {
  let adapter = adapter.clone();
  tokio::spawn(async move { adapter.execute(request).await })
}

fn thread(id: &str, status: &str) -> Value {
  json!({"id":id,"status":{"type":status},"model":"native-model","modelProvider":"local","name":"A native thread","turns":[]})
}

fn message(text: &str, delivery: DeliveryMode) -> Command {
  Command::Message {
    input: MessageInput {
      text: text.into(),
      ..Default::default()
    },
    delivery,
  }
}

async fn next_interaction(events: &mut broadcast::Receiver<RuntimeEvent>) -> PendingInteraction {
  loop {
    let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
      .await
      .unwrap()
      .unwrap();
    if let RuntimeEventKind::InteractionRequested(interaction) = event.kind {
      return interaction;
    }
  }
}

#[tokio::test]
async fn metadata_and_catalog_queries_never_resume_threads() {
  let (adapter, mut peer) = attach().await;
  let metadata = execute(
    &adapter,
    request(&adapter, "metadata", Some("thread-one"), Command::Metadata),
  );
  let read = peer.next().await;
  assert_eq!(read["method"], "thread/read");
  assert_eq!(read["params"], json!({"threadId":"thread-one","includeTurns":false}));
  peer
    .answer(&read, json!({"thread":thread("thread-one","notLoaded")}))
    .await;
  let Reply::Metadata(metadata) = metadata.await.unwrap().unwrap() else {
    panic!("missing metadata")
  };
  assert_eq!(metadata.name.as_deref(), Some("A native thread"));
  assert_eq!(metadata.version.as_deref(), Some("test-codex/1.0"));
  assert_eq!(metadata.model.unwrap().model_id, "native-model");

  let models = execute(&adapter, request(&adapter, "models", None, Command::Models));
  let page_one = peer.next().await;
  assert_eq!(page_one["method"], "model/list");
  peer
    .answer(
      &page_one,
      json!({"data":[
    {"id":"image-id","model":"image-model","displayName":"Image model","inputModalities":["text","image"]},
    {"id":"hidden","model":"hidden-model","displayName":"Hidden model","hidden":true}
  ],"nextCursor":"page-two"}),
    )
    .await;
  let page_two = peer.next().await;
  assert_eq!(page_two["method"], "model/list");
  assert_eq!(page_two["params"]["cursor"], "page-two");
  peer.answer(&page_two, json!({"data":[{"id":"unknown-id","model":"unknown-model","displayName":"Unknown modalities","inputModalities":null}],"nextCursor":null})).await;
  let Reply::Models(models) = models.await.unwrap().unwrap() else {
    panic!("missing catalog")
  };
  assert_eq!(models.len(), 2);
  assert!(models[0].supports_images);
  assert!(!models[1].supports_images);
  assert_eq!(models[0].model.model_id, "image-model");
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn stale_runtime_and_mismatched_native_thread_are_rejected() {
  let (adapter, mut peer) = attach().await;
  let mut stale = request(&adapter, "stale", Some("thread-one"), Command::State);
  stale.target.instance_id = "previous-instance".into();
  assert_eq!(adapter.execute(stale).await.unwrap_err().kind, ErrorKind::StaleTarget);
  let inspect = execute(&adapter, request(&adapter, "state", Some("thread-one"), Command::State));
  let read = peer.next().await;
  assert_eq!(read["method"], "thread/read");
  peer
    .answer(&read, json!({"thread":thread("different-thread","idle")}))
    .await;
  assert_eq!(inspect.await.unwrap().unwrap_err().kind, ErrorKind::StaleTarget);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn stale_read_reply_cannot_overwrite_a_preceding_live_turn_notification() {
  let (adapter, mut peer) = attach_bound().await;
  let inspect = execute(
    &adapter,
    request(&adapter, "state-ordering", Some("thread-one"), Command::State),
  );
  let read = peer.next().await;
  peer.send(json!({"method":"turn/started","params":{"threadId":"thread-one","turn":{"id":"new-turn","status":"inProgress","items":[]}}})).await;
  // The server's asynchronous read may have captured its snapshot before the
  // live event, even though the response follows that event on the wire.
  peer.answer(&read, json!({"thread":thread("thread-one","idle")})).await;
  let Reply::State(state) = inspect.await.unwrap().unwrap() else {
    panic!("missing state")
  };
  assert_eq!(state.status, ExecutionState::Running);
  assert_eq!(state.revision.as_deref(), Some("new-turn"));
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn stale_resume_response_preserves_a_newer_live_turn_and_binds_thread() {
  let (adapter, mut peer) = attach().await;
  let copy = adapter.clone();
  let resume = tokio::spawn(async move { copy.resume_thread("thread-one").await });
  let native = peer.next().await;
  peer
    .send(json!({"method":"thread/started","params":{"thread":thread("thread-one","idle")}}))
    .await;
  peer.send(json!({"method":"turn/started","params":{"threadId":"thread-one","turn":{"id":"new-turn","status":"inProgress","items":[]}}})).await;
  peer
    .answer(
      &native,
      json!({"thread":thread("thread-one","idle"),"model":"native-model","modelProvider":"local"}),
    )
    .await;
  assert_eq!(resume.await.unwrap().unwrap().session_id.as_deref(), Some("thread-one"));
  let snapshot = adapter.inner.snapshot.lock().unwrap();
  assert_eq!(snapshot.sessions["thread-one"].state.status, ExecutionState::Running);
  assert_eq!(
    snapshot.sessions["thread-one"].state.revision.as_deref(),
    Some("new-turn")
  );
  drop(snapshot);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn inspection_does_not_grant_control_and_unsupported_delivery_is_explicit() {
  let (adapter, mut peer) = attach().await;
  let inspect = execute(
    &adapter,
    request(&adapter, "inspect-only", Some("thread-one"), Command::State),
  );
  let read = peer.next().await;
  peer.answer(&read, json!({"thread":thread("thread-one","idle")})).await;
  inspect.await.unwrap().unwrap();
  let control = request(
    &adapter,
    "unbound-input",
    Some("thread-one"),
    message("hello", DeliveryMode::StartOrSteer),
  );
  assert_eq!(adapter.execute(control).await.unwrap_err().kind, ErrorKind::StaleTarget);
  adapter.close().await.unwrap();

  let (adapter, _peer) = attach_bound().await;
  for (id, delivery) in [("strict-start", DeliveryMode::Start), ("queue", DeliveryMode::Queue)] {
    let command = request(&adapter, id, Some("thread-one"), message("hello", delivery));
    assert_eq!(adapter.execute(command).await.unwrap_err().kind, ErrorKind::Unsupported);
  }
  let input = MessageInput {
    text: "with model".into(),
    model: Some(ModelIdentity {
      model_id: "another".into(),
      model_provider: None,
      display_name: None,
    }),
    ..Default::default()
  };
  let command = request(
    &adapter,
    "atomic-model",
    Some("thread-one"),
    Command::Message {
      input,
      delivery: DeliveryMode::StartOrSteer,
    },
  );
  assert_eq!(adapter.execute(command).await.unwrap_err().kind, ErrorKind::Unsupported);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn mutation_receipts_prevent_duplicate_native_input_and_bind_payload() {
  let (adapter, mut peer) = attach_bound().await;
  let input = request(
    &adapter,
    "same-input",
    Some("thread-one"),
    message("hello", DeliveryMode::StartOrSteer),
  );
  let delivery = execute(&adapter, input.clone());
  let native = peer.next().await;
  assert_eq!(native["method"], "turn/start");
  assert_eq!(
    native["params"]["input"],
    json!([{"type":"text","text":"hello","text_elements":[]}])
  );
  assert!(native["params"].get("model").is_none());
  assert!(matches!(adapter.execute(input.clone()).await.unwrap(), Reply::Pending));
  peer
    .answer(
      &native,
      json!({"turn":{"id":"turn-one","status":"inProgress","items":[]}}),
    )
    .await;
  assert!(
    matches!(delivery.await.unwrap().unwrap(), Reply::Admission(receipt) if receipt.native_id.as_deref() == Some("turn-one"))
  );
  assert!(matches!(
    adapter.execute(input.clone()).await.unwrap(),
    Reply::Admission(_)
  ));
  let mut changed = input;
  changed.command = message("changed", DeliveryMode::StartOrSteer);
  assert_eq!(adapter.execute(changed).await.unwrap_err().kind, ErrorKind::Conflict);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn cancelled_caller_does_not_cancel_admitted_delivery() {
  let (adapter, mut peer) = attach_bound().await;
  let input = request(
    &adapter,
    "cancelled-caller",
    Some("thread-one"),
    message("deliver once", DeliveryMode::StartOrSteer),
  );
  let delivery = execute(&adapter, input.clone());
  let native = peer.next().await;
  delivery.abort();
  let _ = delivery.await;
  peer
    .answer(
      &native,
      json!({"turn":{"id":"turn-one","status":"inProgress","items":[]}}),
    )
    .await;
  let outcome = tokio::time::timeout(Duration::from_secs(3), async {
    loop {
      match adapter.execute(input.clone()).await.unwrap() {
        Reply::Pending => tokio::task::yield_now().await,
        result => break result,
      }
    }
  })
  .await
  .unwrap();
  assert!(matches!(outcome, Reply::Admission(_)));
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn strict_steering_uses_native_turn_identity_and_revision_fence() {
  let (adapter, mut peer) = attach_bound().await;
  let steer = execute(
    &adapter,
    request(
      &adapter,
      "steer",
      Some("thread-one"),
      message("adjust", DeliveryMode::Steer),
    ),
  );
  let read = peer.next().await;
  assert_eq!(read["method"], "thread/read");
  assert_eq!(read["params"]["includeTurns"], false);
  let state = thread("thread-one", "active");
  peer.answer(&read, json!({"thread":state})).await;
  let turns = peer.next().await;
  assert_eq!(turns["method"], "thread/turns/list");
  assert_eq!(
    turns["params"],
    json!({"threadId":"thread-one","limit":1,"sortDirection":"desc","itemsView":"notLoaded"})
  );
  peer
    .answer(
      &turns,
      json!({"data":[{"id":"turn-one","status":"inProgress","items":[]}],"nextCursor":null}),
    )
    .await;
  let native = peer.next().await;
  assert_eq!(native["method"], "turn/steer");
  assert_eq!(native["params"]["expectedTurnId"], "turn-one");
  peer.answer(&native, json!({"turnId":"turn-one"})).await;
  assert!(
    matches!(steer.await.unwrap().unwrap(), Reply::Admission(receipt) if receipt.disposition == InputDisposition::Steered)
  );

  let input = MessageInput {
    text: "stale adjustment".into(),
    expected_revision: Some("old-turn".into()),
    ..Default::default()
  };
  let fenced = execute(
    &adapter,
    request(
      &adapter,
      "fenced",
      Some("thread-one"),
      Command::Message {
        input,
        delivery: DeliveryMode::Steer,
      },
    ),
  );
  assert_eq!(fenced.await.unwrap().unwrap_err().kind, ErrorKind::Conflict);
  adapter.close().await.unwrap();
}

fn question(id: Value, session: &str, is_blocking: bool) -> Value {
  json!({"id":id,"method":"item/tool/requestUserInput","params":{
    "threadId":session,"turnId":"turn-one","itemId":"item-one","isBlocking":is_blocking,
    "questions":[{"id":"choose","header":"Choice","question":"Pick one","isOther":true,
      "options":[{"label":"First","description":"first option"},{"label":"Second","description":"second option"}]}]
  }})
}

#[tokio::test]
async fn async_questions_are_scoped_validate_answers_and_preserve_native_ids() {
  let (adapter, mut peer) = attach_bound().await;
  let mut events = adapter.subscribe();
  peer.send(question(json!(17), "thread-one", false)).await;
  let pending = next_interaction(&mut events).await;
  assert_eq!(pending.native["params"]["isBlocking"], false);
  assert!(pending.questions[0].allows_free_text);
  let response = InteractionResponse::Answers {
    answers: vec![QuestionAnswer {
      question_id: "choose".into(),
      answers: vec!["First".into()],
      custom: None,
    }],
  };
  let wrong_scope = request(
    &adapter,
    "wrong-scope",
    Some("thread-two"),
    Command::Respond {
      interaction_id: pending.interaction_id.clone(),
      response: response.clone(),
    },
  );
  assert_eq!(
    adapter.execute(wrong_scope).await.unwrap_err().kind,
    ErrorKind::StaleTarget
  );
  let invalid = request(
    &adapter,
    "invalid-answer",
    Some("thread-one"),
    Command::Respond {
      interaction_id: pending.interaction_id.clone(),
      response: InteractionResponse::Answers {
        answers: vec![QuestionAnswer {
          question_id: "choose".into(),
          answers: vec!["invented".into()],
          custom: None,
        }],
      },
    },
  );
  assert_eq!(
    adapter.execute(invalid).await.unwrap_err().kind,
    ErrorKind::InvalidRequest
  );
  let reply = execute(
    &adapter,
    request(
      &adapter,
      "answer",
      Some("thread-one"),
      Command::Respond {
        interaction_id: pending.interaction_id,
        response,
      },
    ),
  );
  let native = peer.next().await;
  assert_eq!(
    native,
    json!({"id":17,"result":{"answers":{"choose":{"answers":["First"]}}}})
  );
  assert!(matches!(reply.await.unwrap().unwrap(), Reply::Acknowledged));
  let Reply::Interactions(remaining) = adapter
    .execute(request(
      &adapter,
      "remaining",
      Some("thread-one"),
      Command::PendingInteractions,
    ))
    .await
    .unwrap()
  else {
    panic!("missing pending list")
  };
  assert!(remaining.is_empty());
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn selected_option_and_free_text_notes_follow_native_answer_shape() {
  let (adapter, mut peer) = attach_bound().await;
  let mut events = adapter.subscribe();
  peer.send(question(json!("notes"), "thread-one", true)).await;
  let pending = next_interaction(&mut events).await;
  let reply = execute(
    &adapter,
    request(
      &adapter,
      "with-notes",
      Some("thread-one"),
      Command::Respond {
        interaction_id: pending.interaction_id,
        response: InteractionResponse::Answers {
          answers: vec![QuestionAnswer {
            question_id: "choose".into(),
            answers: vec!["First".into()],
            custom: Some("with a detail".into()),
          }],
        },
      },
    ),
  );
  assert_eq!(
    peer.next().await,
    json!({"id":"notes","result":{"answers":{"choose":{"answers":["First","user_note: with a detail"]}}}})
  );
  assert!(matches!(reply.await.unwrap().unwrap(), Reply::Acknowledged));
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn externally_resolved_questions_cannot_be_answered_again() {
  let (adapter, mut peer) = attach_bound().await;
  let mut events = adapter.subscribe();
  peer.send(question(json!("external"), "thread-one", false)).await;
  let pending = next_interaction(&mut events).await;
  peer
    .send(json!({"method":"serverRequest/resolved","params":{"threadId":"thread-two","requestId":"external"}}))
    .await;
  peer
    .send(json!({"method":"serverRequest/resolved","params":{"threadId":"thread-one","requestId":"external"}}))
    .await;
  loop {
    let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
      .await
      .unwrap()
      .unwrap();
    if matches!(event.kind, RuntimeEventKind::InteractionResolved { interaction_id } if interaction_id == pending.interaction_id)
    {
      break;
    }
  }
  let late = request(
    &adapter,
    "late-answer",
    Some("thread-one"),
    Command::Respond {
      interaction_id: pending.interaction_id,
      response: InteractionResponse::Reject,
    },
  );
  assert_eq!(adapter.execute(late).await.unwrap_err().kind, ErrorKind::StaleTarget);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn duplicate_server_requests_preserve_pending_identity_and_conflicting_reuse_disconnects() {
  let (adapter, mut peer) = attach_bound().await;
  let mut events = adapter.subscribe();
  let original = question(json!(42), "thread-one", false);
  peer.send(original.clone()).await;
  let first = next_interaction(&mut events).await;
  peer.send(original).await;
  peer.send(question(json!("42"), "thread-one", false)).await;
  let second = next_interaction(&mut events).await;
  assert_ne!(first.interaction_id, second.interaction_id);
  let Reply::Interactions(pending) = adapter
    .execute(request(
      &adapter,
      "pending-two",
      Some("thread-one"),
      Command::PendingInteractions,
    ))
    .await
    .unwrap()
  else {
    panic!("missing pending requests")
  };
  assert_eq!(pending.len(), 2);
  let mut conflicting = question(json!(42), "thread-one", false);
  conflicting["params"]["questions"][0]["question"] = json!("different question using the same request ID");
  peer.send(conflicting).await;
  loop {
    let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
      .await
      .unwrap()
      .unwrap();
    if let RuntimeEventKind::Disconnected { reason } = event.kind {
      assert!(reason.contains("reused"));
      break;
    }
  }
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn completion_of_an_older_turn_does_not_clear_current_running_state() {
  let (adapter, mut peer) = attach_bound().await;
  let mut events = adapter.subscribe();
  peer.send(json!({"method":"turn/started","params":{"threadId":"thread-one","turn":{"id":"new-turn","status":"inProgress"}}})).await;
  peer.send(json!({"method":"turn/completed","params":{"threadId":"thread-one","turn":{"id":"old-turn","status":"completed"}}})).await;
  loop {
    let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
      .await
      .unwrap()
      .unwrap();
    if matches!(event.kind, RuntimeEventKind::Native(native) if native["method"] == "turn/completed") {
      break;
    }
  }
  let snapshot = adapter.inner.snapshot.lock().unwrap();
  assert_eq!(snapshot.sessions["thread-one"].state.status, ExecutionState::Running);
  assert_eq!(
    snapshot.sessions["thread-one"].state.revision.as_deref(),
    Some("new-turn")
  );
  drop(snapshot);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn approval_responses_never_widen_native_decisions() {
  let (adapter, mut peer) = attach_bound().await;
  let mut events = adapter.subscribe();
  peer
    .send(
      json!({"id":"approve-command","method":"item/commandExecution/requestApproval","params":{
        "threadId":"thread-one","turnId":"turn-one","itemId":"item-one","command":"printf harmless",
        "availableDecisions":["decline",{"acceptWithExecpolicyAmendment":{"execpolicyAmendment":["printf"]}}]
      }}),
    )
    .await;
  let pending = next_interaction(&mut events).await;
  for (id, decision) in [
    ("not-offered", PermissionDecision::AllowOnce),
    ("permanent", PermissionDecision::AllowAlways),
  ] {
    let result = adapter
      .execute(request(
        &adapter,
        id,
        Some("thread-one"),
        Command::Respond {
          interaction_id: pending.interaction_id.clone(),
          response: InteractionResponse::Permission { decision },
        },
      ))
      .await;
    assert_eq!(result.unwrap_err().kind, ErrorKind::Unsupported);
  }
  let deny = execute(
    &adapter,
    request(
      &adapter,
      "deny",
      Some("thread-one"),
      Command::Respond {
        interaction_id: pending.interaction_id,
        response: InteractionResponse::Permission {
          decision: PermissionDecision::Deny,
        },
      },
    ),
  );
  assert_eq!(
    peer.next().await,
    json!({"id":"approve-command","result":{"decision":"decline"}})
  );
  assert!(matches!(deny.await.unwrap().unwrap(), Reply::Acknowledged));
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn unsupported_server_requests_receive_method_error_without_disconnect() {
  let (adapter, mut peer) = attach().await;
  peer
    .send(json!({"id":"future-request","method":"future/request","params":{"threadId":"thread-one"}}))
    .await;
  let error = peer.next().await;
  assert_eq!(error["id"], "future-request");
  assert_eq!(error["error"]["code"], -32601);
  assert!(!adapter.inner.transport.is_closed());
  adapter.close().await.unwrap();
}
