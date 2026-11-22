//! Scripted current-generation peer; legacy ApiProxy tests remain independent.
use super::*;
use axum::{
  Json, Router,
  body::Bytes,
  extract::{
    Path, Query, State,
    ws::{Message, WebSocket, WebSocketUpgrade},
  },
  http::{HeaderMap, StatusCode},
  response::{IntoResponse, Response},
  routing::{get, post},
};

struct Peer {
  calls: Mutex<Vec<Value>>,
  frames: Mutex<Vec<Value>>,
  events: broadcast::Sender<Value>,
  wrong_id: AtomicBool,
  claim_remaining_ms: AtomicU64,
}
struct Server {
  endpoint: String,
  peer: Arc<Peer>,
  task: JoinHandle<()>,
}
impl Drop for Server {
  fn drop(&mut self) {
    self.task.abort();
  }
}

async fn server() -> Server {
  let (events, _) = broadcast::channel(32);
  let peer = Arc::new(Peer {
    calls: Mutex::new(Vec::new()),
    frames: Mutex::new(Vec::new()),
    events,
    wrong_id: AtomicBool::new(false),
    claim_remaining_ms: AtomicU64::new(1000),
  });
  let router = Router::new()
    .route("/connect", get(connect))
    .route("/api/remote.mux", get(socket))
    .route("/api/{*method}", post(call))
    .with_state(peer.clone());
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let endpoint = format!("http://{}/?token=fixture-token", listener.local_addr().unwrap());
  let task = tokio::spawn(async move {
    axum::serve(listener, router).await.unwrap();
  });
  Server { endpoint, peer, task }
}

async fn connect(Query(query): Query<BTreeMap<String, String>>) -> Response {
  assert_eq!(query.get("token").map(String::as_str), Some("fixture-token"));
  assert_eq!(query.get("interactions").map(String::as_str), Some("answer"));
  (
    StatusCode::FOUND,
    [
      ("set-cookie", "dsh_api=fixture-cookie; HttpOnly; Path=/"),
      ("location", "/api/remote.mux"),
    ],
  )
    .into_response()
}

async fn socket(State(peer): State<Arc<Peer>>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
  assert_eq!(headers.get("cookie").unwrap(), "dsh_api=fixture-cookie");
  let events = peer.events.subscribe();
  upgrade.on_upgrade(move |socket| serve_socket(peer, socket, events))
}

async fn serve_socket(peer: Arc<Peer>, mut socket: WebSocket, mut events: broadcast::Receiver<Value>) {
  loop {
    tokio::select! {
      event = events.recv() => {
        let Ok(value) = event else { return };
        let frame = if value.get("streamId").is_some() { value } else { json!({"type":"item","streamId":"events","value":value}) };
        if socket.send(Message::Text(frame.to_string().into())).await.is_err() { return; }
      }
      next = socket.recv() => match next {
        Some(Ok(Message::Text(text))) => {
          let frame: Value = serde_json::from_str(&text).unwrap();
          peer.frames.lock().await.push(frame.clone());
          if frame["type"] != "open" { continue; }
          let value = match frame["endpoint"].as_str().unwrap() {
            "$events" => { assert_eq!(frame["payload"], json!({"args":{}})); json!({"type":"ready","clientId":"generation-1","host":{"home":"/fixture-home"}}) }
            "session/observeEvent" => { assert_eq!(frame["payload"]["args"]["request"]["address"]["kind"], "session"); json!({"type":"ready"}) }
            "session/observeState" => json!({"cursor":0,"active":true,"running":false,"queue":[],"pendingInput":[]}),
            "userQuestions/attachWait" => {
              assert_eq!(frame["payload"]["args"], json!({"agentId":"session","callId":"call-question"}));
              json!({"remainingMs":peer.claim_remaining_ms.load(Ordering::Acquire)})
            }
            method => panic!("unexpected stream {method}"),
          };
          if socket.send(Message::Text(json!({"type":"item","streamId":frame["streamId"],"value":value}).to_string().into())).await.is_err() { return; }
        }
        Some(Ok(Message::Ping(data))) => { if socket.send(Message::Pong(data)).await.is_err() { return; } }
        Some(Ok(Message::Pong(_))) => {},
        _ => return,
      }
    }
  }
}

async fn call(
  State(peer): State<Arc<Peer>>,
  Path(method): Path<String>,
  headers: HeaderMap,
  body: Bytes,
) -> Json<Value> {
  assert_eq!(headers.get("cookie").unwrap(), "dsh_api=fixture-cookie");
  let frame: Value = serde_json::from_slice(&body).unwrap();
  assert_eq!(frame["method"], method);
  peer.calls.lock().await.push(frame.clone());
  let value = match method.as_str() {
    "session/list" => {
      assert_eq!(frame["payload"], json!({"args":{"_request":{}}}));
      json!({"items":[{"sessionId":"session","running":false,"cwd":"/workspace"}]})
    }
    "session/create" => json!({"sessionId":frame["payload"]["args"]["request"]["sessionId"]}),
    "session/state" => json!({"cursor":0,"active":true,"running":false,"queue":[],"pendingInput":[]}),
    "session/modelCatalog" => {
      assert_eq!(frame["payload"], json!({"args":{}}));
      json!({"groups":[],"failures":[],"default":{"provider":"fixture","model":"model"},"routableProviders":[]})
    }
    "session/prompt" => {
      assert_eq!(frame["payload"]["args"]["request"]["requestId"], frame["rpcId"]);
      json!({"accepted":true})
    }
    "session/cancel" => json!({"accepted":true}),
    "$events/result" => {
      assert_eq!(frame["payload"]["args"]["clientId"], "generation-1");
      Value::Null
    }
    method => panic!("unexpected Gateway method {method}"),
  };
  Json(
    json!({"type":"server-response","rpcId":if peer.wrong_id.load(Ordering::Acquire) { json!("wrong") } else { frame["rpcId"].clone() },"result":{"ok":true,"value":value}}),
  )
}

async fn execute(adapter: &DshAdapter, command: Command) -> Result<Reply, InteractionError> {
  let descriptor = adapter.descriptor();
  adapter
    .execute(InteractionRequest {
      request_id: uuid::Uuid::new_v4().to_string(),
      target: SessionTarget {
        runtime_id: descriptor.runtime_id,
        instance_id: descriptor.instance_id,
        session_id: Some("session".into()),
      },
      command,
    })
    .await
}

#[tokio::test]
async fn current_gateway_authenticates_observes_and_never_replays_uncertain_input() {
  let server = server().await;
  let adapter = DshAdapter::attach(ApiProxyConfig::new(&server.endpoint)).await.unwrap();
  assert!(adapter.inner.transport.is_gateway());
  assert!(matches!(
    execute(&adapter, Command::Metadata).await.unwrap(),
    Reply::Metadata(_)
  ));
  assert!(matches!(
    execute(&adapter, Command::Models).await.unwrap(),
    Reply::Models(_)
  ));
  assert!(matches!(
    execute(&adapter, Command::State).await.unwrap(),
    Reply::State(SessionState {
      status: ExecutionState::Idle,
      pending_inputs: Some(0),
      ..
    })
  ));
  let send = || Command::Message {
    input: MessageInput {
      text: "hello".into(),
      ..Default::default()
    },
    delivery: DeliveryMode::Queue,
  };
  assert!(matches!(execute(&adapter, send()).await.unwrap(), Reply::Admission(_)));
  assert_eq!(
    server
      .peer
      .frames
      .lock()
      .await
      .iter()
      .filter(|frame| frame["type"] == "open")
      .count(),
    3
  );
  server.peer.wrong_id.store(true, Ordering::Release);
  assert_eq!(execute(&adapter, send()).await.unwrap_err().kind, ErrorKind::Uncertain);
  assert_eq!(
    server
      .peer
      .calls
      .lock()
      .await
      .iter()
      .filter(|call| call["method"] == "session/prompt")
      .count(),
    2
  );
  assert!(
    server
      .peer
      .calls
      .lock()
      .await
      .iter()
      .all(|call| !call["method"].as_str().unwrap().contains('.'))
  );
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn current_waterfalls_preserve_native_identity_and_one_shot_decisions() {
  let server = server().await;
  let adapter = DshAdapter::attach(ApiProxyConfig::new(&server.endpoint)).await.unwrap();
  let mut events = adapter.subscribe();
  execute(&adapter, Command::State).await.unwrap();
  let stream = server
    .peer
    .frames
    .lock()
    .await
    .iter()
    .find(|frame| frame["endpoint"] == "session/observeEvent")
    .unwrap()["streamId"]
    .clone();
  server.peer.events.send(json!({"type":"item","streamId":stream,"value":{"type":"event","event":{"type":"tool/call","seq":1,"data":{"turn":1,"step":1,"callId":"tool-1","name":"shell","arguments":"{\"command\":\"printf fixture\"}"}}}})).unwrap();
  server.peer.events.send(json!({"type":"waterfall","event":"approval/request","eventId":"event-1","agentId":"session","request":{"toolName":"shell","callId":"tool-1","reason":"review"}})).unwrap();
  let pending = tokio::time::timeout(Duration::from_secs(2), async {
    loop {
      if let RuntimeEventKind::InteractionRequested(pending) = events.recv().await.unwrap().kind {
        break pending;
      }
    }
  })
  .await
  .unwrap();
  assert_eq!(pending.interaction_id, "event-1");
  assert_eq!(pending.native["wire"]["value"]["eventId"], "event-1");
  assert_eq!(pending.native["permission_decisions"], json!(["allow_once", "deny"]));
  assert!(pending.native["details"].as_str().unwrap().contains("printf fixture"));
  execute(
    &adapter,
    Command::Respond {
      interaction_id: "event-1".into(),
      response: InteractionResponse::Permission {
        decision: PermissionDecision::AllowOnce,
      },
    },
  )
  .await
  .unwrap();
  let calls = server.peer.calls.lock().await;
  let response = calls.iter().find(|call| call["method"] == "$events/result").unwrap();
  assert_eq!(
    response["payload"]["args"],
    json!({"clientId":"generation-1","eventId":"event-1","outcome":{"kind":"result","value":"allowed-once"}})
  );
  drop(calls);
  assert!(
    matches!(execute(&adapter, Command::PendingInteractions).await.unwrap(), Reply::Interactions(pending) if pending.is_empty())
  );
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn timed_questions_require_native_claim_readiness_and_claim_end_retires_the_card() {
  let server = server().await;
  let adapter = DshAdapter::attach(ApiProxyConfig::new(&server.endpoint)).await.unwrap();
  let mut events = adapter.subscribe();
  server.peer.events.send(json!({"type":"waterfall","event":"user-questions/request","eventId":"question-1","agentId":"session","request":{"wait":{"timed":true,"callId":"call-question"},"questions":[{"id":"q","question":"Choose","options":[{"label":"A"}]}]}})).unwrap();
  tokio::time::timeout(Duration::from_secs(2), async {
    loop {
      if let RuntimeEventKind::InteractionRequested(pending) = events.recv().await.unwrap().kind {
        assert_eq!(pending.interaction_id, "question-1");
        assert_eq!(pending.questions[0].question_id, "q");
        break;
      }
    }
  })
  .await
  .unwrap();
  let frames = server.peer.frames.lock().await;
  let claim = frames
    .iter()
    .find(|frame| frame["endpoint"] == "userQuestions/attachWait")
    .unwrap()["streamId"]
    .clone();
  drop(frames);
  execute(
    &adapter,
    Command::Respond {
      interaction_id: "question-1".into(),
      response: InteractionResponse::Answers {
        answers: vec![QuestionAnswer {
          question_id: "q".into(),
          answers: vec!["A".into()],
          custom: None,
        }],
      },
    },
  )
  .await
  .unwrap();
  let calls = server.peer.calls.lock().await;
  assert_eq!(
    calls.iter().find(|call| call["method"] == "$events/result").unwrap()["payload"]["args"]["outcome"],
    json!({"kind":"result","value":{"answers":[{"id":"q","selected":["A"]}]}})
  );
  drop(calls);
  server.peer.events.send(json!({"type":"end","streamId":claim})).unwrap();
  // This business stream naturally ends when the answer settles; it must not
  // invalidate the physical Gateway generation or other Session controls.
  assert!(matches!(
    execute(&adapter, Command::CancelTurn).await.unwrap(),
    Reply::Acknowledged
  ));
  adapter.close().await.unwrap();
}

fn timed_question() -> Value {
  json!({"type":"waterfall","event":"user-questions/request","eventId":"question-1","agentId":"session","request":{"wait":{"timed":true,"callId":"call-question"},"questions":[{"id":"q","question":"Choose","options":[{"label":"A"}]}]}})
}

fn answer_question() -> Command {
  Command::Respond {
    interaction_id: "question-1".into(),
    response: InteractionResponse::Answers {
      answers: vec![QuestionAnswer {
        question_id: "q".into(),
        answers: vec!["A".into()],
        custom: None,
      }],
    },
  }
}

async fn wait_question(events: &mut broadcast::Receiver<RuntimeEvent>) {
  tokio::time::timeout(Duration::from_secs(2), async {
    loop {
      if let RuntimeEventKind::InteractionRequested(_) = events.recv().await.unwrap().kind {
        break;
      }
    }
  })
  .await
  .unwrap();
}

async fn wait_claim_cancel(peer: &Peer) -> Value {
  tokio::time::timeout(Duration::from_secs(2), async {
    loop {
      if let Some(frame) = peer
        .frames
        .lock()
        .await
        .iter()
        .find(|frame| frame["type"] == "cancel")
        .cloned()
      {
        return frame["streamId"].clone();
      }
      tokio::time::sleep(Duration::from_millis(2)).await;
    }
  })
  .await
  .unwrap()
}

#[tokio::test]
async fn timed_question_expiry_sends_native_timeout_once_and_retires_without_disconnect() {
  let server = server().await;
  server.peer.claim_remaining_ms.store(40, Ordering::Release);
  let adapter = DshAdapter::attach(ApiProxyConfig::new(&server.endpoint)).await.unwrap();
  let mut events = adapter.subscribe();
  server.peer.events.send(timed_question()).unwrap();
  wait_question(&mut events).await;
  let stream = wait_claim_cancel(&server.peer).await;
  let calls = server.peer.calls.lock().await;
  let responses: Vec<_> = calls.iter().filter(|call| call["method"] == "$events/result").collect();
  assert_eq!(responses.len(), 1);
  assert_eq!(
    responses[0]["payload"]["args"],
    json!({"clientId":"generation-1","eventId":"question-1","outcome":{"kind":"rejected","error":{"name":"UserQuestionError","message":"ask_user_question timed out before the user answered","code":"ASK_TIMED_OUT"}}})
  );
  drop(calls);
  assert_eq!(
    execute(&adapter, answer_question()).await.unwrap_err().kind,
    ErrorKind::Conflict
  );
  assert!(
    matches!(execute(&adapter, Command::PendingInteractions).await.unwrap(), Reply::Interactions(items) if items.is_empty())
  );
  // A queued item and final End after our cancellation belong to that business
  // stream and must not invalidate the physical Gateway generation.
  server
    .peer
    .events
    .send(json!({"type":"item","streamId":stream,"value":{"remainingMs":0}}))
    .unwrap();
  server
    .peer
    .events
    .send(json!({"type":"end","streamId":stream}))
    .unwrap();
  server
    .peer
    .events
    .send(json!({"type":"emit","event":"api-session/status","args":["session",false]}))
    .unwrap();
  tokio::time::timeout(Duration::from_secs(2), async {
    loop {
      match events.recv().await.unwrap().kind {
        RuntimeEventKind::State(_) => break,
        RuntimeEventKind::Disconnected { reason } => panic!("business stream invalidated Gateway: {reason}"),
        _ => {}
      }
    }
  })
  .await
  .unwrap();
  assert!(matches!(
    execute(&adapter, Command::CancelTurn).await.unwrap(),
    Reply::Acknowledged
  ));
  assert!(adapter.inner.state.lock().await.responding.is_empty());
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn already_expired_question_is_never_published_as_answerable() {
  let server = server().await;
  server.peer.claim_remaining_ms.store(0, Ordering::Release);
  let adapter = DshAdapter::attach(ApiProxyConfig::new(&server.endpoint)).await.unwrap();
  let mut events = adapter.subscribe();
  server.peer.events.send(timed_question()).unwrap();
  wait_claim_cancel(&server.peer).await;
  while let Ok(event) = events.try_recv() {
    assert!(!matches!(event.kind, RuntimeEventKind::InteractionRequested(_)));
  }
  assert!(
    matches!(execute(&adapter, Command::PendingInteractions).await.unwrap(), Reply::Interactions(items) if items.is_empty())
  );
  assert_eq!(
    server
      .peer
      .calls
      .lock()
      .await
      .iter()
      .filter(|call| call["method"] == "$events/result")
      .count(),
    1
  );
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn uncertain_answer_is_reserved_and_countdown_cannot_send_a_second_outcome() {
  let server = server().await;
  server.peer.claim_remaining_ms.store(500, Ordering::Release);
  let adapter = DshAdapter::attach(ApiProxyConfig::new(&server.endpoint)).await.unwrap();
  let mut events = adapter.subscribe();
  server.peer.events.send(timed_question()).unwrap();
  wait_question(&mut events).await;
  server.peer.wrong_id.store(true, Ordering::Release);
  assert_eq!(
    execute(&adapter, answer_question()).await.unwrap_err().kind,
    ErrorKind::Uncertain
  );
  assert_eq!(
    execute(&adapter, answer_question()).await.unwrap_err().kind,
    ErrorKind::Conflict
  );
  assert!(
    matches!(execute(&adapter, Command::PendingInteractions).await.unwrap(), Reply::Interactions(items) if items[0].native["response_pending"] == true)
  );
  server.peer.wrong_id.store(false, Ordering::Release);
  let stream = wait_claim_cancel(&server.peer).await;
  assert_eq!(
    server
      .peer
      .calls
      .lock()
      .await
      .iter()
      .filter(|call| call["method"] == "$events/result")
      .count(),
    1
  );
  server
    .peer
    .events
    .send(json!({"type":"end","streamId":stream}))
    .unwrap();
  // Our local Cancel only releases this UI's claim. Only the native waterfall
  // resolution can retire the uncertain response reservation.
  server
    .peer
    .events
    .send(json!({"type":"cancel","eventId":"question-1"}))
    .unwrap();
  tokio::time::timeout(Duration::from_secs(2), async {
    loop {
      if matches!(execute(&adapter, Command::PendingInteractions).await.unwrap(), Reply::Interactions(items) if items.is_empty()) { break; }
      tokio::time::sleep(Duration::from_millis(2)).await;
    }
  }).await.unwrap();
  assert!(adapter.inner.state.lock().await.responding.is_empty());
  adapter.close().await.unwrap();
}
