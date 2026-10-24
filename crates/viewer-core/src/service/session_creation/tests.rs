use super::*;
use crate::{
  images::{ImageTarget, UploadImageChunkRequest},
  model::{InputDeliveryStatus, SubmitSessionInputRequest},
  repository::ViewerRepository,
  rpc_profiles::{RpcCredentials, RpcRuntimeProfileInput, RpcSaveProfileRequest},
  session_supervisor::CancelSessionTurnRequest,
  workspace::{CreateProjectSessionRequest, ManagedSessionRequest, RegisterProjectRequest},
};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokn_interaction_core::creation::FirstInputState;
use tokn_interaction_core::runtime::NativeSessionDescriptor;
use tokn_interaction_core::{Capabilities, SessionTarget};

struct Repository {
  provider: ViewerProvider,
  location: PathBuf,
}

impl ViewerRepository for Repository {
  fn list_session_headers(&self, provider: ViewerProvider) -> Result<Vec<SessionHeader>, String> {
    if provider != self.provider {
      return Ok(Vec::new());
    }
    tokn_session_client::AgentClient::list_session_headers(provider.source(), Some(self.location.clone()))
  }

  fn file_session_roots(&self, provider: ViewerProvider) -> Result<Vec<PathBuf>, String> {
    if provider != self.provider {
      return Ok(Vec::new());
    }
    Ok(vec![self.location.clone()])
  }

  fn session_header_at_path(&self, provider: ViewerProvider, path: &Path) -> Result<SessionHeader, String> {
    tokn_session_client::AgentClient::session_header_at_path(provider.source(), Some(self.location.clone()), path)
  }

  fn session_header_exact(
    &self,
    provider: ViewerProvider,
    path: &Path,
    session_id: &str,
  ) -> Result<SessionHeader, String> {
    if provider == ViewerProvider::Codex {
      let header = self.session_header_at_path(provider, path)?;
      if header.id != session_id {
        return Err("Exact native identity changed".into());
      }
      return Ok(header);
    }
    if provider == ViewerProvider::OpenCode {
      return tokn_session_opencode::OpenCodeSessionSource::new(Some(path.to_path_buf()))
        .session_header_exact(session_id);
    }
    self
      .list_session_headers(provider)?
      .into_iter()
      .find(|header| header.path == path && header.id == session_id)
      .ok_or_else(|| "Exact native header is unavailable".to_owned())
  }

  fn load_session(&self, locator: &SessionLocator) -> Result<LoadedSession, String> {
    tokn_session_client::AgentClient::load_session(
      locator.provider.source(),
      Some(self.location.clone()),
      &locator.session_id,
    )
  }
}

fn service(provider: ViewerProvider, location: PathBuf) -> ViewerService {
  ViewerService::new(Arc::new(Repository { provider, location }))
}

fn project(service: &ViewerService, folder: &Path) -> crate::workspace::RegisteredProject {
  std::fs::create_dir_all(folder).unwrap();
  service
    .register_project(RegisterProjectRequest {
      folder_path: folder.to_string_lossy().into_owned(),
      name: None,
    })
    .unwrap()
}

async fn save_profile(
  service: &ViewerService,
  provider: LiveProvider,
  endpoint: String,
  cwd: Option<String>,
  is_default: bool,
) -> String {
  let current = service.get_rpc_runtime_profiles().await.unwrap();
  let snapshot = service
    .save_rpc_runtime_profile(RpcSaveProfileRequest {
      expected_revision: current.revision,
      profile: RpcRuntimeProfileInput {
        profile_id: None,
        name: format!("{provider:?} test"),
        provider,
        mode: RpcRuntimeMode::Attached,
        executable: None,
        arguments: Vec::new(),
        working_directory: cwd,
        endpoint: Some(endpoint),
        environment: None,
        credentials: Some(RpcCredentials::None),
        is_default,
      },
    })
    .await
    .unwrap();
  snapshot.profiles.last().unwrap().profile_id.clone()
}

struct CodexPeer {
  endpoint: String,
  requests: Arc<Mutex<Vec<Value>>>,
  task: tokio::task::JoinHandle<()>,
}

impl CodexPeer {
  async fn start(history: PathBuf, cwd: PathBuf, supports_images: bool) -> Self {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let task = tokio::spawn(async move {
      let mut peers = tokio::task::JoinSet::new();
      loop {
        let Ok((stream, _)) = listener.accept().await else {
          return;
        };
        let requests = recorded.clone();
        let history = history.clone();
        let cwd = cwd.clone();
        peers.spawn(async move {
          let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
          let mut active = false;
          while let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) = socket.next().await {
            let request: Value = serde_json::from_str(&text).unwrap();
            requests.lock().unwrap().push(request.clone());
            let Some(id) = request.get("id") else { continue };
            let thread = |active: bool| json!({
              "id":"created-codex", "cwd":cwd, "path":history, "ephemeral":false,
              "status":{"type":if active {"active"} else {"idle"}},
              "model":"test-model", "modelProvider":"test", "turns":[],
            });
            let result = match request["method"].as_str().unwrap() {
              "initialize" => json!({"userAgent":"creation-test"}),
              "thread/start" => {
                assert!(!history.exists(), "native history must remain deferred until first input");
                assert_eq!(request["params"]["cwd"], json!(cwd));
                assert_eq!(request["params"]["ephemeral"], false);
                json!({"thread":thread(false),"cwd":cwd,"model":"test-model","modelProvider":"test"})
              },
              "thread/read" => json!({"thread":thread(active)}),
              "thread/turns/list" => json!({"data":if active {vec![json!({"id":"test-turn","status":"inProgress","items":[]})]}else{Vec::new()},"nextCursor":null}),
              "model/list" => json!({"data":[{"id":"test-model","model":"test-model","displayName":"Test","inputModalities":if supports_images {vec!["text","image"]}else{vec!["text"]}}],"nextCursor":null}),
              "turn/start" => {
                if !history.exists() {
                  std::fs::write(&history, format!("{}\n", json!({"type":"session_meta","payload":{"id":"created-codex","cwd":cwd,"timestamp":"2026-10-25T00:00:00Z","model_provider":"test"}}))).unwrap();
                }
                active = true;
                json!({"turn":{"id":"test-turn","status":"inProgress","items":[]}})
              },
              "turn/interrupt" => { active = false; json!({}) },
              other => panic!("unexpected creation RPC {other}"),
            };
            if socket.send(tokio_tungstenite::tungstenite::Message::Text(json!({"id":id,"result":result}).to_string().into())).await.is_err() { break; }
          }
        });
      }
    });
    Self {
      endpoint,
      requests,
      task,
    }
  }

  fn count(&self, method: &str) -> usize {
    self
      .requests
      .lock()
      .unwrap()
      .iter()
      .filter(|request| request["method"] == method)
      .count()
  }
}

impl Drop for CodexPeer {
  fn drop(&mut self) {
    self.task.abort();
  }
}

fn request(project: &crate::workspace::RegisteredProject) -> CreateProjectSessionRequest {
  CreateProjectSessionRequest {
    request_id: uuid::Uuid::new_v4().to_string(),
    project_id: project.project_id.clone(),
    prompt: "first prompt exactly once".into(),
    image_ids: Vec::new(),
    provider: LiveProvider::Codex,
    runtime_profile_id: None,
    runtime_revision: None,
  }
}

async fn settled(service: &ViewerService, request_id: &str) -> CreationSnapshot {
  tokio::time::timeout(Duration::from_secs(8), async {
    loop {
      let receipt = service.creation_manager().unwrap().get(request_id).unwrap().unwrap();
      if matches!(receipt.phase, CreationPhase::Ready | CreationPhase::Failed)
        || receipt.first_input == FirstInputState::Rejected
      {
        return receipt;
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .unwrap_or_else(|_| {
    panic!(
      "creation did not settle: {:?}",
      service.creation_manager().unwrap().get(request_id)
    )
  })
}

#[tokio::test]
async fn codex_creation_indexes_real_history_and_reuses_one_runtime_for_followup_and_stop() {
  let directory = tempfile::tempdir().unwrap();
  let history_root = directory.path().join("codex");
  std::fs::create_dir(&history_root).unwrap();
  let service = service(ViewerProvider::Codex, history_root.clone());
  let project = project(&service, &directory.path().join("project"));
  let history = history_root.join("created.jsonl");
  let peer = CodexPeer::start(history.clone(), project.folder_path.clone().into(), true).await;
  save_profile(&service, LiveProvider::Codex, peer.endpoint.clone(), None, true).await;
  let options = service
    .get_session_creation_options(SessionCreationOptionsRequest {
      project_id: project.project_id.clone(),
    })
    .await
    .unwrap();
  assert!(
    options
      .providers
      .iter()
      .find(|provider| provider.provider == LiveProvider::Codex)
      .unwrap()
      .available
  );
  assert_eq!(peer.count("initialize"), 0, "options must not activate runtimes");
  let request = request(&project);
  let (first, second) = tokio::join!(
    service.create_project_session(request.clone()),
    service.create_project_session(request.clone())
  );
  first.unwrap();
  second.unwrap();
  let receipt = settled(&service, &request.request_id).await;
  assert_eq!(receipt.phase, CreationPhase::Ready);
  assert_eq!(receipt.first_input, FirstInputState::Accepted);
  assert!(history.is_file());
  let snapshot = service
    .get_managed_session(ManagedSessionRequest {
      request_id: Some(request.request_id.clone()),
      session_key: None,
    })
    .await
    .unwrap()
    .unwrap();
  let summary = snapshot.session.expect("navigation requires an indexed real session");
  assert_eq!(summary.session_id, "created-codex");
  let key = receipt.session_key.unwrap();
  assert!(service.sessions.runtimes.binding(&key).is_some());
  assert_eq!(peer.count("thread/start"), 1);
  assert_eq!(peer.count("turn/start"), 1);
  let input = SubmitSessionInputRequest {
    request_id: uuid::Uuid::new_v4().to_string(),
    session_key: key.clone(),
    text: "follow up".into(),
    image_ids: Vec::new(),
    model_selection: None,
  };
  let first = service.submit_session_input(input.clone()).await.unwrap();
  let duplicate = service.submit_session_input(input).await.unwrap();
  assert_eq!(first.status, InputDeliveryStatus::Accepted);
  assert_eq!(duplicate.status, first.status);
  assert_eq!(peer.count("turn/start"), 2);
  let cancellation = service
    .cancel_session_turn(CancelSessionTurnRequest {
      request_id: uuid::Uuid::new_v4().to_string(),
      session_key: key,
    })
    .await
    .unwrap();
  assert_eq!(cancellation.status, InputDeliveryStatus::Accepted);
  assert_eq!(peer.count("turn/interrupt"), 1);
  assert_eq!(
    peer.count("thread/resume"),
    0,
    "created bindings must be adopted without resume"
  );
  assert_eq!(
    service.list_rpc_runtimes().await.unwrap().runtimes[0].status,
    "connected"
  );
  service.sessions.shutdown().await;
}

#[tokio::test]
async fn rpc_creation_uuid_cannot_admit_pi_while_a_duplicate_rpc_call_observes_it() {
  let directory = tempfile::tempdir().unwrap();
  let history_root = directory.path().join("codex");
  std::fs::create_dir(&history_root).unwrap();
  let mut service = service(ViewerProvider::Codex, history_root.clone());
  // A regression must never launch an installed Pi or reach a provider account.
  service.sessions.managed =
    crate::managed_sessions::ManagedSessions::with_test_executable(None, directory.path().join("must-not-start-pi"))
      .unwrap();
  let project = project(&service, &directory.path().join("project"));
  let peer = CodexPeer::start(
    history_root.join("created.jsonl"),
    project.folder_path.clone().into(),
    true,
  )
  .await;
  save_profile(&service, LiveProvider::Codex, peer.endpoint.clone(), None, true).await;
  let request = request(&project);
  service.create_project_session(request.clone()).await.unwrap();
  let mut pi = request.clone();
  pi.provider = LiveProvider::Pi;
  let (observed, rejected) = tokio::join!(
    service.create_project_session(request.clone()),
    service.create_project_session(pi.clone()),
  );
  assert_eq!(observed.unwrap().request_id, request.request_id);
  let rejected = rejected.unwrap_err();
  assert!(!rejected.may_have_started);
  assert!(rejected.message.contains("another agent"));
  let identity = ManagedSessionRequest {
    request_id: Some(request.request_id.clone()),
    session_key: None,
  };
  assert!(service.sessions.managed.get(&identity).unwrap().is_none());
  let receipt = settled(&service, &request.request_id).await;
  assert_eq!(receipt.phase, CreationPhase::Ready);
  let rejected = service.create_project_session(pi).await.unwrap_err();
  assert!(!rejected.may_have_started);
  assert!(service.sessions.managed.get(&identity).unwrap().is_none());
  assert_eq!(peer.count("thread/start"), 1);
  assert_eq!(peer.count("turn/start"), 1);
  service.sessions.shutdown().await;
}

#[tokio::test]
async fn unsupported_initial_images_never_send_a_first_prompt() {
  use base64::Engine;
  let directory = tempfile::tempdir().unwrap();
  let history_root = directory.path().join("codex");
  std::fs::create_dir(&history_root).unwrap();
  let service = service(ViewerProvider::Codex, history_root.clone());
  let project = project(&service, &directory.path().join("project"));
  let history = history_root.join("created.jsonl");
  let peer = CodexPeer::start(history.clone(), project.folder_path.clone().into(), false).await;
  save_profile(&service, LiveProvider::Codex, peer.endpoint.clone(), None, true).await;
  let mut bytes = std::io::Cursor::new(Vec::new());
  image::DynamicImage::new_rgb8(1, 1)
    .write_to(&mut bytes, image::ImageFormat::Png)
    .unwrap();
  let bytes = bytes.into_inner();
  let uploaded = service
    .upload_image_chunk(UploadImageChunkRequest {
      target: ImageTarget::Project {
        project_id: project.project_id.clone(),
      },
      upload_id: uuid::Uuid::new_v4().to_string(),
      offset: 0,
      total_bytes: bytes.len(),
      mime_type: "image/png".into(),
      name: None,
      data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
    .await
    .unwrap();
  let mut request = request(&project);
  request.prompt.clear();
  request.image_ids.push(uploaded.image_id.unwrap());
  service.create_project_session(request.clone()).await.unwrap();
  let receipt = settled(&service, &request.request_id).await;
  assert_eq!(receipt.first_input, FirstInputState::Rejected);
  assert!(receipt.message.unwrap().contains("image support"));
  assert_eq!(peer.count("thread/start"), 1);
  assert_eq!(peer.count("turn/start"), 0);
  assert!(!history.exists());
  service.sessions.shutdown().await;
}

#[tokio::test]
async fn creation_options_reject_remote_endpoints_and_other_opencode_folders_without_activation() {
  let directory = tempfile::tempdir().unwrap();
  let service = service(ViewerProvider::Codex, directory.path().to_path_buf());
  let project = project(&service, &directory.path().join("project"));
  save_profile(
    &service,
    LiveProvider::Dsh,
    "https://example.invalid".into(),
    None,
    true,
  )
  .await;
  save_profile(
    &service,
    LiveProvider::OpenCode,
    "http://127.0.0.1:1".into(),
    Some(directory.path().to_string_lossy().into_owned()),
    true,
  )
  .await;
  let options = service
    .get_session_creation_options(SessionCreationOptionsRequest {
      project_id: project.project_id,
    })
    .await
    .unwrap();
  for provider in [LiveProvider::Dsh, LiveProvider::OpenCode] {
    let choice = options
      .providers
      .iter()
      .find(|choice| choice.provider == provider)
      .unwrap();
    assert!(!choice.available);
    assert!(choice.reason.is_some());
  }
  assert!(service.list_rpc_runtimes().await.unwrap().runtimes.is_empty());
  service.sessions.shutdown().await;
}

fn receipt(project: &crate::workspace::RegisteredProject, provider: LiveProvider, id: &str) -> CreationSnapshot {
  CreationSnapshot {
    request_id: uuid::Uuid::new_v4().to_string(),
    provider,
    project_id: project.project_id.clone(),
    working_directory: project.folder_path.clone().into(),
    runtime: None,
    fingerprint: "a".repeat(64),
    revision: 1,
    phase: CreationPhase::HistoryPending,
    planned_native_id: Some(id.into()),
    created: Some(CreatedNativeSession {
      provider,
      profile_id: uuid::Uuid::new_v4().to_string(),
      target: SessionTarget {
        runtime_id: "test".into(),
        instance_id: "instance".into(),
        session_id: Some(id.into()),
      },
      native: NativeSessionDescriptor {
        native_id: id.into(),
        working_directory: project.folder_path.clone().into(),
        history_path: None,
      },
      capabilities: Capabilities::default(),
    }),
    first_input: FirstInputState::Accepted,
    session_key: None,
    message: None,
    cancel_requested: false,
  }
}

fn publisher(service: &ViewerService) -> HistoryPublisher {
  HistoryPublisher {
    repository: service.repository.clone(),
    index: service.session_index.clone(),
    gate: service.index_refresh_gate.clone(),
    retry: service.index_retry_sender.clone(),
  }
}

#[tokio::test]
async fn dsh_publication_requires_matching_native_header_before_indexing() {
  let directory = tempfile::tempdir().unwrap();
  let root = directory.path().join("dsh");
  std::fs::create_dir(&root).unwrap();
  let service = service(ViewerProvider::Dsh, root.clone());
  let project = project(&service, &directory.path().join("project"));
  let mut receipt = receipt(&project, LiveProvider::Dsh, "created-dsh");
  receipt.created = None;
  receipt.phase = CreationPhase::NativeCreateUncertain;
  receipt.first_input = FirstInputState::NotAttempted;
  assert!(publisher(&service).publish(&receipt).unwrap().is_none());
  let folder = root.join("created-dsh");
  std::fs::create_dir(&folder).unwrap();
  let history = folder.join("session.jsonl");
  std::fs::write(&history, format!("{}\n", json!({"type":"session","version":0,"id":"created-dsh","createdAt":1792886400000_u64,"cwd":"/another/project","delegationDepth":0}))).unwrap();
  assert!(publisher(&service).publish(&receipt).is_err());
  assert!(service.session_index.list_present_sessions().unwrap().is_empty());
  std::fs::write(&history, format!("{}\n", json!({"type":"session","version":0,"id":"created-dsh","createdAt":1792886400000_u64,"cwd":project.folder_path,"delegationDepth":0}))).unwrap();
  let host = Host {
    runtimes: service.sessions.runtimes.clone(),
    history: publisher(&service),
  };
  let key = host.publish_history(&receipt).await.unwrap().unwrap();
  assert_eq!(receipt.phase, CreationPhase::NativeCreateUncertain);
  assert_eq!(receipt.first_input, FirstInputState::NotAttempted);
  assert!(
    service.sessions.runtimes.inventory().runtimes.is_empty(),
    "history reconciliation must not activate a runtime or send input"
  );
  assert_eq!(decode_session_key(&key).unwrap().source_path, history);
  assert!(
    service
      .session_index
      .session(&index_session_key(&decode_session_key(&key).unwrap()).unwrap())
      .unwrap()
      .unwrap()
      .present
  );
}

#[tokio::test]
async fn opencode_publication_preserves_other_sessions_in_the_shared_v2_database() {
  let directory = tempfile::tempdir().unwrap();
  let database = directory.path().join("opencode.db");
  let connection = rusqlite::Connection::open(&database).unwrap();
  connection.execute_batch(
    "create table session (id text primary key, parent_id text, directory text not null, time_created integer not null, time_updated integer not null);
     create table session_message (id text primary key, session_id text not null, type text not null, seq integer not null, time_created integer not null, data text not null);"
  ).unwrap();
  let service = service(ViewerProvider::OpenCode, database.clone());
  let project = project(&service, &directory.path().join("project"));
  connection
    .execute(
      "insert into session values ('ses_existing', null, ?1, 1, 1)",
      [&project.folder_path],
    )
    .unwrap();
  let first = receipt(&project, LiveProvider::OpenCode, "ses_existing");
  let old_key = publisher(&service).publish(&first).unwrap().unwrap();
  connection
    .execute(
      "insert into session values ('ses_created', null, ?1, 2, 2)",
      [&project.folder_path],
    )
    .unwrap();
  let mut created = receipt(&project, LiveProvider::OpenCode, "ses_created");
  created.created = None;
  created.phase = CreationPhase::NativeCreateUncertain;
  created.first_input = FirstInputState::NotAttempted;
  let host = Host {
    runtimes: service.sessions.runtimes.clone(),
    history: publisher(&service),
  };
  connection
    .execute(
      "update session set directory = '/another/project' where id = 'ses_created'",
      [],
    )
    .unwrap();
  assert!(host.publish_history(&created).await.is_err());
  assert_eq!(service.session_index.list_present_sessions().unwrap().len(), 1);
  connection
    .execute(
      "update session set directory = ?1 where id = 'ses_created'",
      [&project.folder_path],
    )
    .unwrap();
  let key = host.publish_history(&created).await.unwrap().unwrap();
  assert_eq!(created.phase, CreationPhase::NativeCreateUncertain);
  assert_eq!(created.first_input, FirstInputState::NotAttempted);
  assert!(
    service.sessions.runtimes.inventory().runtimes.is_empty(),
    "history reconciliation must not activate a runtime or send input"
  );
  assert_ne!(old_key, key);
  let rows = service.session_index.list_present_sessions().unwrap();
  assert_eq!(
    rows.len(),
    2,
    "publishing one new session must preserve the shared source catalog"
  );
  assert!(rows.iter().any(|row| row.key.session_id == "ses_existing"));
  assert!(rows.iter().any(|row| row.key.session_id == "ses_created"));
}

#[test]
fn competing_index_commit_during_creation_header_read_keeps_its_checkpoint() {
  struct CompetingRepository {
    header: SessionHeader,
    index: Arc<SessionIndex>,
  }
  impl ViewerRepository for CompetingRepository {
    fn list_session_headers(&self, _: ViewerProvider) -> Result<Vec<SessionHeader>, String> {
      Ok(vec![self.header.clone()])
    }
    fn session_header_exact(&self, _: ViewerProvider, _: &Path, _: &str) -> Result<SessionHeader, String> {
      let source = index_source_key_for_path(ViewerProvider::Dsh, &self.header.path)?;
      let mut winner = session_metadata_from_header(&source, self.header.clone(), None, false)?;
      winner.title = Some("Concurrent winner".into());
      self
        .index
        .replace_sources(&[SourceReplacement::new(
          SourceState::new(source, "concurrent-index-winner", 1),
          vec![winner],
        )])
        .map_err(|error| error.to_string())?;
      Ok(self.header.clone())
    }
    fn load_session(&self, _: &SessionLocator) -> Result<LoadedSession, String> {
      panic!("creation indexing must not read conversation bodies")
    }
  }
  let directory = tempfile::tempdir().unwrap();
  let root = directory.path().join("dsh");
  let folder = root.join("created-dsh");
  std::fs::create_dir_all(&folder).unwrap();
  let mut service = service(ViewerProvider::Dsh, root);
  let project = project(&service, &directory.path().join("project"));
  let history = folder.join("session.jsonl");
  std::fs::write(&history, format!("{}\n", json!({"type":"session","version":0,"id":"created-dsh","createdAt":1792886400000_u64,"cwd":project.folder_path,"delegationDepth":0}))).unwrap();
  let header = service
    .repository
    .list_session_headers(ViewerProvider::Dsh)
    .unwrap()
    .remove(0);
  service.repository = Arc::new(CompetingRepository {
    header,
    index: service.session_index.clone(),
  });
  let receipt = receipt(&project, LiveProvider::Dsh, "created-dsh");
  assert!(
    publisher(&service).publish(&receipt).unwrap().is_none(),
    "a competing header-time commit must invalidate creation's earlier checkpoint"
  );
  let source = index_source_key_for_path(ViewerProvider::Dsh, &history).unwrap();
  assert_eq!(
    service.session_index.source_state(&source).unwrap().unwrap().cursor,
    "concurrent-index-winner"
  );
}
