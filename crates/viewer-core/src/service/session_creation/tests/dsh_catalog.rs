//! The runtime is the authority for configured DSH providers/models. Catalog
//! reads must not turn a blank viewer draft into a native session.
use super::*;
use crate::rpc_runtime::tests::http_peer::HttpPeer;
use crate::workspace::{ListSessionCreationModelsRequest, SessionCreationModelSelection};
use std::sync::atomic::Ordering;

struct Fixture {
  _directory: tempfile::TempDir,
  history_root: PathBuf,
  service: ViewerService,
  project: crate::workspace::RegisteredProject,
  peer: HttpPeer,
}

impl Fixture {
  async fn start() -> Self {
    let directory = tempfile::tempdir().unwrap();
    let history_root = directory.path().join("dsh-history");
    std::fs::create_dir(&history_root).unwrap();
    let service = service(ViewerProvider::Dsh, history_root.clone());
    let project = project(&service, &directory.path().join("project"));
    let peer = HttpPeer::start(LiveProvider::Dsh, project.folder_path.clone()).await;
    save_profile(&service, LiveProvider::Dsh, peer.endpoint.clone(), None, true).await;
    Self {
      _directory: directory,
      history_root,
      service,
      project,
      peer,
    }
  }

  fn catalog_request(&self) -> ListSessionCreationModelsRequest {
    ListSessionCreationModelsRequest {
      project_id: self.project.project_id.clone(),
      provider: LiveProvider::Dsh,
      runtime_profile_id: None,
      runtime_revision: None,
      runtime_plan_id: None,
    }
  }

  fn configured_model(&self, model_id: &str) {
    *self.peer.catalog_result.lock().unwrap() = json!({"ok":true,"value":{"groups":[{
      "id":"configured-provider", "name":"Host provider", "models":[{
        "id":model_id, "name":"Host configured model"
      }]
    }]}});
  }

  async fn assert_view_only(&self) {
    assert_eq!(std::fs::read_dir(&self.history_root).unwrap().count(), 0);
    assert!(self.service.session_index.list_all_sessions().unwrap().is_empty());
    assert!(self.service.creation_manager().unwrap().list().is_empty());
    assert!(self.service.list_rpc_runtimes().await.unwrap().runtimes.is_empty());
    assert!(self.service.interactions.snapshots().is_empty());
    assert!(self.peer.requests.lock().unwrap().iter().all(|path| matches!(
      path.as_str(),
      "/api/events.mux" | "/api/events.host" | "/api/host.describe" | "/api/llm.models"
    )));
    tokio::time::timeout(Duration::from_secs(2), async {
      while self.peer.active_downlinks.load(Ordering::SeqCst) != 0 {
        tokio::task::yield_now().await;
      }
    })
    .await
    .expect("temporary catalog attachment must close both downlinks");
  }
}

#[tokio::test]
async fn dsh_catalog_reads_and_refreshes_host_models_without_creating_a_session() {
  let fixture = Fixture::start().await;
  fixture.configured_model("host-configured-model");
  let options = fixture
    .service
    .get_session_creation_options(SessionCreationOptionsRequest {
      project_id: fixture.project.project_id.clone(),
    })
    .await
    .unwrap();
  assert!(
    options
      .providers
      .iter()
      .find(|row| row.provider == LiveProvider::Dsh)
      .unwrap()
      .available
  );
  assert!(
    fixture.peer.requests.lock().unwrap().is_empty(),
    "availability must not boot a catalog runtime"
  );

  let catalog = fixture
    .service
    .list_session_creation_models(fixture.catalog_request())
    .await
    .unwrap();
  assert!(catalog.can_select);
  assert!(catalog.updates_default);
  assert!(
    catalog.catalog_revision.is_some(),
    "selectable catalogs must provide a verified selection proof"
  );
  assert_eq!(catalog.models.len(), 1);
  assert_eq!(
    catalog.models[0].model.model_provider.as_deref(),
    Some("configured-provider")
  );
  assert_eq!(catalog.models[0].model.model_id, "host-configured-model");
  assert_eq!(
    catalog.models[0].model.display_name.as_deref(),
    Some("Host configured model")
  );
  assert!(!catalog.models[0].supports_images);
  assert!(catalog.runtime_profile_id.is_some());
  assert!(catalog.runtime_plan_id.is_some());
  assert!(catalog.reason.is_none());
  fixture.assert_view_only().await;

  fixture.configured_model("changed-host-model");
  let refreshed = fixture
    .service
    .list_session_creation_models(fixture.catalog_request())
    .await
    .unwrap();
  assert_eq!(refreshed.models[0].model.model_id, "changed-host-model");
  assert_eq!(refreshed.runtime_plan_id, catalog.runtime_plan_id);
  assert_eq!(
    fixture
      .peer
      .requests
      .lock()
      .unwrap()
      .iter()
      .filter(|path| *path == "/api/llm.models")
      .count(),
    2
  );
  fixture.assert_view_only().await;
  fixture.service.sessions.shutdown().await;
}

#[tokio::test]
async fn dsh_catalog_distinguishes_empty_models_from_runtime_errors_and_allows_retry() {
  let fixture = Fixture::start().await;
  let empty = fixture
    .service
    .list_session_creation_models(fixture.catalog_request())
    .await
    .unwrap();
  assert!(empty.models.is_empty());
  assert!(empty.can_select);
  assert!(empty.updates_default);
  assert!(empty.reason.is_some());
  fixture.assert_view_only().await;

  *fixture.peer.catalog_result.lock().unwrap() = json!({"ok":false,"error":{
    "code":"provider-unavailable", "message":"fixture-private-provider-diagnostic"
  }});
  let error = fixture
    .service
    .list_session_creation_models(fixture.catalog_request())
    .await
    .unwrap_err();
  assert!(!error.is_empty());
  assert!(!error.contains("fixture-private-provider-diagnostic"));
  fixture.assert_view_only().await;

  fixture.configured_model("recovered-host-model");
  let recovered = fixture
    .service
    .list_session_creation_models(fixture.catalog_request())
    .await
    .unwrap();
  assert_eq!(recovered.models[0].model.model_id, "recovered-host-model");
  fixture.assert_view_only().await;
  fixture.service.sessions.shutdown().await;
}

#[tokio::test]
async fn dsh_catalog_keeps_default_creation_and_rejects_stale_choice_or_plan_before_admission() {
  let fixture = Fixture::start().await;
  fixture.configured_model("host-configured-model");
  let catalog = fixture
    .service
    .list_session_creation_models(fixture.catalog_request())
    .await
    .unwrap();
  let mut creation = request(&fixture.project);
  creation.provider = LiveProvider::Dsh;
  creation.runtime_plan_id = catalog.runtime_plan_id.clone();
  let plan = fixture
    .service
    .preflight_creation_runtime(&creation, &fixture.project)
    .await
    .unwrap();
  fixture
    .service
    .preflight_creation_model(&creation, &plan)
    .await
    .unwrap();
  let before = fixture.peer.requests.lock().unwrap().len();
  creation.model = Some(SessionCreationModelSelection {
    model_provider: Some("configured-provider".into()),
    model_id: "removed-model".into(),
    catalog_revision: catalog.catalog_revision.unwrap(),
    runtime_profile_id: catalog.runtime_profile_id,
    runtime_revision: catalog.runtime_revision,
    runtime_plan_id: catalog.runtime_plan_id.clone(),
  });
  let error = fixture.service.create_project_session(creation).await.unwrap_err();
  assert!(!error.may_have_started);
  assert!(error.message.contains("no longer available"));
  assert!(fixture.peer.requests.lock().unwrap().len() > before);
  let before = fixture.peer.requests.lock().unwrap().len();

  save_profile(
    &fixture.service,
    LiveProvider::Dsh,
    fixture.peer.endpoint.clone(),
    None,
    true,
  )
  .await;
  let mut stale = fixture.catalog_request();
  stale.runtime_plan_id = catalog.runtime_plan_id;
  let error = fixture.service.list_session_creation_models(stale).await.unwrap_err();
  assert!(error.contains("runtime configuration changed"));
  assert_eq!(fixture.peer.requests.lock().unwrap().len(), before);
  fixture.assert_view_only().await;
  fixture.service.sessions.shutdown().await;
}
