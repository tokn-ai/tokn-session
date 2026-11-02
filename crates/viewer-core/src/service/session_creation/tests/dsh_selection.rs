//! Creation-only DSH model intent must follow its durable native identity and
//! precede the first prompt, including when native selection cannot be confirmed.
use super::*;
use crate::rpc_runtime::tests::http_peer::HttpPeer;
use crate::workspace::{ListSessionCreationModelsRequest, SessionCreationModelSelection};

#[derive(Clone, Copy, Debug)]
enum Fault {
  None,
  RejectSelection,
  LoseSelectionAcknowledgement,
  WrongConfirmation,
  HistoryBeforePrompt,
}

struct NativeState {
  id: Option<String>,
  cwd: PathBuf,
  history_root: PathBuf,
  blank: bool,
  running: bool,
  model: String,
  calls: Vec<(String, Value)>,
  default_changes: usize,
}

impl NativeState {
  fn handle(&mut self, route: &str, request: &Value, fault: Fault) -> Option<Value> {
    let payload = &request["payload"];
    self.calls.push((route.into(), payload.clone()));
    let value = match route {
      "/api/session.create" => {
        assert!(
          self.id.is_none(),
          "a creation retry must not create another native session"
        );
        self.id = Some(payload["sessionId"].as_str().unwrap().to_owned());
        assert_eq!(payload["cwd"].as_str(), self.cwd.to_str());
        assert!(payload.get("agentPreset").is_none());
        if matches!(fault, Fault::HistoryBeforePrompt) {
          self.write_blank_history();
        }
        json!({"sessionId":self.id,"agentPreset":"standard"})
      }
      "/api/session.list" => json!({"items":self.id.as_ref().map(|id| json!({
        "sessionId":id,"cwd":self.cwd,"running":self.running,"blank":self.blank
      })).into_iter().collect::<Vec<_>>()}),
      "/api/session.selectModel" => {
        assert_eq!(payload["sessionId"].as_str(), self.id.as_deref());
        assert_eq!(payload["provider"], "configured-provider");
        assert_eq!(payload["model"], "chosen-model");
        assert!(self.blank && !self.running);
        if matches!(fault, Fault::RejectSelection) {
          return Some(json!({"ok":false,"error":{"code":"rejected","message":"fixture selection rejected"}}));
        }
        self.model = "chosen-model".into();
        self.default_changes += 1;
        if matches!(fault, Fault::LoseSelectionAcknowledgement) {
          return None;
        }
        json!({"selected":{"provider":"configured-provider","model":self.model}})
      }
      "/api/session.models" => {
        assert_eq!(payload["sessionId"].as_str(), self.id.as_deref());
        json!({"current":{"provider":"configured-provider","model":if matches!(fault,Fault::WrongConfirmation) {"another-model"} else {self.model.as_str()}},
          "routable":true,"groups":[],"failures":[]})
      }
      "/api/session.prompt" => {
        assert_eq!(payload["sessionId"].as_str(), self.id.as_deref());
        assert_eq!(payload["mode"], "steer");
        assert_eq!(
          payload["content"],
          json!([{"type":"text","text":"first prompt exactly once"}])
        );
        self.blank = false;
        self.running = true;
        self.write_history();
        json!({"accepted":true})
      }
      "/api/session.cancel" => {
        assert_eq!(payload["sessionId"].as_str(), self.id.as_deref());
        self.running = false;
        json!({"accepted":true})
      }
      other => panic!("unexpected DSH selection route {other}"),
    };
    Some(json!({"ok":true,"value":value}))
  }

  fn write_history(&self) {
    let id = self.id.as_ref().unwrap();
    let records = [
      json!({"type":"session","version":0,"id":id,"createdAt":1787157590000u64,"cwd":self.cwd,"delegationDepth":0}),
      json!({"type":"turn/start","seq":0,"time":1000,"data":{"turn":1}}),
      json!({"type":"user/message","seq":1,"time":1001,"data":{"id":"first-user","role":"user","source":{"kind":"user"},"content":[{"type":"text","text":"first prompt exactly once"}]}}),
      json!({"type":"request/header","seq":2,"time":1002,"data":{"reason":"initial","header":{"config":{"provider":"configured-provider","model":self.model}}}}),
    ];
    let jsonl = records.iter().map(|value| format!("{value}\n")).collect::<String>();
    let directory = self.history_root.join(id);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("session.jsonl"), jsonl).unwrap();
  }

  fn write_blank_history(&self) {
    let id = self.id.as_ref().unwrap();
    let header =
      json!({"type":"session","version":0,"id":id,"createdAt":1787157590000u64,"cwd":self.cwd,"delegationDepth":0});
    let directory = self.history_root.join(id);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("session.jsonl"), format!("{header}\n")).unwrap();
  }

  fn count(&self, route: &str) -> usize {
    self.calls.iter().filter(|(called, _)| called == route).count()
  }
}

struct Fixture {
  _directory: tempfile::TempDir,
  service: ViewerService,
  project: crate::workspace::RegisteredProject,
  _peer: HttpPeer,
  native: Arc<Mutex<NativeState>>,
  receipt_path: PathBuf,
}

impl Fixture {
  async fn start(fault: Fault) -> Self {
    let directory = tempfile::tempdir().unwrap();
    let history_root = directory.path().join("dsh-history");
    std::fs::create_dir(&history_root).unwrap();
    let history_root = history_root.canonicalize().unwrap();
    let mut service = service(ViewerProvider::Dsh, history_root.clone());
    let receipt_path = directory.path().join("session-creations.json");
    service.sessions.creation_path = Some(receipt_path.clone());
    let project = project(&service, &directory.path().join("project"));
    let native = Arc::new(Mutex::new(NativeState {
      id: None,
      cwd: project.folder_path.clone().into(),
      history_root,
      blank: true,
      running: false,
      model: "default-model".into(),
      calls: Vec::new(),
      default_changes: 0,
    }));
    let state = native.clone();
    let peer = HttpPeer::start_dsh_with_handler(
      project.folder_path.clone(),
      Arc::new(move |route, request| state.lock().unwrap().handle(route, request, fault)),
    )
    .await;
    *peer.catalog_result.lock().unwrap() = json!({"ok":true,"value":{"groups":[{
      "id":"configured-provider","name":"Configured provider","models":[{"id":"chosen-model","name":"Chosen model"}]
    }],"failures":[]}});
    save_profile(&service, LiveProvider::Dsh, peer.endpoint.clone(), None, true).await;
    Self {
      _directory: directory,
      service,
      project,
      _peer: peer,
      native,
      receipt_path,
    }
  }

  async fn selected_request(&self) -> CreateProjectSessionRequest {
    let catalog = self
      .service
      .list_session_creation_models(ListSessionCreationModelsRequest {
        project_id: self.project.project_id.clone(),
        provider: LiveProvider::Dsh,
        runtime_profile_id: None,
        runtime_revision: None,
        runtime_plan_id: None,
      })
      .await
      .unwrap();
    assert!(catalog.can_select && catalog.updates_default);
    assert_eq!(catalog.models[0].model.model_id, "chosen-model");
    assert!(
      self.native.lock().unwrap().calls.is_empty(),
      "catalog inspection must not mutate a native session or its model"
    );
    assert!(self.service.creation_manager().unwrap().list().is_empty());
    let mut request = request(&self.project);
    request.provider = LiveProvider::Dsh;
    request.runtime_plan_id = catalog.runtime_plan_id.clone();
    request.model = Some(SessionCreationModelSelection {
      model_provider: Some("configured-provider".into()),
      model_id: "chosen-model".into(),
      catalog_revision: catalog.catalog_revision.unwrap(),
      runtime_profile_id: catalog.runtime_profile_id,
      runtime_revision: catalog.runtime_revision,
      runtime_plan_id: catalog.runtime_plan_id,
    });
    request
  }

  fn mutation_counts(&self) -> [usize; 3] {
    let state = self.native.lock().unwrap();
    [
      state.count("/api/session.create"),
      state.count("/api/session.selectModel"),
      state.count("/api/session.prompt"),
    ]
  }

  fn persisted_receipt(&self, request_id: &str) -> CreationSnapshot {
    let bytes = std::fs::read(&self.receipt_path).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("first prompt exactly once"));
    let saved: Value = serde_json::from_slice(&bytes).unwrap();
    let receipt = saved["receipts"]
      .as_array()
      .unwrap()
      .iter()
      .find(|receipt| receipt["request_id"] == request_id)
      .unwrap()
      .clone();
    serde_json::from_value(receipt).unwrap()
  }

  async fn wait_for_native_call(&self, route: &str) {
    tokio::time::timeout(Duration::from_secs(8), async {
      while self.native.lock().unwrap().count(route) == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
    })
    .await
    .unwrap_or_else(|_| panic!("DSH did not reach {route}"));
  }
}

#[tokio::test]
async fn dsh_slow_model_preparation_does_not_publish_a_stale_history_error() {
  let fixture = Fixture::start(Fault::HistoryBeforePrompt).await;
  let response = fixture._peer.block_response("/api/session.models");
  let request = fixture.selected_request().await;
  fixture.service.create_project_session(request.clone()).await.unwrap();
  fixture.wait_for_native_call("/api/session.models").await;
  // Exact native history already exists while the model confirmation is still
  // pending. Cross the 500ms publication interval before allowing first input.
  tokio::time::sleep(Duration::from_millis(800)).await;
  let manager = fixture.service.creation_manager().unwrap();
  let pending = manager.get(&request.request_id).unwrap().unwrap();
  assert!(pending.created.is_some());
  assert!(pending.session_key.is_none());
  assert!(pending.message.is_none());
  assert_eq!(pending.first_input, FirstInputState::Uncertain);
  assert_eq!(fixture.mutation_counts(), [1, 1, 0]);
  response.add_permits(1);
  let receipt = settled(&fixture.service, &request.request_id).await;
  assert_eq!(receipt.phase, CreationPhase::Ready);
  assert_eq!(receipt.first_input, FirstInputState::Accepted);
  assert!(receipt.session_key.is_some());
  assert!(receipt.message.is_none());
  assert!(fixture.persisted_receipt(&request.request_id).message.is_none());
  assert_eq!(fixture.mutation_counts(), [1, 1, 1]);
  fixture.service.sessions.shutdown().await;
}

#[tokio::test]
async fn dsh_stop_during_model_preparation_fences_first_prompt_and_keeps_native_identity() {
  let fixture = Fixture::start(Fault::HistoryBeforePrompt).await;
  let response = fixture._peer.block_response("/api/session.models");
  let request = fixture.selected_request().await;
  fixture.service.create_project_session(request.clone()).await.unwrap();
  fixture.wait_for_native_call("/api/session.models").await;
  let manager = fixture.service.creation_manager().unwrap();
  let cancelling = manager.clone();
  let request_id = request.request_id.clone();
  let stop = tokio::spawn(async move { cancelling.cancel(&request_id).await });
  tokio::time::timeout(Duration::from_secs(8), async {
    while !manager.get(&request.request_id).unwrap().unwrap().cancel_requested {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .unwrap();
  assert!(
    !stop.is_finished(),
    "Stop must drain the model preparation before acknowledging"
  );
  assert_eq!(fixture.mutation_counts(), [1, 1, 0]);
  response.add_permits(1);
  let receipt = tokio::time::timeout(Duration::from_secs(8), stop)
    .await
    .expect("Stop did not drain the pending preparation")
    .unwrap()
    .unwrap()
    .unwrap();
  assert!(receipt.cancel_requested);
  assert_eq!(receipt.first_input, FirstInputState::Rejected);
  let created = receipt.created.as_ref().unwrap();
  assert_eq!(
    Some(created.native.native_id.as_str()),
    receipt.planned_native_id.as_deref()
  );
  assert_eq!(
    fixture.native.lock().unwrap().id.as_deref(),
    Some(created.native.native_id.as_str())
  );
  let saved = fixture.persisted_receipt(&request.request_id);
  assert!(saved.cancel_requested);
  assert_eq!(
    saved.created.as_ref().unwrap().native.native_id,
    created.native.native_id
  );
  assert_eq!(fixture.mutation_counts(), [1, 1, 0]);
  assert!(fixture.native.lock().unwrap().count("/api/session.cancel") >= 1);
  fixture.service.create_project_session(request.clone()).await.unwrap();
  manager.refresh(&request.request_id).await.unwrap();
  assert_eq!(
    fixture.mutation_counts(),
    [1, 1, 0],
    "retrying a stopped draft must not replay model selection or input"
  );
  fixture.service.sessions.shutdown().await;
}

#[tokio::test]
async fn dsh_selection_is_verified_once_before_first_prompt_and_recorded_in_history() {
  let fixture = Fixture::start(Fault::None).await;
  let mut request = fixture.selected_request().await;
  let (first, duplicate) = tokio::join!(
    fixture.service.create_project_session(request.clone()),
    fixture.service.create_project_session(request.clone())
  );
  first.unwrap();
  duplicate.unwrap();
  let receipt = settled(&fixture.service, &request.request_id).await;
  assert_eq!(receipt.phase, CreationPhase::Ready);
  assert_eq!(receipt.first_input, FirstInputState::Accepted);
  assert_eq!(receipt.model.as_ref().unwrap().model_id, "chosen-model");
  let created = receipt.created.as_ref().unwrap();
  assert_eq!(
    Some(created.native.native_id.as_str()),
    receipt.planned_native_id.as_deref()
  );
  assert!(receipt.session_key.is_some());
  let saved = fixture.persisted_receipt(&request.request_id);
  assert_eq!(
    saved.created.as_ref().unwrap().native.native_id,
    created.native.native_id
  );
  assert_eq!(saved.model.as_ref().unwrap().model_id, "chosen-model");
  assert_eq!(saved.first_input, FirstInputState::Accepted);
  assert_eq!(fixture.mutation_counts(), [1, 1, 1]);
  {
    let state = fixture.native.lock().unwrap();
    let routes = state.calls.iter().map(|(route, _)| route.as_str()).collect::<Vec<_>>();
    let create = routes.iter().position(|route| *route == "/api/session.create").unwrap();
    let select = routes
      .iter()
      .position(|route| *route == "/api/session.selectModel")
      .unwrap();
    let confirm = routes.iter().position(|route| *route == "/api/session.models").unwrap();
    let prompt = routes.iter().position(|route| *route == "/api/session.prompt").unwrap();
    assert!(create < select && select < confirm && confirm < prompt);
    assert_eq!(state.count("/api/session.models"), 1);
    assert_eq!(state.default_changes, 1);
    let loaded = tokn_session_client::AgentClient::load_session(
      ViewerProvider::Dsh.source(),
      Some(state.history_root.clone()),
      state.id.as_ref().unwrap(),
    )
    .unwrap();
    assert!(loaded.events.iter().any(|event| matches!(event, AgentEvent::ProviderChanged(model)
      if model.model_provider.as_deref() == Some("configured-provider") && model.model_id.as_deref() == Some("chosen-model"))));
  }
  // A repeated first Send observes the durable receipt, including after proofs expire.
  request.model.as_mut().unwrap().catalog_revision = "0".repeat(64);
  fixture.service.create_project_session(request.clone()).await.unwrap();
  fixture
    .service
    .creation_manager()
    .unwrap()
    .refresh(&request.request_id)
    .await
    .unwrap();
  assert_eq!(fixture.mutation_counts(), [1, 1, 1]);
  fixture.service.sessions.shutdown().await;
}

#[tokio::test]
async fn dsh_runtime_default_sends_without_selecting_or_confirming_a_model() {
  let fixture = Fixture::start(Fault::None).await;
  let mut request = request(&fixture.project);
  request.provider = LiveProvider::Dsh;
  fixture.service.create_project_session(request.clone()).await.unwrap();
  let receipt = settled(&fixture.service, &request.request_id).await;
  assert_eq!(receipt.phase, CreationPhase::Ready);
  assert_eq!(receipt.first_input, FirstInputState::Accepted);
  assert!(receipt.model.is_none());
  assert_eq!(fixture.mutation_counts(), [1, 0, 1]);
  let state = fixture.native.lock().unwrap();
  assert_eq!(state.count("/api/session.models"), 0);
  assert_eq!(state.model, "default-model");
  assert_eq!(state.default_changes, 0);
  drop(state);
  fixture.service.sessions.shutdown().await;
}

#[tokio::test]
async fn dsh_failed_or_unconfirmed_selection_retains_native_identity_without_prompt_or_replay() {
  for fault in [
    Fault::RejectSelection,
    Fault::LoseSelectionAcknowledgement,
    Fault::WrongConfirmation,
  ] {
    let fixture = Fixture::start(fault).await;
    let request = fixture.selected_request().await;
    fixture.service.create_project_session(request.clone()).await.unwrap();
    let receipt = tokio::time::timeout(Duration::from_secs(8), async {
      loop {
        let receipt = fixture
          .service
          .creation_manager()
          .unwrap()
          .get(&request.request_id)
          .unwrap()
          .unwrap();
        if receipt.created.is_some() && receipt.message.is_some() {
          break receipt;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
    })
    .await
    .unwrap_or_else(|_| panic!("{fault:?} creation did not expose its failed model preparation"));
    assert!(matches!(
      receipt.first_input,
      FirstInputState::Rejected | FirstInputState::Uncertain
    ));
    let created = receipt.created.as_ref().unwrap();
    assert_eq!(
      Some(created.native.native_id.as_str()),
      receipt.planned_native_id.as_deref()
    );
    assert_eq!(
      fixture.native.lock().unwrap().id.as_deref(),
      Some(created.native.native_id.as_str())
    );
    assert!(receipt.session_key.is_none());
    let saved = fixture.persisted_receipt(&request.request_id);
    assert_eq!(
      saved.created.as_ref().unwrap().native.native_id,
      created.native.native_id
    );
    assert_eq!(saved.model.as_ref().unwrap().model_id, "chosen-model");
    assert_eq!(saved.first_input, receipt.first_input);
    assert_eq!(fixture.mutation_counts(), [1, 1, 0], "{fault:?}");
    fixture.service.create_project_session(request.clone()).await.unwrap();
    fixture
      .service
      .creation_manager()
      .unwrap()
      .refresh(&request.request_id)
      .await
      .unwrap();
    assert_eq!(
      fixture.mutation_counts(),
      [1, 1, 0],
      "{fault:?} must not replay a failed or uncertain model mutation"
    );
    assert_eq!(
      std::fs::read_dir(&fixture.native.lock().unwrap().history_root)
        .unwrap()
        .count(),
      0
    );
    fixture.service.sessions.shutdown().await;
  }
}
