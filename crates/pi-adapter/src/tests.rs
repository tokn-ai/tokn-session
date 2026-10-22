use super::*;
use tokio::io::{AsyncWriteExt, DuplexStream, WriteHalf};

struct Fixture {
  adapter: PiAdapter,
  server: tokio::task::JoinHandle<()>,
  output: Arc<AsyncMutex<WriteHalf<DuplexStream>>>,
  state: Arc<Mutex<Value>>,
  requests: Arc<Mutex<Vec<Value>>>,
}

impl Fixture {
  fn new() -> Self {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (reader, writer) = tokio::io::split(client);
    let mut options = EndpointOptions::new("pi-test");
    options.instance_id = "instance-1".into();
    options.request_timeout = Duration::from_secs(1);
    options.prompt_timeout = Duration::from_secs(1);
    let adapter = PiAdapter::attach(options, reader, writer).unwrap();
    let state = Arc::new(Mutex::new(native_state("pi-session", false)));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (reader, writer) = tokio::io::split(server);
    let output = Arc::new(AsyncMutex::new(writer));
    let server_output = output.clone();
    let server_state = state.clone();
    let server_requests = requests.clone();
    let server = tokio::spawn(async move {
      let mut reader = framing::RpcReader::new(reader);
      while let Ok(Some(request)) = reader.read_frame().await {
        server_requests.lock().unwrap().push(request.clone());
        if request["type"] == "extension_ui_response" {
          continue;
        }
        let data = match request["type"].as_str().unwrap() {
          "get_state" => server_state.lock().unwrap().clone(),
          "get_commands" => {
            json!({"commands":[{"name":"inspect","source":"extension","sourceInfo":{"path":"private-path"}}]})
          }
          "get_available_models" => {
            json!({"models":[{"provider":"test","id":"model-1","name":"Test model","input":["text","image"],"baseUrl":"private-endpoint","headers":{"authorization":"secret"}}]})
          }
          _ => Value::Null,
        };
        let response =
          json!({"type":"response","id":request["id"],"command":request["type"],"success":true,"data":data});
        if request["type"] == "get_state"
          && server_state
            .lock()
            .unwrap()
            .get("emit_running_after_state")
            .and_then(Value::as_bool)
            == Some(true)
        {
          // One write puts both frames in the same reader buffer, so the native
          // event is consumed before the waiting query task is polled again.
          let mut bytes = serde_json::to_vec(&response).unwrap();
          bytes.push(b'\n');
          bytes.extend_from_slice(b"{\"type\":\"agent_start\"}\n");
          server_output.lock().await.write_all(&bytes).await.unwrap();
        } else {
          write_frame(&mut *server_output.lock().await, response).await;
        }
      }
    });
    Self {
      adapter,
      server,
      output,
      state,
      requests,
    }
  }

  fn target(&self) -> SessionTarget {
    SessionTarget {
      runtime_id: "pi-test".into(),
      instance_id: "instance-1".into(),
      session_id: Some("pi-session".into()),
    }
  }

  async fn execute(&self, id: &str, command: Command) -> Result<Reply, InteractionError> {
    self
      .adapter
      .execute(InteractionRequest {
        request_id: id.into(),
        target: self.target(),
        command,
      })
      .await
  }

  async fn emit(&self, value: Value) {
    write_frame(&mut *self.output.lock().await, value).await;
  }
}

impl Drop for Fixture {
  fn drop(&mut self) {
    self.server.abort();
  }
}

fn native_state(id: &str, busy: bool) -> Value {
  json!({"sessionId":id,"sessionFile":"/tmp/pi-test.jsonl","sessionName":"Pi demo","thinkingLevel":"medium",
    "isStreaming":busy,"isCompacting":false,"pendingMessageCount":0,
    "model":{"provider":"test","id":"model-1","name":"Test model","input":["text","image"]}})
}

async fn write_frame(writer: &mut (impl AsyncWrite + Unpin), value: Value) {
  let mut bytes = serde_json::to_vec(&value).unwrap();
  bytes.push(b'\n');
  writer.write_all(&bytes).await.unwrap();
}

fn input(text: &str) -> MessageInput {
  MessageInput {
    text: text.into(),
    ..MessageInput::default()
  }
}

#[tokio::test]
async fn concurrent_publishers_deliver_sequences_in_broadcast_order() {
  let fixture = Fixture::new();
  let mut events = fixture.adapter.subscribe();
  let barrier = std::sync::Barrier::new(8);
  // Exactly one broadcast buffer's worth of events avoids relying on receiver
  // scheduling and keeps this a publication-order test rather than a lag test.
  std::thread::scope(|scope| {
    for publisher in 0..8 {
      let inner = &fixture.adapter.inner;
      let barrier = &barrier;
      scope.spawn(move || {
        barrier.wait();
        for item in 0..32 {
          inner.publish(RuntimeEventKind::Native(json!({"publisher":publisher,"item":item})));
          std::thread::yield_now();
        }
      });
    }
  });
  for sequence in 1..=256 {
    assert_eq!(events.try_recv().unwrap().sequence, sequence);
  }
  assert!(matches!(events.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
}

async fn interaction(receiver: &mut broadcast::Receiver<RuntimeEvent>) -> PendingInteraction {
  tokio::time::timeout(Duration::from_secs(1), async {
    loop {
      if let RuntimeEventKind::InteractionRequested(interaction) = receiver.recv().await.unwrap().kind {
        return interaction;
      }
    }
  })
  .await
  .unwrap()
}

#[tokio::test]
async fn reads_scoped_metadata_models_and_preserves_native_stream_events() {
  let fixture = Fixture::new();
  let Reply::Metadata(metadata) = fixture.execute("metadata", Command::Metadata).await.unwrap() else {
    panic!("metadata expected")
  };
  assert_eq!(metadata.name.as_deref(), Some("Pi demo"));
  assert_eq!(metadata.commands, ["inspect"]);
  assert_eq!(metadata.model.unwrap().model_id, "model-1");
  let Reply::Models(models) = fixture.execute("models", Command::Models).await.unwrap() else {
    panic!("models expected")
  };
  assert_eq!(models.len(), 1);
  assert!(models[0].supports_images);
  let encoded = serde_json::to_string(&models).unwrap();
  assert!(!encoded.contains("private-endpoint"));
  assert!(!encoded.contains("secret"));
  let mut events = fixture.adapter.subscribe();
  let native = json!({"type":"message_update","message":{"role":"assistant"},"assistantMessageEvent":{"type":"text_delta","delta":"Hello"}});
  fixture.emit(native.clone()).await;
  let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
    .await
    .unwrap()
    .unwrap();
  assert_eq!(event.session_id.as_deref(), Some("pi-session"));
  assert!(matches!(event.kind, RuntimeEventKind::Native(value) if value == native));
}

#[tokio::test]
async fn rejects_stale_native_session_and_instance_without_delivering_input() {
  let fixture = Fixture::new();
  let mut target = fixture.target();
  target.instance_id = "old-instance".into();
  let error = fixture
    .adapter
    .execute(InteractionRequest {
      request_id: "stale".into(),
      target,
      command: Command::State,
    })
    .await
    .unwrap_err();
  assert_eq!(error.kind, ErrorKind::StaleTarget);
  assert!(fixture.requests.lock().unwrap().is_empty());
  *fixture.state.lock().unwrap() = native_state("other-session", false);
  let error = fixture
    .execute(
      "wrong-session",
      Command::Message {
        input: input("Do work"),
        delivery: DeliveryMode::StartOrSteer,
      },
    )
    .await
    .unwrap_err();
  assert_eq!(error.kind, ErrorKind::StaleTarget);
  assert_eq!(
    fixture
      .requests
      .lock()
      .unwrap()
      .iter()
      .filter(|request| request["type"] == "prompt")
      .count(),
    0
  );
}

#[tokio::test]
async fn native_auto_admission_deduplicates_and_never_changes_the_model() {
  let fixture = Fixture::new();
  *fixture.state.lock().unwrap() = native_state("pi-session", true);
  let command = Command::Message {
    input: input("Steer this work"),
    delivery: DeliveryMode::StartOrSteer,
  };
  assert!(matches!(
    fixture.execute("send-once", command.clone()).await.unwrap(),
    Reply::Admission(InputReceipt {
      disposition: InputDisposition::Accepted,
      ..
    })
  ));
  fixture.execute("send-once", command).await.unwrap();
  let requests = fixture.requests.lock().unwrap().clone();
  let prompts = requests
    .iter()
    .filter(|request| request["type"] == "prompt")
    .collect::<Vec<_>>();
  assert_eq!(prompts.len(), 1);
  assert_eq!(prompts[0]["streamingBehavior"], "steer");
  let error = fixture
    .execute(
      "send-once",
      Command::Message {
        input: input("Different"),
        delivery: DeliveryMode::StartOrSteer,
      },
    )
    .await
    .unwrap_err();
  assert_eq!(error.kind, ErrorKind::Conflict);
  let mut selected = input("Wrong model");
  selected.model = Some(ModelIdentity {
    model_provider: Some("test".into()),
    model_id: "model-2".into(),
    display_name: None,
  });
  assert_eq!(
    fixture
      .execute(
        "intent",
        Command::Message {
          input: selected,
          delivery: DeliveryMode::StartOrSteer
        }
      )
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Unsupported
  );
  assert!(!fixture.adapter.descriptor().capabilities.atomic_model_on_send);
  assert!(
    !fixture
      .requests
      .lock()
      .unwrap()
      .iter()
      .any(|request| request["type"] == "set_model")
  );
  assert_eq!(
    fixture
      .execute(
        "strict-steer",
        Command::Message {
          input: input("Only while active"),
          delivery: DeliveryMode::Steer
        }
      )
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Unsupported
  );
}

#[tokio::test]
async fn keeps_multiple_dialogs_and_replies_to_exact_pending_native_ids() {
  let fixture = Fixture::new();
  fixture.execute("bind", Command::State).await.unwrap();
  let mut events = fixture.adapter.subscribe();
  fixture.emit(json!({"type":"extension_ui_request","id":"confirm-1","method":"confirm","title":"Proceed?","message":"Allow work?"})).await;
  assert_eq!(interaction(&mut events).await.interaction_id, "confirm-1");
  fixture
    .emit(json!({"type":"extension_ui_request","id":"select-2","method":"select","title":"Choose","options":["A","B"]}))
    .await;
  assert_eq!(interaction(&mut events).await.interaction_id, "select-2");
  let Reply::Interactions(pending) = fixture.execute("pending", Command::PendingInteractions).await.unwrap() else {
    panic!("interactions expected")
  };
  assert_eq!(pending.len(), 2);
  let error = fixture
    .execute(
      "bad-answer",
      Command::Respond {
        interaction_id: "select-2".into(),
        response: InteractionResponse::Dialog {
          value: Some("not listed".into()),
          confirmed: None,
          cancelled: false,
        },
      },
    )
    .await
    .unwrap_err();
  assert_eq!(error.kind, ErrorKind::InvalidRequest);
  fixture
    .execute(
      "confirm-answer",
      Command::Respond {
        interaction_id: "confirm-1".into(),
        response: InteractionResponse::Dialog {
          value: None,
          confirmed: Some(true),
          cancelled: false,
        },
      },
    )
    .await
    .unwrap();
  fixture
    .execute(
      "select-answer",
      Command::Respond {
        interaction_id: "select-2".into(),
        response: InteractionResponse::Answers {
          answers: vec![QuestionAnswer {
            question_id: "select-2".into(),
            answers: vec!["B".into()],
            custom: None,
          }],
        },
      },
    )
    .await
    .unwrap();
  // A subsequent RPC response proves that preceding one-way replies reached
  // the synthetic native peer before inspecting its recorded writes.
  fixture.execute("after", Command::State).await.unwrap();
  let requests = fixture.requests.lock().unwrap().clone();
  let replies = requests
    .iter()
    .filter(|request| request["type"] == "extension_ui_response")
    .collect::<Vec<_>>();
  assert_eq!(replies.len(), 2);
  assert_eq!(replies[0]["id"], "confirm-1");
  assert_eq!(replies[0]["confirmed"], true);
  assert_eq!(replies[1]["id"], "select-2");
  assert_eq!(replies[1]["value"], "B");
}

#[tokio::test]
async fn expires_dialogs_and_does_not_resurrect_them_after_session_switch() {
  let fixture = Fixture::new();
  fixture.execute("bind", Command::State).await.unwrap();
  let mut events = fixture.adapter.subscribe();
  fixture
    .emit(json!({"type":"extension_ui_request","id":"expiring","method":"input","title":"Value","timeout":20}))
    .await;
  interaction(&mut events).await;
  tokio::time::timeout(Duration::from_secs(1), async {
    loop { if matches!(events.recv().await.unwrap().kind, RuntimeEventKind::InteractionResolved { interaction_id } if interaction_id == "expiring") { break; } }
  }).await.unwrap();
  let Reply::Interactions(pending) = fixture.execute("empty", Command::PendingInteractions).await.unwrap() else {
    panic!("interactions expected")
  };
  assert!(pending.is_empty());
  fixture
    .emit(json!({"type":"extension_ui_request","id":"old-session-dialog","method":"input","title":"Value"}))
    .await;
  interaction(&mut events).await;
  *fixture.state.lock().unwrap() = native_state("new-session", false);
  assert_eq!(
    fixture.execute("switched", Command::State).await.unwrap_err().kind,
    ErrorKind::StaleTarget
  );
  let snapshot = fixture.adapter.inner.snapshot.lock().unwrap();
  assert!(snapshot.interactions.is_empty());
}

#[tokio::test]
async fn lost_acknowledgement_remains_uncertain_and_is_not_replayed() {
  let (client, server) = tokio::io::duplex(8192);
  let (reader, writer) = tokio::io::split(client);
  let adapter = PiAdapter::attach(EndpointOptions::new("loss-test"), reader, writer).unwrap();
  let descriptor = adapter.descriptor();
  let server = tokio::spawn(async move {
    let (reader, mut writer) = tokio::io::split(server);
    let mut reader = framing::RpcReader::new(reader);
    let state = reader.read_frame().await.unwrap().unwrap();
    write_frame(&mut writer, json!({"type":"response","id":state["id"],"command":"get_state","success":true,"data":native_state("pi-session",false)})).await;
    let prompt = reader.read_frame().await.unwrap().unwrap();
    assert_eq!(prompt["type"], "prompt");
    // A real peer can persist/accept a prompt and disappear before acknowledging.
  });
  let request = InteractionRequest {
    request_id: "lost".into(),
    target: SessionTarget {
      runtime_id: descriptor.runtime_id,
      instance_id: descriptor.instance_id,
      session_id: Some("pi-session".into()),
    },
    command: Command::Message {
      input: input("One attempt"),
      delivery: DeliveryMode::StartOrSteer,
    },
  };
  assert_eq!(
    adapter.execute(request.clone()).await.unwrap_err().kind,
    ErrorKind::Uncertain
  );
  assert_eq!(adapter.execute(request).await.unwrap_err().kind, ErrorKind::Uncertain);
  server.await.unwrap();
}

#[tokio::test]
async fn attached_close_only_detaches_without_sending_abort() {
  let fixture = Fixture::new();
  fixture.execute("bind", Command::State).await.unwrap();
  assert_eq!(fixture.adapter.descriptor().ownership, Ownership::Attached);
  fixture.adapter.close().await.unwrap();
  assert!(
    !fixture
      .requests
      .lock()
      .unwrap()
      .iter()
      .any(|request| request["type"] == "abort")
  );
}

#[tokio::test]
async fn partial_write_invalidates_the_jsonl_transport_before_another_command() {
  use std::{
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll},
  };

  struct PartialWriter(Arc<AtomicUsize>);
  impl AsyncWrite for PartialWriter {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, bytes: &[u8]) -> Poll<std::io::Result<usize>> {
      if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
        Poll::Ready(Ok(bytes.len().min(4)))
      } else {
        Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)))
      }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
      Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
      Poll::Ready(Ok(()))
    }
  }

  for mutation in [false, true] {
    let (reader, _peer) = tokio::io::duplex(128);
    let writes = Arc::new(AtomicUsize::new(0));
    let adapter = PiAdapter::attach(
      EndpointOptions::new("partial-write"),
      reader,
      PartialWriter(writes.clone()),
    )
    .unwrap();
    let mut events = adapter.subscribe();
    let error = adapter
      .inner
      .write_value(json!({"type":"abort"}), mutation)
      .await
      .unwrap_err();
    assert_eq!(
      error.kind,
      if mutation {
        ErrorKind::Uncertain
      } else {
        ErrorKind::Unavailable
      }
    );
    assert!(matches!(
      events.recv().await.unwrap().kind,
      RuntimeEventKind::Disconnected { .. }
    ));
    adapter.inner.publish(RuntimeEventKind::State(SessionState {
      session_id: None,
      status: ExecutionState::Running,
      model: None,
      pending_inputs: None,
      revision: None,
    }));
    assert!(matches!(
      events.try_recv(),
      Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(writes.load(Ordering::SeqCst), 2);
    assert_eq!(
      adapter
        .inner
        .write_value(json!({"type":"get_state"}), false)
        .await
        .unwrap_err()
        .kind,
      ErrorKind::Unavailable
    );
    assert_eq!(
      writes.load(Ordering::SeqCst),
      2,
      "a damaged frame must never receive another write"
    );
    adapter.close().await.unwrap();
  }
}

#[tokio::test]
async fn a_query_reply_cannot_replace_a_newer_native_running_event() {
  let fixture = Fixture::new();
  fixture.state.lock().unwrap()["emit_running_after_state"] = json!(true);
  let mut events = fixture.adapter.subscribe();
  let Reply::State(state) = fixture.execute("ordered-state", Command::State).await.unwrap() else {
    panic!("state expected")
  };
  assert_eq!(state.status, ExecutionState::Running);
  let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
    .await
    .unwrap()
    .unwrap();
  assert!(matches!(
    event.kind,
    RuntimeEventKind::State(SessionState {
      status: ExecutionState::Running,
      ..
    })
  ));
  // There is one independent native state event, rather than a synthetic idle
  // event for the query followed by the real running transition.
}

#[tokio::test]
async fn common_service_recovers_pi_state_and_answers_live_dialogs() {
  let fixture = Fixture::new();
  let service = InteractionService::default();
  service.register(Arc::new(fixture.adapter.clone())).unwrap();
  let execute = |id: &str, command| InteractionRequest {
    request_id: id.to_owned(),
    target: fixture.target(),
    command,
  };
  assert!(matches!(
    service.execute(execute("core-state", Command::State)).await.unwrap(),
    Reply::State(_)
  ));
  assert!(
    matches!(service.execute(execute("core-pending", Command::PendingInteractions)).await.unwrap(), Reply::Interactions(items) if items.is_empty())
  );
  assert!(!service.snapshot("pi-test").unwrap().recovery_required);
  let mut events = service.subscribe();
  fixture.emit(json!({"type":"extension_ui_request","id":"core-confirm","method":"confirm","title":"Continue?","message":"Run this step?"})).await;
  assert_eq!(interaction(&mut events).await.interaction_id, "core-confirm");
  assert_eq!(service.snapshot("pi-test").unwrap().pending_interactions.len(), 1);
  assert!(matches!(
    service
      .execute(execute(
        "core-answer",
        Command::Respond {
          interaction_id: "core-confirm".into(),
          response: InteractionResponse::Dialog {
            value: None,
            confirmed: Some(true),
            cancelled: false
          },
        }
      ))
      .await
      .unwrap(),
    Reply::Acknowledged
  ));
  assert!(service.snapshot("pi-test").unwrap().pending_interactions.is_empty());
  let message = execute(
    "core-send",
    Command::Message {
      input: input("Continue through core"),
      delivery: DeliveryMode::StartOrSteer,
    },
  );
  assert!(matches!(
    service.execute(message.clone()).await.unwrap(),
    Reply::Admission(_)
  ));
  service.execute(message).await.unwrap();
  assert_eq!(
    fixture
      .requests
      .lock()
      .unwrap()
      .iter()
      .filter(|request| request["type"] == "prompt")
      .count(),
    1
  );
  service.detach("pi-test", "instance-1").await.unwrap();
  assert!(service.snapshot("pi-test").is_none());
}

#[tokio::test]
async fn native_session_switch_resolves_the_old_sessions_interaction_in_core() {
  let fixture = Fixture::new();
  let service = InteractionService::default();
  service.register(Arc::new(fixture.adapter.clone())).unwrap();
  let request = |id: &str, command| InteractionRequest {
    request_id: id.to_owned(),
    target: fixture.target(),
    command,
  };
  service.execute(request("bind", Command::State)).await.unwrap();
  service
    .execute(request("pending", Command::PendingInteractions))
    .await
    .unwrap();
  let mut events = service.subscribe();
  fixture
    .emit(json!({"type":"extension_ui_request","id":"old-dialog","method":"input","title":"Value"}))
    .await;
  interaction(&mut events).await;
  assert_eq!(service.snapshot("pi-test").unwrap().pending_interactions.len(), 1);
  *fixture.state.lock().unwrap() = native_state("new-session", false);
  assert_eq!(
    service
      .execute(request("old-binding", Command::State))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::StaleTarget
  );
  let resolved = tokio::time::timeout(Duration::from_secs(1), async {
    loop {
      let event = events.recv().await.unwrap();
      if matches!(event.kind, RuntimeEventKind::InteractionResolved { .. }) {
        break event;
      }
    }
  })
  .await
  .unwrap();
  assert_eq!(resolved.session_id.as_deref(), Some("pi-session"));
  assert!(service.snapshot("pi-test").unwrap().pending_interactions.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn managed_launch_uses_rpc_and_closes_its_synthetic_child() {
  use std::os::unix::fs::PermissionsExt;
  let directory = tempfile::tempdir().unwrap();
  let executable = directory.path().join("fake-pi");
  std::fs::write(&executable, r#"#!/usr/bin/env python3
import json, sys
assert sys.argv[1:3] == ['--mode', 'rpc']
for line in sys.stdin:
  request = json.loads(line)
  state = {'sessionId':'synthetic-owned','isStreaming':False,'isCompacting':False,'pendingMessageCount':0}
  print(json.dumps({'type':'response','id':request['id'],'command':request['type'],'success':True,'data':state}), flush=True)
"#).unwrap();
  std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
  let mut options = EndpointOptions::new("owned");
  options.expected_session_id = Some("synthetic-owned".into());
  let adapter = PiAdapter::spawn(
    options,
    LaunchOptions {
      executable,
      working_directory: directory.path().to_owned(),
      arguments: vec![],
      environment: BTreeMap::new(),
      session_file: None,
    },
  )
  .await
  .unwrap();
  let descriptor = adapter.descriptor();
  assert_eq!(descriptor.ownership, Ownership::Managed);
  adapter
    .execute(InteractionRequest {
      request_id: "state".into(),
      target: SessionTarget {
        runtime_id: descriptor.runtime_id,
        instance_id: descriptor.instance_id,
        session_id: Some("synthetic-owned".into()),
      },
      command: Command::State,
    })
    .await
    .unwrap();
  adapter.close().await.unwrap();
  assert_eq!(adapter.inner.session_state().status, ExecutionState::Stopped);
}
