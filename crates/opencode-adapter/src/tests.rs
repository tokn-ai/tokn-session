use super::*;
use std::sync::Mutex as StdMutex;
use tokio::{
  io::{AsyncReadExt, AsyncWriteExt},
  net::TcpListener,
};

#[derive(Default)]
struct ServerState {
  current: AtomicBool,
  legacy: AtomicBool,
  corrupt_admission: AtomicBool,
  fail_mutation: AtomicBool,
  wrong_created_location: AtomicBool,
  wrong_created_id: AtomicBool,
  malformed_creation: AtomicBool,
  disconnect: AtomicBool,
  idle: AtomicBool,
  native_events: StdMutex<Vec<Value>>,
  active_gate: StdMutex<Option<Arc<ReadGate>>>,
  admissions: StdMutex<BTreeMap<String, Value>>,
  requests: StdMutex<Vec<(String, String, Value)>>,
}

#[derive(Default)]
struct ReadGate {
  captured: tokio::sync::Notify,
  release: tokio::sync::Notify,
}

struct Server {
  endpoint: String,
  state: Arc<ServerState>,
  task: JoinHandle<()>,
}
impl Drop for Server {
  fn drop(&mut self) {
    self.task.abort();
  }
}

fn location() -> Value {
  json!({"directory":"/test/project"})
}

fn document() -> Value {
  let mut document = json!({"paths":{}});
  for path in [
    "/api/event",
    "/api/session/active",
    "/api/model",
    "/api/agent",
    "/api/provider",
    "/api/command",
    "/api/session/{sessionID}/question",
    "/api/session/{sessionID}/permission",
  ] {
    document["paths"][path] = json!({"get":{}});
  }
  for path in [
    "/api/session/{sessionID}/prompt",
    "/api/session/{sessionID}/interrupt",
    "/api/session/{sessionID}/question/{requestID}/reply",
    "/api/session/{sessionID}/question/{requestID}/reject",
    "/api/session/{sessionID}/permission/{requestID}/reply",
  ] {
    document["paths"][path] = json!({"post":{}});
  }
  document
}

fn current_document() -> Value {
  let mut document = document();
  document["paths"]
    .as_object_mut()
    .unwrap()
    .retain(|path, _| !path.contains("question"));
  document["paths"]["/api/session/{sessionID}/form"] = json!({"get":{}});
  document["paths"]["/api/session/{sessionID}/form/{formID}/reply"] = json!({"post":{}});
  document["paths"]["/api/session/{sessionID}/form/{formID}"] = json!({"delete":{}});
  document["paths"]["/api/session/{sessionID}/inbox"] = json!({"get":{}});
  document
}

fn current_form() -> Value {
  json!({"id":"frm_1", "sessionID":"ses_1", "title":"Questions", "metadata":{"kind":"question"}, "fields":[{
    "key":"opaque-field", "type":"string", "title":"Choice", "description":"Pick one",
    "options":[{"value":"opaque-native-value","label":"One","description":"First"}],"custom":true
  }]})
}

impl Server {
  async fn start() -> Self {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let state = Arc::new(ServerState::default());
    let service_state = state.clone();
    let task = tokio::spawn(async move {
      loop {
        let (mut socket, _) = match listener.accept().await {
          Ok(connection) => connection,
          Err(_) => return,
        };
        let state = service_state.clone();
        tokio::spawn(async move {
          let mut bytes = Vec::new();
          let header_end = loop {
            let mut chunk = [0_u8; 4096];
            let count = socket.read(&mut chunk).await.unwrap_or(0);
            if count == 0 {
              return;
            }
            bytes.extend_from_slice(&chunk[..count]);
            if bytes.len() > transport::MAX_BODY + 8192 {
              return;
            }
            if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
              break offset + 4;
            }
          };
          let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
          let content_length: usize = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim().parse().unwrap())
            .unwrap_or(0);
          while bytes.len() < header_end + content_length {
            let mut chunk = [0_u8; 4096];
            let count = socket.read(&mut chunk).await.unwrap_or(0);
            if count == 0 {
              return;
            }
            bytes.extend_from_slice(&chunk[..count]);
          }
          let mut line = headers.lines().next().unwrap().split_whitespace();
          let method = line.next().unwrap();
          let target = line.next().unwrap();
          let url = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
          let path = url.path();
          let body: Value =
            serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap_or(Value::Null);
          state
            .requests
            .lock()
            .unwrap()
            .push((method.into(), target.into(), body.clone()));
          if path == "/api/event" {
            let native = if state.current.load(Ordering::Relaxed) {
              json!({"id":"evt_form","type":"form.created","data":{"form":current_form()},"location":location()})
            } else {
              json!({"id":"evt_future","type":"future.event","data":{"sessionID":"ses_1","extra":true},"location":location(),"extension":42})
            };
            if socket
              .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n")
              .await
              .is_err()
            {
              return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            let output = format!("data: {native}\n\n");
            let chunk = format!("{:x}\r\n{output}\r\n", output.len());
            if socket.write_all(chunk.as_bytes()).await.is_err() {
              return;
            }
            while !state.disconnect.load(Ordering::Relaxed) {
              tokio::time::sleep(Duration::from_millis(25)).await;
              let events = std::mem::take(&mut *state.native_events.lock().unwrap());
              for native in events {
                let output = format!("data: {native}\n\n");
                let chunk = format!("{:x}\r\n{output}\r\n", output.len());
                if socket.write_all(chunk.as_bytes()).await.is_err() {
                  return;
                }
              }
              if socket.write_all(b"3\r\n:\n\n\r\n").await.is_err() {
                return;
              }
            }
            return;
          }
          let (status, value) = response(&state, method, path, body);
          let active_gate = if path == "/api/session/active" {
            state.active_gate.lock().unwrap().take()
          } else {
            None
          };
          if let Some(gate) = active_gate {
            // Capture the native snapshot before delaying its HTTP response.
            gate.captured.notify_one();
            gate.release.notified().await;
          }
          let content_type = "application/json";
          let output = value.to_string();
          let head = format!(
            "HTTP/1.1 {status} Status\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            output.len()
          );
          let _ = socket.write_all(head.as_bytes()).await;
          let _ = socket.write_all(output.as_bytes()).await;
        });
      }
    });
    Self { endpoint, state, task }
  }

  async fn adapter(&self) -> OpenCodeAdapter {
    OpenCodeAdapter::connect(ConnectionOptions {
      runtime_id: "opencode-test".into(),
      endpoint: self.endpoint.clone(),
      directory: "/test/project".into(),
      workspace_id: None,
      credentials: None,
    })
    .await
    .unwrap()
  }

  fn mutations(&self) -> Vec<(String, String, Value)> {
    self
      .state
      .requests
      .lock()
      .unwrap()
      .iter()
      .filter(|(method, _, _)| method == "POST")
      .cloned()
      .collect()
  }

  fn active_reads(&self) -> usize {
    self
      .state
      .requests
      .lock()
      .unwrap()
      .iter()
      .filter(|(method, path, _)| method == "GET" && path.starts_with("/api/session/active?"))
      .count()
  }
}

fn response(state: &ServerState, method: &str, path: &str, body: Value) -> (u16, Value) {
  let envelope = |data| json!({"location":location(),"data":data});
  let session =
    || json!({"id":"ses_1","title":"Test","location":location(),"model":{"id":"model-1","providerID":"provider-1"}});
  let result = match (method, path) {
    ("GET", "/api/health") if state.current.load(Ordering::Relaxed) => return (404, json!({"message":"not found"})),
    ("GET", "/api/health") => json!({"healthy":true}),
    ("GET", "/api/info") => json!({"version":"2.3.0","pid":1,"urls":[],"paths":{"tmp":"/fixture/tmp"}}),
    ("GET", "/openapi.json") => current_document(),
    ("GET", "/global/health") => json!({"healthy":true,"version":"1.18.8"}),
    ("GET", "/doc") => document(),
    ("GET", "/api/model") => envelope(
      json!([{"id":"model-1","providerID":"provider-1","name":"Model One", "enabled":true,"capabilities":{"input":["text","image"],"output":["text"],"tools":true}}]),
    ),
    ("GET", "/api/agent") => envelope(json!([{"id":"build"}])),
    ("GET", "/api/provider") => envelope(json!([{"id":"provider-1"}])),
    ("GET", "/api/command") => envelope(json!([{"id":"review"}])),
    ("GET", "/api/session/ses_1") => json!({"data":session()}),
    ("GET", "/api/session/active") => {
      if state.idle.load(Ordering::Acquire) {
        json!({"data":{}})
      } else {
        json!({"data":{"ses_1":{"type":"running"}}})
      }
    }
    ("GET", "/session/ses_1/message") => {
      if state.legacy.load(Ordering::Relaxed) {
        json!([{"info":{"role":"user"},"parts":[]}])
      } else {
        json!([])
      }
    }
    ("GET", "/api/session/ses_1/question") => {
      json!({"data":[{"id":"que_1","sessionID":"ses_1","questions":[{"question":"Pick one", "header":"Choice", "options":[{"label":"One","description":"First"}]}]}]})
    }
    ("GET", "/api/session/ses_1/form") => json!({"data":[current_form()]}),
    ("GET", "/api/session/ses_1/permission") => {
      json!({"data":[{"id":"per_1","sessionID":"ses_1","action":"write","resources":["file.rs"]}]})
    }
    ("POST", "/api/session/ses_1/prompt") => {
      if state.fail_mutation.load(Ordering::Relaxed) {
        return (503, json!({"message":"internal"}));
      }
      if state.current.load(Ordering::Relaxed) {
        if state.corrupt_admission.load(Ordering::Relaxed) {
          return (200, json!({"data":{"id":"wrong"}}));
        }
        return (
          200,
          json!({"data":{"id":body["id"],"sessionID":"ses_1","type":"user","payload":{"text":body["text"]},"delivery":body["delivery"],"time":{"created":1}}}),
        );
      }
      let mut admissions = state.admissions.lock().unwrap();
      let id = body["id"].as_str().unwrap().to_owned();
      if let Some(previous) = admissions.get(&id) {
        if previous["prompt"] != body["prompt"] || previous["delivery"] != body["delivery"] {
          return (409, json!({"message":"conflict"}));
        }
      }
      let admission = admissions
        .entry(id.clone())
        .or_insert_with(
          || json!({"id":id,"sessionID":"ses_1","prompt":body["prompt"],"delivery":body["delivery"],"admittedSeq":1}),
        )
        .clone();
      if state.corrupt_admission.load(Ordering::Relaxed) {
        json!({"data":{"id":"different"}})
      } else {
        json!({"data":admission})
      }
    }
    (
      "POST",
      "/api/session/ses_1/interrupt"
      | "/api/session/ses_1/question/que_1/reply"
      | "/api/session/ses_1/question/que_1/reject"
      | "/api/session/ses_1/form/frm_1/reply"
      | "/api/session/ses_1/permission/per_1/reply",
    ) => return (204, Value::Null),
    ("DELETE", "/api/session/ses_1/form/frm_1") => return (204, Value::Null),
    ("POST", "/api/session") => {
      if state.fail_mutation.load(Ordering::Relaxed) {
        return (503, json!({"message":"internal"}));
      }
      if state.malformed_creation.load(Ordering::Relaxed) {
        return (200, json!({"data":{}}));
      }
      let mut created = session();
      if let Some(id) = body.get("id") {
        created["id"] = id.clone();
      }
      if state.wrong_created_id.load(Ordering::Relaxed) {
        created["id"] = json!("ses_unexpected");
      }
      if state.wrong_created_location.load(Ordering::Relaxed) {
        created["location"]["directory"] = json!("/another/project");
      }
      json!({"data":created})
    }
    _ => return (404, json!({"message":"not found"})),
  };
  (200, result)
}

#[tokio::test]
async fn creation_uses_preallocated_identity_and_native_location_without_prompting() {
  let server = Server::start().await;
  let adapter = server.adapter().await;
  let id = new_session_id();
  let created = adapter
    .create_session_with_id(
      &id,
      Some("build".into()),
      Some(ModelIdentity {
        model_id: "model-1".into(),
        model_provider: Some("provider-1".into()),
        display_name: None,
      }),
    )
    .await
    .unwrap();
  assert_eq!(created.target.session_id.as_deref(), Some(id.as_str()));
  assert_eq!(created.working_directory, PathBuf::from("/test/project"));
  assert_eq!(created.workspace_id, None);
  let writes = server.mutations();
  assert_eq!(writes.len(), 1);
  assert_eq!(
    writes[0].2,
    json!({"id":id,"location":{"directory":"/test/project"},"agent":"build","model":{"id":"model-1","providerID":"provider-1"}})
  );
  assert!(adapter.create_session_with_id("invalid", None, None).await.is_err());
  assert_eq!(server.mutations().len(), 1);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn uncertain_creation_is_never_retried_or_published_with_a_different_identity() {
  let server = Server::start().await;
  let adapter = server.adapter().await;
  for fault in [
    &server.state.wrong_created_id,
    &server.state.wrong_created_location,
    &server.state.malformed_creation,
    &server.state.fail_mutation,
  ] {
    fault.store(true, Ordering::Relaxed);
    let before = server.mutations().len();
    let error = adapter
      .create_session_with_id("ses_reserved", None, None)
      .await
      .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Uncertain);
    assert_eq!(server.mutations().len(), before + 1);
    fault.store(false, Ordering::Relaxed);
  }
  adapter.close().await.unwrap();
}

fn request(adapter: &OpenCodeAdapter, id: &str, command: Command) -> InteractionRequest {
  let descriptor = adapter.descriptor();
  InteractionRequest {
    request_id: id.into(),
    target: SessionTarget {
      runtime_id: descriptor.runtime_id,
      instance_id: descriptor.instance_id,
      session_id: Some("ses_1".into()),
    },
    command,
  }
}

#[tokio::test]
async fn native_activity_monitor_is_passive_preserves_models_and_stops_reading_when_idle() {
  for current in [false, true] {
    let server = Server::start().await;
    server.state.current.store(current, Ordering::Release);
    server.state.idle.store(true, Ordering::Release);
    let adapter = server.adapter().await;
    let mut events = adapter.subscribe();
    let Reply::State(initial) = adapter
      .execute(request(&adapter, "baseline", Command::State))
      .await
      .unwrap()
    else {
      panic!("missing state")
    };
    assert_eq!(initial.status, ExecutionState::Idle);
    server.state.idle.store(false, Ordering::Release);
    let mut started = json!({"id":"started","type":if current {"session.execution.started"} else {"session.next.step.started"},"data":{"sessionID":"ses_1"}});
    if !current {
      started["location"] = location();
    }
    server.state.native_events.lock().unwrap().push(started);
    let running = tokio::time::timeout(Duration::from_secs(2), async {
      loop {
        if let RuntimeEventKind::State(state) = events.recv().await.unwrap().kind {
          if state.status == ExecutionState::Running {
            return state;
          }
        }
      }
    })
    .await
    .unwrap();
    assert_eq!(running.model, initial.model);
    server.state.idle.store(true, Ordering::Release);
    let idle = tokio::time::timeout(Duration::from_secs(2), async {
      loop {
        if let RuntimeEventKind::State(state) = events.recv().await.unwrap().kind {
          if state.status == ExecutionState::Idle {
            return state;
          }
        }
      }
    })
    .await
    .unwrap();
    assert_eq!(idle.model, initial.model);
    let completed = server.active_reads();
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(server.active_reads(), completed, "idle sessions do not keep polling");
    assert!(
      server.mutations().is_empty(),
      "observing activity cannot activate sessions"
    );
    adapter.close().await.unwrap();
  }
}

#[tokio::test]
async fn pending_activity_read_never_blocks_permissions_or_overwrites_a_newer_execution() {
  let server = Server::start().await;
  server.state.current.store(true, Ordering::Release);
  server.state.idle.store(true, Ordering::Release);
  let adapter = server.adapter().await;
  let mut events = adapter.subscribe();
  adapter
    .execute(request(&adapter, "baseline", Command::State))
    .await
    .unwrap();
  let gate = Arc::new(ReadGate::default());
  *server.state.active_gate.lock().unwrap() = Some(gate.clone());
  let started = |id: &str| json!({"id":id,"type":"session.execution.started","data":{"sessionID":"ses_1"}});
  server.state.native_events.lock().unwrap().push(started("first"));
  tokio::time::timeout(Duration::from_secs(2), gate.captured.notified())
    .await
    .unwrap();
  // That pending read captured an empty native map. A new execution and its
  // permission request must remain observable while the old HTTP reply waits.
  server.state.idle.store(false, Ordering::Release);
  server.state.native_events.lock().unwrap().extend([
    started("newer"),
    json!({"id":"permission","type":"permission.asked","data":{"id":"per_delayed","sessionID":"ses_1","action":"shell","resources":["printf fixture"]},"location":location()}),
  ]);
  tokio::time::timeout(Duration::from_secs(2), async {
    loop {
      if let RuntimeEventKind::InteractionRequested(pending) = events.recv().await.unwrap().kind {
        if pending.interaction_id == "permission:per_delayed" {
          break;
        }
      }
    }
  })
  .await
  .unwrap();
  tokio::time::sleep(Duration::from_millis(300)).await;
  assert_eq!(server.active_reads(), 2, "at most one monitor request can be in flight");
  gate.release.notify_one();
  // Seeing a fresh subsequent read proves the delayed reply was processed.
  tokio::time::timeout(Duration::from_secs(2), async {
    while server.active_reads() < 3 {
      tokio::time::sleep(Duration::from_millis(5)).await;
    }
  })
  .await
  .unwrap();
  while let Ok(event) = events.try_recv() {
    assert!(
      !matches!(
        event.kind,
        RuntimeEventKind::State(SessionState {
          status: ExecutionState::Idle,
          ..
        })
      ),
      "an empty snapshot from before the newer execution cannot publish Idle"
    );
  }
  assert!(adapter.inner.live.has_running());
  assert!(server.mutations().is_empty());
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn current_dialect_uses_flat_inbox_admission_native_forms_and_decisions_without_legacy_calls() {
  let server = Server::start().await;
  server.state.current.store(true, Ordering::Relaxed);
  server.state.legacy.store(true, Ordering::Relaxed);
  let adapter = server.adapter().await;
  let mut events = adapter.subscribe();
  assert!(adapter.descriptor().capabilities.supports(Operation::Respond));
  assert!(matches!(
    adapter
      .execute(request(
        &adapter,
        "current-message",
        message("Modern prompt", DeliveryMode::StartOrSteer)
      ))
      .await
      .unwrap(),
    Reply::Admission(_)
  ));
  let reply = adapter
    .execute(request(&adapter, "current-pending", Command::PendingInteractions))
    .await
    .unwrap();
  let Reply::Interactions(pending) = reply else {
    panic!("pending interactions expected")
  };
  let form = pending.iter().find(|item| item.interaction_id == "form:frm_1").unwrap();
  assert_eq!(form.kind, InteractionKind::Question);
  adapter
    .execute(request(
      &adapter,
      "answer-form",
      Command::Respond {
        interaction_id: form.interaction_id.clone(),
        response: InteractionResponse::Answers {
          answers: vec![QuestionAnswer {
            question_id: form.questions[0].question_id.clone(),
            answers: vec!["One".into()],
            custom: None,
          }],
        },
      },
    ))
    .await
    .unwrap();
  adapter
    .execute(request(
      &adapter,
      "reject-form",
      Command::Respond {
        interaction_id: form.interaction_id.clone(),
        response: InteractionResponse::Reject,
      },
    ))
    .await
    .unwrap();
  adapter
    .execute(request(
      &adapter,
      "allow-current",
      Command::Respond {
        interaction_id: "permission:per_1".into(),
        response: InteractionResponse::Permission {
          decision: PermissionDecision::AllowOnce,
        },
      },
    ))
    .await
    .unwrap();
  tokio::time::timeout(Duration::from_secs(2), async {
    loop {
      if let RuntimeEvent {
        session_id,
        kind: RuntimeEventKind::InteractionRequested(interaction),
        ..
      } = events.recv().await.unwrap()
      {
        assert_eq!(session_id.as_deref(), Some("ses_1"));
        assert_eq!(interaction.interaction_id, "form:frm_1");
        break;
      }
    }
  })
  .await
  .unwrap();
  let requests = server.state.requests.lock().unwrap();
  assert!(
    !requests
      .iter()
      .any(|(_, path, _)| path.starts_with("/session/") || path.starts_with("/doc"))
  );
  assert!(requests.iter().any(|(method, path, body)| method == "POST"
    && path.starts_with("/api/session/ses_1/prompt")
    && body["text"] == "Modern prompt"
    && body.get("prompt").is_none()));
  assert!(requests.iter().any(|(method, path, body)| method == "POST"
    && path.starts_with("/api/session/ses_1/form/frm_1/reply")
    && *body == json!({"answer":{"opaque-field":"opaque-native-value"}})));
  assert!(
    requests
      .iter()
      .any(|(method, path, _)| method == "DELETE" && path.starts_with("/api/session/ses_1/form/frm_1"))
  );
  assert!(requests.iter().any(|(method, path, body)| method == "POST"
    && path.starts_with("/api/session/ses_1/permission/per_1/reply")
    && *body == json!({"decision":"once"})));
  drop(requests);
  server.state.corrupt_admission.store(true, Ordering::Relaxed);
  let before = server.mutations().len();
  let error = adapter
    .execute(request(
      &adapter,
      "uncertain-modern",
      message("Do not replay", DeliveryMode::Queue),
    ))
    .await
    .unwrap_err();
  assert_eq!(error.kind, ErrorKind::Uncertain);
  assert_eq!(server.mutations().len(), before + 1);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn current_dialect_rejects_an_explicit_workspace_fence_it_cannot_attest() {
  let server = Server::start().await;
  server.state.current.store(true, Ordering::Relaxed);
  let result = OpenCodeAdapter::connect(ConnectionOptions {
    runtime_id: "workspace-test".into(),
    endpoint: server.endpoint.clone(),
    directory: "/test/project".into(),
    workspace_id: Some("wrk_expected".into()),
    credentials: None,
  })
  .await;
  assert!(matches!(
    result,
    Err(InteractionError {
      kind: ErrorKind::Unsupported,
      ..
    })
  ));
  assert!(server.mutations().is_empty());
}

fn message(text: &str, delivery: DeliveryMode) -> Command {
  Command::Message {
    input: MessageInput {
      text: text.into(),
      ..MessageInput::default()
    },
    delivery,
  }
}

#[tokio::test]
async fn native_metadata_models_and_state_use_same_location() {
  let server = Server::start().await;
  let adapter = server.adapter().await;
  let Reply::Metadata(metadata) = adapter
    .execute(request(&adapter, "metadata", Command::Metadata))
    .await
    .unwrap()
  else {
    panic!()
  };
  assert_eq!(metadata.version.as_deref(), Some("1.18.8"));
  assert_eq!(metadata.agents, ["build"]);
  assert_eq!(metadata.providers, ["provider-1"]);
  assert_eq!(metadata.commands, ["review"]);
  let Reply::Models(models) = adapter
    .execute(request(&adapter, "models", Command::Models))
    .await
    .unwrap()
  else {
    panic!()
  };
  assert_eq!(models.len(), 1);
  assert!(models[0].supports_images);
  let Reply::State(state) = adapter
    .execute(request(&adapter, "state", Command::State))
    .await
    .unwrap()
  else {
    panic!()
  };
  assert_eq!(state.status, ExecutionState::Running);
  assert!(!adapter.descriptor().capabilities.atomic_model_on_send);
  assert!(!adapter.descriptor().capabilities.supports(Operation::Steer));
  assert!(
    server
      .state
      .requests
      .lock()
      .unwrap()
      .iter()
      .all(|(_, path, _)| path.contains("location%5Bdirectory%5D=%2Ftest%2Fproject"))
  );
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn admission_is_not_guessed_completion_and_request_identity_survives_retry() {
  let server = Server::start().await;
  let adapter = server.adapter().await;
  for _ in 0..2 {
    let Reply::Admission(receipt) = adapter
      .execute(request(
        &adapter,
        "same-request",
        message("hello", DeliveryMode::StartOrSteer),
      ))
      .await
      .unwrap()
    else {
      panic!()
    };
    assert_eq!(receipt.disposition, InputDisposition::Accepted);
    assert_eq!(receipt.native_id.as_deref(), Some("msg_tokn_same-request"));
  }
  assert_eq!(server.state.admissions.lock().unwrap().len(), 1);
  let error = adapter
    .execute(request(
      &adapter,
      "same-request",
      message("changed", DeliveryMode::StartOrSteer),
    ))
    .await
    .unwrap_err();
  assert_eq!(error.kind, ErrorKind::Conflict);
  let Reply::Admission(queued) = adapter
    .execute(request(&adapter, "queue", message("later", DeliveryMode::Queue)))
    .await
    .unwrap()
  else {
    panic!()
  };
  assert_eq!(queued.disposition, InputDisposition::Queued);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn rejects_legacy_history_strict_modes_and_nonatomic_model_intent_before_mutation() {
  let server = Server::start().await;
  let adapter = server.adapter().await;
  server.state.legacy.store(true, Ordering::Relaxed);
  let error = adapter
    .execute(request(
      &adapter,
      "legacy",
      message("hello", DeliveryMode::StartOrSteer),
    ))
    .await
    .unwrap_err();
  assert_eq!(error.kind, ErrorKind::Unsupported);
  let Reply::State(state) = adapter
    .execute(request(&adapter, "legacy-state", Command::State))
    .await
    .unwrap()
  else {
    panic!()
  };
  assert_eq!(state.status, ExecutionState::Unknown);
  server.state.legacy.store(false, Ordering::Relaxed);
  for mode in [
    DeliveryMode::Prompt,
    DeliveryMode::Start,
    DeliveryMode::Steer,
    DeliveryMode::Guide,
  ] {
    assert_eq!(
      adapter
        .execute(request(&adapter, "strict", message("hello", mode)))
        .await
        .unwrap_err()
        .kind,
      ErrorKind::Unsupported
    );
  }
  let input = MessageInput {
    text: "hello".into(),
    model: Some(ModelIdentity {
      model_provider: Some("provider-1".into()),
      model_id: "model-1".into(),
      display_name: None,
    }),
    ..MessageInput::default()
  };
  assert_eq!(
    adapter
      .execute(request(
        &adapter,
        "model",
        Command::Message {
          input,
          delivery: DeliveryMode::StartOrSteer
        }
      ))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Unsupported
  );
  assert!(server.mutations().is_empty());
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn questions_and_permissions_keep_distinct_native_reply_shapes() {
  let server = Server::start().await;
  let adapter = server.adapter().await;
  let Reply::Interactions(pending) = adapter
    .execute(request(&adapter, "pending", Command::PendingInteractions))
    .await
    .unwrap()
  else {
    panic!()
  };
  assert_eq!(pending.len(), 2);
  assert!(pending[0].questions[0].allows_free_text);
  adapter
    .execute(request(
      &adapter,
      "answer",
      Command::Respond {
        interaction_id: "question:que_1".into(),
        response: InteractionResponse::Answers {
          answers: vec![QuestionAnswer {
            question_id: "que_1:0".into(),
            answers: vec![],
            custom: Some("Custom answer".into()),
          }],
        },
      },
    ))
    .await
    .unwrap();
  adapter
    .execute(request(
      &adapter,
      "permission",
      Command::Respond {
        interaction_id: "permission:per_1".into(),
        response: InteractionResponse::Permission {
          decision: PermissionDecision::AllowOnce,
        },
      },
    ))
    .await
    .unwrap();
  let mutations = server.mutations();
  assert_eq!(mutations[0].2, json!({"answers":[["Custom answer"]]}));
  assert_eq!(mutations[1].2, json!({"reply":"once"}));
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn ambiguous_mutation_errors_do_not_become_definite_rejection() {
  let server = Server::start().await;
  let adapter = server.adapter().await;
  server.state.fail_mutation.store(true, Ordering::Relaxed);
  assert_eq!(
    adapter
      .execute(request(
        &adapter,
        "failure",
        message("hello", DeliveryMode::StartOrSteer)
      ))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Uncertain
  );
  server.state.fail_mutation.store(false, Ordering::Relaxed);
  server.state.corrupt_admission.store(true, Ordering::Relaxed);
  assert_eq!(
    adapter
      .execute(request(
        &adapter,
        "corrupt",
        message("hello", DeliveryMode::StartOrSteer)
      ))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Uncertain
  );
  assert_eq!(server.mutations().len(), 2);
  server.state.wrong_created_location.store(true, Ordering::Relaxed);
  assert_eq!(
    adapter.create_session(None, None).await.unwrap_err().kind,
    ErrorKind::Uncertain
  );
  assert_eq!(server.mutations().len(), 3);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn unknown_native_events_preserve_fields_and_attached_close_only_detaches() {
  let server = Server::start().await;
  let adapter = server.adapter().await;
  assert_eq!(adapter.descriptor().ownership, Ownership::Attached);
  let mut events = adapter.subscribe();
  let event = tokio::time::timeout(Duration::from_secs(3), async {
    loop {
      let event = events.recv().await.unwrap();
      if matches!(&event.kind, RuntimeEventKind::Native(value) if value["type"] == "future.event") {
        break event;
      }
    }
  })
  .await
  .unwrap();
  let RuntimeEventKind::Native(native) = event.kind else {
    panic!()
  };
  assert_eq!(native["extension"], 42);
  adapter.close().await.unwrap();
  assert!(
    reqwest::get(format!("{}api/health", server.endpoint))
      .await
      .unwrap()
      .status()
      .is_success()
  );
  assert!(server.mutations().is_empty());
  assert_eq!(
    adapter
      .execute(request(&adapter, "closed", Command::State))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Unavailable
  );
}

#[tokio::test]
async fn attached_disconnect_invalidates_instance_before_future_mutations() {
  let server = Server::start().await;
  let adapter = server.adapter().await;
  let mut events = adapter.subscribe();
  server.state.disconnect.store(true, Ordering::Relaxed);
  tokio::time::timeout(Duration::from_secs(3), async {
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
      .execute(request(&adapter, "lost", message("hello", DeliveryMode::StartOrSteer)))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::StaleTarget
  );
  assert!(server.mutations().is_empty());
  adapter.close().await.unwrap();
}

#[test]
fn rejects_legacy_only_catalog_and_credential_bearing_urls() {
  assert!(
    capabilities(
      &json!({"paths":{"/session/{sessionID}/prompt_async":{"post":{}}}}),
      api::Dialect::LegacyV2
    )
    .is_err()
  );
  assert!(Transport::new("http://user:secret@localhost:4096/", "/test/project".into(), None, None).is_err());
}
