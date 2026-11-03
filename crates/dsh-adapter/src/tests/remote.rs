//! Authenticated Remote-generation requests, without saved host configuration.
use super::*;
use axum::{
  Json,
  extract::Query,
  http::{HeaderMap, StatusCode, header},
  response::IntoResponse,
};

const TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const COOKIE: &str = "dsh-auth-fixture=v2.remote.fixture";
const CWD: &str = "/workspace/remote-fixture";

struct Native {
  calls: Vec<Value>,
  sessions: BTreeMap<String, Value>,
  selected: BTreeMap<String, Value>,
  projection_fault: bool,
  projection_cached: bool,
  projection_updates: broadcast::Sender<Value>,
  selection_gate: Arc<tokio::sync::Semaphore>,
  projection_gate: Arc<tokio::sync::Semaphore>,
  projection_started: Arc<tokio::sync::Notify>,
}

struct RemotePeer {
  endpoint: String,
  native: Arc<Mutex<Native>>,
  opens: Arc<Mutex<Vec<Value>>>,
  selection_gate: Arc<tokio::sync::Semaphore>,
  projection_gate: Arc<tokio::sync::Semaphore>,
  projection_started: Arc<tokio::sync::Notify>,
  task: JoinHandle<()>,
}

impl Drop for RemotePeer {
  fn drop(&mut self) {
    self.task.abort();
  }
}

fn assert_authenticated(headers: &HeaderMap) {
  assert_eq!(
    headers.get(header::COOKIE).and_then(|value| value.to_str().ok()),
    Some(COOKIE)
  );
  assert!(headers.get(header::AUTHORIZATION).is_none());
}

async fn remote_peer() -> RemotePeer {
  let (updates, _) = broadcast::channel(8);
  let selection_gate = Arc::new(tokio::sync::Semaphore::new(1));
  let projection_gate = Arc::new(tokio::sync::Semaphore::new(1));
  let projection_started = Arc::new(tokio::sync::Notify::new());
  let native = Arc::new(Mutex::new(Native {
    calls: Vec::new(),
    sessions: BTreeMap::new(),
    selected: BTreeMap::new(),
    projection_fault: false,
    projection_cached: false,
    projection_updates: updates.clone(),
    selection_gate: selection_gate.clone(),
    projection_gate: projection_gate.clone(),
    projection_started: projection_started.clone(),
  }));
  let opens = Arc::new(Mutex::new(Vec::new()));
  let router = Router::new()
    .route(
      "/connect",
      get(|Query(query): Query<BTreeMap<String, String>>| async move {
        assert_eq!(
          query,
          BTreeMap::from([("token".into(), TOKEN.into()), ("interactions".into(), "answer".into())])
        );
        (
          StatusCode::FOUND,
          [
            (header::LOCATION, "./api/remote.mux"),
            (
              header::SET_COOKIE,
              "dsh-auth-fixture=v2.remote.fixture; HttpOnly; SameSite=Strict",
            ),
          ],
        )
      }),
    )
    .route(
      "/api/remote.mux",
      get({
        let opens = opens.clone();
        let updates = updates.clone();
        move |headers: HeaderMap, upgrade: WebSocketUpgrade| {
          let opens = opens.clone();
          let updates = updates.subscribe();
          async move {
            assert_authenticated(&headers);
            upgrade.on_upgrade(move |socket| remote_stream(socket, opens, updates))
          }
        }
      }),
    )
    .route("/api/{*method}", post(remote_call))
    .with_state(native.clone());
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let endpoint = format!("http://{}/", listener.local_addr().unwrap());
  let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
  RemotePeer {
    endpoint,
    native,
    opens,
    selection_gate,
    projection_gate,
    projection_started,
    task,
  }
}

async fn remote_stream(mut socket: WebSocket, opens: Arc<Mutex<Vec<Value>>>, mut updates: broadcast::Receiver<Value>) {
  let mut opened = std::collections::BTreeSet::new();
  let mut control_id = None;
  while opened.len() < 2 {
    let Some(Ok(Message::Text(text))) = socket.recv().await else {
      panic!("Remote readiness requires its two stream opens");
    };
    let open: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(open["type"], "open");
    assert_eq!(open["payload"], json!({"args":{}}));
    let stream_id = open["streamId"].as_str().unwrap();
    let endpoint = open["endpoint"].as_str().unwrap();
    assert!(opened.insert(endpoint.to_owned()));
    let value = match endpoint {
      "session/control" => {
        control_id = Some(stream_id.to_owned());
        json!({"type":"baseline","value":{"projections":{}}})
      }
      "$events" => json!({"type":"ready","clientId":"remote-fixture-client","host":{"home":"/home/fixture"}}),
      other => panic!("unexpected Remote stream {other}"),
    };
    opens.lock().await.push(open.clone());
    socket
      .send(Message::Text(
        json!({"type":"item","streamId":stream_id,"value":value})
          .to_string()
          .into(),
      ))
      .await
      .unwrap();
  }
  loop {
    tokio::select! {
      value = updates.recv() => {
        let Ok(value) = value else { return };
        if socket.send(Message::Text(json!({"type":"item","streamId":control_id,"value":value}).to_string().into())).await.is_err() { return; }
      }
      frame = socket.recv() => match frame {
        Some(Ok(Message::Ping(data))) => {
          if socket.send(Message::Pong(data)).await.is_err() {
            return;
          }
        }
        Some(Ok(Message::Pong(_))) => {}
        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
        other => panic!("unexpected Remote client frame {other:?}"),
      }
    }
  }
}

async fn remote_call(
  State(native): State<Arc<Mutex<Native>>>,
  Path(method): Path<String>,
  headers: HeaderMap,
  Json(request): Json<Value>,
) -> impl IntoResponse {
  assert_authenticated(&headers);
  assert_eq!(request["type"], "client-request");
  assert_eq!(request["method"], method);
  assert!(request["rpcId"].as_str().is_some_and(|id| !id.is_empty()));
  let mut native = native.lock().await;
  native.calls.push(request.clone());
  let payload = &request["payload"];
  let arguments = &payload["args"]["request"];
  let value = match method.as_str() {
    "session/modelCatalog" => {
      assert_eq!(payload, &json!({"args":{}}));
      json!({"groups":[{"id":"fixture-provider","name":"Fixture provider","models":[{"id":"selected-model","name":"Selected model"}]}],"failures":[]})
    }
    "session/list" => {
      assert_eq!(payload, &json!({"args":{"request":{}}}));
      json!({"items":native.sessions.values().cloned().collect::<Vec<_>>()})
    }
    "session/create" => {
      let id = arguments["sessionId"].as_str().unwrap();
      assert_eq!(arguments["cwd"], CWD);
      assert!(arguments.get("agentPreset").is_none());
      assert!(
        native
          .sessions
          .insert(
            id.into(),
            json!({"sessionId":id,"cwd":CWD,"blank":true,"running":false})
          )
          .is_none()
      );
      json!({"sessionId":id,"agentPreset":"standard"})
    }
    "session/selectModel" => {
      let id = arguments["sessionId"].as_str().unwrap();
      assert!(native.sessions.contains_key(id));
      assert_eq!(arguments["provider"], "fixture-provider");
      assert_eq!(arguments["model"], "selected-model");
      let selection = json!({"provider":arguments["provider"],"model":arguments["model"]});
      assert!(native.selected.insert(id.into(), selection.clone()).is_none());
      json!({"selected":selection})
    }
    "session/projections" => {
      let id = arguments["sessionId"].as_str().unwrap();
      let next = if native.projection_fault {
        json!({"provider":"fixture-provider","model":"another-model"})
      } else {
        native.selected.get(id).cloned().unwrap()
      };
      if native.projection_cached {
        json!({"kind":"migration-required","values":{"modelSelection":{"lastUsed":null,"next":next}}})
      } else {
        json!({"kind":"sequenced","asOfSeq":0,"values":{"modelSelection":{"lastUsed":null,"next":next}}})
      }
    }
    "session/prompt" => {
      let id = arguments["sessionId"].as_str().unwrap();
      assert_eq!(arguments["requestId"], request["rpcId"]);
      assert_eq!(arguments["mode"], "steer");
      assert_eq!(arguments["content"], json!([{"type":"text","text":"First prompt"}]));
      let session = native.sessions.get_mut(id).unwrap();
      assert_eq!(session["blank"], true);
      session["blank"] = json!(false);
      session["running"] = json!(true);
      json!({"accepted":true})
    }
    other => panic!("unexpected or legacy API route {other}"),
  };
  let selection = if method == "session/selectModel" {
    Some((native.projection_updates.clone(), native.selection_gate.clone()))
  } else {
    None
  };
  let projection = if method == "session/projections" {
    Some((native.projection_gate.clone(), native.projection_started.clone()))
  } else {
    None
  };
  drop(native);
  if let Some((updates, gate)) = selection {
    updates.send(json!({"type":"projection","sessionId":arguments["sessionId"],"key":"modelSelection","value":{"lastUsed":null,"next":value["selected"]},"seq":0})).unwrap();
    let _permit = gate.acquire().await.unwrap();
  }
  if let Some((gate, started)) = projection {
    started.notify_one();
    let _permit = gate.acquire().await.unwrap();
  }
  Json(json!({"type":"server-response","rpcId":request["rpcId"],"result":{"ok":true,"value":value}}))
}

async fn connect(peer: &RemotePeer) -> DshAdapter {
  let mut config = ApiProxyConfig::new(&peer.endpoint);
  config.connect_timeout = Duration::from_secs(2);
  config.request_timeout = Duration::from_secs(2);
  DshAdapter::connect_with_launch_token(config, Ownership::Attached, Some(TOKEN.into()))
    .await
    .unwrap()
}

fn selected_model() -> ModelIdentity {
  ModelIdentity {
    model_provider: Some("fixture-provider".into()),
    model_id: "selected-model".into(),
    display_name: None,
  }
}

fn remote_request(adapter: &DshAdapter, id: Option<&str>, command: Command) -> InteractionRequest {
  let descriptor = adapter.descriptor();
  InteractionRequest {
    request_id: uuid::Uuid::new_v4().to_string(),
    target: SessionTarget {
      runtime_id: descriptor.runtime_id,
      instance_id: descriptor.instance_id,
      session_id: id.map(str::to_owned),
    },
    command,
  }
}

fn prompt() -> Command {
  Command::Message {
    input: MessageInput {
      text: "First prompt".into(),
      ..Default::default()
    },
    delivery: DeliveryMode::StartOrSteer,
  }
}

#[tokio::test]
async fn remote_catalog_uses_authenticated_named_arguments_without_creating_or_activating_a_session() {
  let peer = remote_peer().await;
  let adapter = connect(&peer).await;
  assert!(matches!(
    adapter
      .execute(remote_request(&adapter, None, Command::Metadata))
      .await
      .unwrap(),
    Reply::Metadata(_)
  ));
  let Reply::Models(models) = adapter
    .execute(remote_request(&adapter, None, Command::Models))
    .await
    .unwrap()
  else {
    panic!()
  };
  assert_eq!(models.len(), 1);
  assert_eq!(
    models[0].model,
    ModelIdentity {
      display_name: Some("Selected model".into()),
      ..selected_model()
    }
  );
  let native = peer.native.lock().await;
  assert!(native.sessions.is_empty());
  assert_eq!(
    native
      .calls
      .iter()
      .map(|call| call["method"].as_str().unwrap())
      .collect::<Vec<_>>(),
    ["session/modelCatalog"]
  );
  drop(native);
  assert_eq!(peer.opens.lock().await.len(), 2);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn remote_creation_selection_is_confirmed_from_projection_before_one_prompt() {
  let peer = remote_peer().await;
  let adapter = connect(&peer).await;
  let created = adapter
    .create_session(CreateSessionOptions {
      session_id: new_session_id(),
      working_directory: CWD.into(),
      agent_preset: None,
    })
    .await
    .unwrap();
  let id = created.target.session_id.as_deref().unwrap();
  adapter.prepare_created_model(id, &selected_model()).await.unwrap();
  adapter.prepare_created_model(id, &selected_model()).await.unwrap();
  let Reply::State(state) = adapter
    .execute(remote_request(&adapter, Some(id), Command::State))
    .await
    .unwrap()
  else {
    panic!()
  };
  assert_eq!(state.model, Some(selected_model()));
  adapter
    .execute(remote_request(&adapter, Some(id), prompt()))
    .await
    .unwrap();
  let mut override_input = MessageInput {
    text: "Do not send".into(),
    ..Default::default()
  };
  override_input.model = Some(selected_model());
  assert_eq!(
    adapter
      .execute(remote_request(
        &adapter,
        Some(id),
        Command::Message {
          input: override_input,
          delivery: DeliveryMode::StartOrSteer
        }
      ))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Unsupported
  );
  let native = peer.native.lock().await;
  let methods = native
    .calls
    .iter()
    .map(|call| call["method"].as_str().unwrap())
    .collect::<Vec<_>>();
  assert_eq!(
    methods,
    [
      "session/create",
      "session/list",
      "session/modelCatalog",
      "session/list",
      "session/selectModel",
      "session/projections",
      "session/list",
      "session/list",
      "session/prompt"
    ]
  );
  assert_eq!(native.selected.len(), 1);
  drop(native);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn remote_default_creation_sends_without_a_model_mutation() {
  let peer = remote_peer().await;
  let adapter = connect(&peer).await;
  let created = adapter
    .create_session(CreateSessionOptions {
      session_id: new_session_id(),
      working_directory: CWD.into(),
      agent_preset: None,
    })
    .await
    .unwrap();
  adapter
    .execute(remote_request(&adapter, created.target.session_id.as_deref(), prompt()))
    .await
    .unwrap();
  assert_eq!(
    peer
      .native
      .lock()
      .await
      .calls
      .iter()
      .map(|call| call["method"].as_str().unwrap())
      .collect::<Vec<_>>(),
    ["session/create", "session/list", "session/prompt"]
  );
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn remote_projection_mismatch_preserves_creation_and_never_retries_selection() {
  let peer = remote_peer().await;
  let adapter = connect(&peer).await;
  let created = adapter
    .create_session(CreateSessionOptions {
      session_id: new_session_id(),
      working_directory: CWD.into(),
      agent_preset: None,
    })
    .await
    .unwrap();
  let id = created.target.session_id.as_deref().unwrap();
  peer.native.lock().await.projection_fault = true;
  let failure = adapter.prepare_created_model(id, &selected_model()).await.unwrap_err();
  assert_eq!(failure.kind, ErrorKind::Uncertain);
  assert_eq!(
    adapter.prepare_created_model(id, &selected_model()).await.unwrap_err(),
    failure
  );
  let native = peer.native.lock().await;
  assert!(native.sessions.contains_key(id));
  assert_eq!(
    native
      .calls
      .iter()
      .filter(|call| call["method"] == "session/selectModel")
      .count(),
    1
  );
  assert!(native.calls.iter().all(|call| call["method"] != "session/prompt"));
  drop(native);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn remote_setters_own_next_projection_does_not_count_as_request_activity() {
  let peer = remote_peer().await;
  let adapter = Arc::new(connect(&peer).await);
  let created = adapter
    .create_session(CreateSessionOptions {
      session_id: new_session_id(),
      working_directory: CWD.into(),
      agent_preset: None,
    })
    .await
    .unwrap();
  let id = created.target.session_id.clone().unwrap();
  let mut events = adapter.subscribe();
  let permit = peer.selection_gate.acquire().await.unwrap();
  let preparing = tokio::spawn({
    let adapter = adapter.clone();
    let id = id.clone();
    async move { adapter.prepare_created_model(&id, &selected_model()).await }
  });
  tokio::time::timeout(Duration::from_secs(2), async {
    loop {
      let event = events.recv().await.unwrap();
      if matches!(event.kind, RuntimeEventKind::State(state) if state.session_id.as_deref() == Some(id.as_str()) && state.model == Some(selected_model())) {
        break;
      }
    }
  })
  .await
  .unwrap();
  assert!(!preparing.is_finished());
  drop(permit);
  preparing.await.unwrap().unwrap();
  assert_eq!(adapter.inner.state.lock().await.model_observations.get(&id), None);
  assert!(
    peer
      .native
      .lock()
      .await
      .calls
      .iter()
      .all(|call| call["method"] != "session/prompt")
  );
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn remote_cached_projection_hints_cannot_confirm_the_selected_model() {
  let peer = remote_peer().await;
  let adapter = connect(&peer).await;
  let created = adapter
    .create_session(CreateSessionOptions {
      session_id: new_session_id(),
      working_directory: CWD.into(),
      agent_preset: None,
    })
    .await
    .unwrap();
  let id = created.target.session_id.as_deref().unwrap();
  peer.native.lock().await.projection_cached = true;
  let failure = adapter.prepare_created_model(id, &selected_model()).await.unwrap_err();
  assert_eq!(failure.kind, ErrorKind::Uncertain);
  assert_eq!(
    adapter.prepare_created_model(id, &selected_model()).await.unwrap_err(),
    failure
  );
  let native = peer.native.lock().await;
  assert!(native.sessions.contains_key(id));
  assert_eq!(
    native
      .calls
      .iter()
      .filter(|call| call["method"] == "session/selectModel")
      .count(),
    1
  );
  assert!(native.calls.iter().all(|call| call["method"] != "session/prompt"));
  drop(native);
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn remote_newer_conflicting_next_selection_cannot_be_overwritten_by_an_older_confirmation() {
  let peer = remote_peer().await;
  let adapter = Arc::new(connect(&peer).await);
  let created = adapter
    .create_session(CreateSessionOptions {
      session_id: new_session_id(),
      working_directory: CWD.into(),
      agent_preset: None,
    })
    .await
    .unwrap();
  let id = created.target.session_id.clone().unwrap();
  let mut events = adapter.subscribe();
  // Freeze the already-read authoritative selection B before returning it to
  // the adapter. A different client can commit a newer next selection meanwhile.
  let permit = peer.projection_gate.acquire().await.unwrap();
  let preparing = tokio::spawn({
    let adapter = adapter.clone();
    let id = id.clone();
    async move { adapter.prepare_created_model(&id, &selected_model()).await }
  });
  peer.projection_started.notified().await;
  let concurrent = ModelIdentity {
    model_id: "concurrent-model".into(),
    ..selected_model()
  };
  let updates = {
    let mut native = peer.native.lock().await;
    native.selected.insert(
      id.clone(),
      json!({"provider":"fixture-provider","model":"concurrent-model"}),
    );
    native.projection_updates.clone()
  };
  updates.send(json!({"type":"projection","sessionId":id,"key":"modelSelection","value":{"lastUsed":null,"next":{"provider":"fixture-provider","model":"concurrent-model"}},"seq":1})).unwrap();
  next_state(&mut events, |state| state.model.as_ref() == Some(&concurrent)).await;
  assert!(!preparing.is_finished());
  drop(permit);
  assert_eq!(preparing.await.unwrap().unwrap_err().kind, ErrorKind::Uncertain);
  assert_eq!(adapter.inner.state.lock().await.sessions[&id].model, Some(concurrent));
  let native = peer.native.lock().await;
  assert_eq!(
    native
      .calls
      .iter()
      .filter(|call| call["method"] == "session/selectModel")
      .count(),
    1
  );
  assert!(native.calls.iter().all(|call| call["method"] != "session/prompt"));
  drop(native);
  adapter.close().await.unwrap();
}

async fn next_state(
  events: &mut broadcast::Receiver<RuntimeEvent>,
  accept: impl Fn(&SessionState) -> bool,
) -> SessionState {
  tokio::time::timeout(Duration::from_secs(2), async {
    loop {
      let event = events.recv().await.unwrap();
      if let RuntimeEventKind::State(state) = event.kind
        && accept(&state)
      {
        return state;
      }
    }
  })
  .await
  .unwrap()
}

#[tokio::test]
async fn remote_stale_projection_is_ignored_and_conflicting_replay_disconnects_with_native_frame() {
  let peer = remote_peer().await;
  let adapter = connect(&peer).await;
  let created = adapter
    .create_session(CreateSessionOptions {
      session_id: new_session_id(),
      working_directory: CWD.into(),
      agent_preset: None,
    })
    .await
    .unwrap();
  let id = created.target.session_id.as_deref().unwrap();
  let updates = peer.native.lock().await.projection_updates.clone();
  let mut events = adapter.subscribe();
  updates.send(json!({"type":"projection","sessionId":id,"key":"modelSelection","value":{"lastUsed":{"provider":"fixture-provider","model":"selected-model"},"next":null},"seq":10})).unwrap();
  let current = next_state(&mut events, |state| state.model == Some(selected_model())).await;
  assert_eq!(current.revision.as_deref(), Some("remote:10"));
  updates.send(json!({"type":"projection","sessionId":id,"key":"modelSelection","value":{"lastUsed":{"provider":"fixture-provider","model":"stale-model"},"next":null},"seq":9})).unwrap();
  // A later, independent projection is a processing barrier for the stale frame.
  updates.send(json!({"type":"projection","sessionId":id,"key":"inbox","value":{"next-turn":[{"id":"pending-item"}],"next-step":[]},"seq":11})).unwrap();
  let current = next_state(&mut events, |state| state.pending_inputs == Some(1)).await;
  assert_eq!(current.model, Some(selected_model()));
  let conflict = json!({"type":"projection","sessionId":id,"key":"modelSelection","value":{"lastUsed":{"provider":"fixture-provider","model":"conflicting-model"},"next":null},"seq":10,"future":"preserved"});
  let control_id = peer
    .opens
    .lock()
    .await
    .iter()
    .find(|open| open["endpoint"] == "session/control")
    .unwrap()["streamId"]
    .clone();
  let complete = json!({"type":"item","streamId":control_id,"value":conflict});
  updates.send(complete["value"].clone()).unwrap();
  tokio::time::timeout(Duration::from_secs(2), async {
    let mut native_observed = false;
    loop {
      match events.recv().await.unwrap().kind {
        RuntimeEventKind::Native(frame) if frame == complete => native_observed = true,
        RuntimeEventKind::Disconnected { .. } => {
          assert!(native_observed, "the conflicting native frame must remain inspectable");
          break;
        }
        _ => {}
      }
    }
  })
  .await
  .unwrap();
  assert_eq!(
    adapter.inner.state.lock().await.sessions[id].model,
    Some(selected_model())
  );
  assert!(
    adapter
      .execute(remote_request(&adapter, Some(id), Command::State))
      .await
      .is_err()
  );
  adapter.close().await.unwrap();
}
