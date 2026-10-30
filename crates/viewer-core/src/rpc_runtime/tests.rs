use super::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
  path::Path,
  sync::atomic::{AtomicBool, AtomicUsize},
  time::Duration,
};
use tokio::{sync::watch, task::JoinSet};
mod http_peer;
mod resolution;
mod workflow_tests;

pub(crate) struct CodexPeer {
  pub(crate) endpoint: String,
  requests: Arc<Mutex<Vec<Value>>>,
  model_catalog: Arc<Mutex<Vec<Value>>>,
  connections: Arc<AtomicUsize>,
  active: Arc<AtomicBool>,
  drop_prompt_ack: Arc<AtomicBool>,
  initialize_gate: Arc<tokio::sync::Semaphore>,
  stop: watch::Sender<bool>,
  task: tokio::task::JoinHandle<()>,
}

impl CodexPeer {
  pub(crate) async fn start(gated: bool) -> Self {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let model_catalog = Arc::new(Mutex::new(Vec::new()));
    let catalog = model_catalog.clone();
    let connections = Arc::new(AtomicUsize::new(0));
    let accepted = connections.clone();
    let active = Arc::new(AtomicBool::new(false));
    let native_active = active.clone();
    let drop_prompt_ack = Arc::new(AtomicBool::new(false));
    let drop_ack = drop_prompt_ack.clone();
    let initialize_gate = Arc::new(tokio::sync::Semaphore::new(if gated { 0 } else { 32 }));
    let gate = initialize_gate.clone();
    let (stop, mut stopping) = watch::channel(false);
    let task = tokio::spawn(async move {
      let mut peers = JoinSet::new();
      loop {
        tokio::select! {
          _ = stopping.changed() => break,
          connection = listener.accept() => {
            let Ok((stream, _)) = connection else { break };
            accepted.fetch_add(1, Ordering::Relaxed);
            let requests = recorded.clone();
            let catalog = catalog.clone();
            let gate = gate.clone();
            let active = native_active.clone();
            let drop_ack = drop_ack.clone();
            peers.spawn(async move {
              let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else { return };
              while let Some(Ok(frame)) = socket.next().await {
                let tokio_tungstenite::tungstenite::Message::Text(text) = frame else { continue };
                let request: Value = serde_json::from_str(&text).unwrap();
                requests.lock().unwrap().push(request.clone());
                if request["method"] == "turn/start" && drop_ack.load(Ordering::Relaxed) { break; }
                let Some(id) = request.get("id") else { continue };
                let result = match request["method"].as_str().unwrap_or_default() {
                  "initialize" => {
                    let permit = gate.acquire().await.unwrap();
                    permit.forget();
                    json!({"userAgent":"viewer-test-codex"})
                  }
                  "thread/resume" | "thread/read" => json!({
                    "thread":{
                      "id":request["params"]["threadId"], "status":{"type":if active.load(Ordering::Relaxed){"active"}else{"idle"}},
                      "model":"test-model", "modelProvider":"test", "turns":[]
                    }
                  }),
                  "model/list" => json!({"data":catalog.lock().unwrap().clone(),"nextCursor":null}),
                  "thread/start" => json!({"model":request["params"]["model"],"modelProvider":"test","thread":{
                    "id":"created-test-thread", "status":{"type":"idle"}, "cwd":request["params"]["cwd"],
                    "path":"/tmp/created-test-thread.jsonl", "ephemeral":false, "turns":[],
                    "model":request["params"]["model"], "modelProvider":"test"
                  }}),
                  "turn/start" => {active.store(true,Ordering::Relaxed);json!({"turn":{"id":"test-turn","status":"inProgress","items":[]}})},
                  other => panic!("unexpected native method: {other}"),
                };
                if socket.send(tokio_tungstenite::tungstenite::Message::Text(
                  json!({"id":id,"result":result}).to_string().into()
                )).await.is_err() { break }
              }
            });
          }
        }
      }
    });
    Self {
      endpoint,
      requests,
      model_catalog,
      connections,
      active,
      drop_prompt_ack,
      initialize_gate,
      stop,
      task,
    }
  }

  pub(crate) fn methods(&self) -> Vec<String> {
    self
      .requests
      .lock()
      .unwrap()
      .iter()
      .filter_map(|request| request["method"].as_str().map(str::to_owned))
      .collect()
  }

  pub(crate) fn drop_prompt_acknowledgements(&self) {
    self.drop_prompt_ack.store(true, Ordering::Relaxed);
  }
}

impl Drop for CodexPeer {
  fn drop(&mut self) {
    let _ = self.stop.send(true);
    self.task.abort();
  }
}

fn attached_profile(endpoint: String) -> RpcRuntimeProfileInput {
  RpcRuntimeProfileInput {
    profile_id: None,
    name: "Codex test server".into(),
    provider: Provider::Codex,
    mode: RpcRuntimeMode::Attached,
    executable: None,
    arguments: Vec::new(),
    working_directory: None,
    endpoint: Some(endpoint),
    environment: None,
    credentials: None,
    is_default: true,
  }
}

fn save_profile(manager: &RpcRuntimeManager, input: RpcRuntimeProfileInput) -> RpcStartRuntimeRequest {
  let store = manager.profiles().unwrap();
  let snapshot = store
    .save(RpcSaveProfileRequest {
      expected_revision: store.snapshot().unwrap().revision,
      profile: input,
    })
    .unwrap();
  RpcStartRuntimeRequest {
    request_id: uuid::Uuid::new_v4().to_string(),
    profile_id: snapshot.profiles.last().unwrap().profile_id.clone(),
    expected_revision: snapshot.revision,
  }
}

fn close_request(info: &RpcRuntimeInfo) -> RpcCloseRuntimeRequest {
  RpcCloseRuntimeRequest {
    request_id: uuid::Uuid::new_v4().to_string(),
    runtime_id: info.runtime_id.clone(),
    expected_instance_id: info.instance_id.clone(),
  }
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
  tokio::time::timeout(Duration::from_secs(5), async {
    while !condition() {
      tokio::time::sleep(Duration::from_millis(5)).await;
    }
  })
  .await
  .expect("runtime condition did not become true");
}

fn manager() -> RpcRuntimeManager {
  let manager = RpcRuntimeManager::new(None, InteractionService::default());
  manager.set_discovery_environment(discovery::DiscoveryEnvironment::default());
  manager
}

#[test]
fn pinned_builtin_model_runtime_does_not_follow_a_new_saved_default_or_another_project() {
  let manager = manager();
  let folder = tempfile::tempdir().unwrap();
  let cwd = std::fs::canonicalize(folder.path()).unwrap();
  let environment = discovery::DiscoveryEnvironment {
    path: None,
    executables: BTreeMap::from([(
      "TOKN_SESSION_CODEX_EXECUTABLE".into(),
      std::env::current_exe().unwrap().into_os_string(),
    )]),
  };
  manager.set_discovery_environment(environment);
  let pinned = manager
    .resolve_plan(Provider::Codex, &cwd, None)
    .unwrap()
    .shared_runtime_selection()
    .unwrap();
  let added = save_profile(&manager, attached_profile("ws://127.0.0.1:1".into()));
  assert_eq!(
    manager
      .creation_selection(Provider::Codex, &cwd, Some(&pinned))
      .unwrap(),
    Some(pinned.clone())
  );
  assert_eq!(
    manager
      .resolve_plan(Provider::Codex, &cwd, Some(&pinned))
      .unwrap()
      .shared_runtime_selection()
      .unwrap(),
    pinned.clone()
  );
  assert_eq!(
    manager
      .creation_selection(Provider::Codex, &cwd, None)
      .unwrap()
      .unwrap()
      .profile_id,
    added.profile_id
  );
  let other = tempfile::tempdir().unwrap();
  assert!(
    manager
      .creation_selection(Provider::Codex, other.path(), Some(&pinned))
      .is_err()
  );
  assert!(manager.inventory().runtimes.is_empty());
}

#[tokio::test]
async fn codex_draft_catalog_is_sessionless_and_creation_revalidates_then_passes_model() {
  let peer = CodexPeer::start(false).await;
  peer.model_catalog.lock().unwrap().push(json!({
    "id":"test-model", "model":"test-model", "displayName":"Test model", "description":"Model for test", "hidden":false,
    "isDefault":true, "defaultReasoningEffort":"medium", "supportedReasoningEfforts":[], "inputModalities":["text","image"]
  }));
  let manager = manager();
  let profile = save_profile(&manager, attached_profile(peer.endpoint.clone()));
  let selection = core_runtime::RuntimeSelection {
    profile_id: profile.profile_id,
    expected_revision: profile.expected_revision,
  };
  let models = manager
    .creation_models(Provider::Codex, PathBuf::from("/tmp"), Some(selection.clone()))
    .await
    .unwrap();
  assert_eq!(models.len(), 1);
  assert_eq!(models[0].model.model_id, "test-model");
  assert!(models[0].supports_images);
  assert!(manager.inventory().runtimes.is_empty());
  assert!(
    !peer
      .methods()
      .iter()
      .any(|method| matches!(method.as_str(), "thread/start" | "thread/resume" | "turn/start"))
  );
  let created = manager
    .create_native_session(
      Provider::Codex,
      PathBuf::from("/tmp"),
      Some(selection),
      None,
      Some(models[0].model.clone()),
    )
    .await
    .unwrap();
  assert_eq!(created.native.native_id, "created-test-thread");
  let requests = peer.requests.lock().unwrap().clone();
  let create = requests
    .iter()
    .position(|request| request["method"] == "thread/start")
    .unwrap();
  assert_eq!(requests[create]["params"]["model"], "test-model");
  assert!(requests[create]["params"].get("modelProvider").is_none());
  assert_eq!(requests[create - 1]["method"], "model/list");
  assert!(
    !peer
      .methods()
      .iter()
      .any(|method| matches!(method.as_str(), "thread/resume" | "turn/start"))
  );
  assert_eq!(peer.connections.load(Ordering::Relaxed), 2);
  manager.shutdown().await;
}

#[tokio::test]
async fn opening_or_listing_runtime_configuration_never_registers_or_launches() {
  let manager = manager();
  assert!(manager.profiles().unwrap().snapshot().unwrap().profiles.is_empty());
  assert!(manager.inventory().runtimes.is_empty());
  assert!(manager.0.interactions.snapshots().is_empty());
  manager.shutdown().await;
}
#[tokio::test]
async fn start_rejects_invalid_and_stale_configuration_without_registering() {
  let manager = manager();
  assert!(
    manager
      .start(RpcStartRuntimeRequest {
        request_id: "bad".into(),
        profile_id: uuid::Uuid::new_v4().to_string(),
        expected_revision: 0
      })
      .await
      .is_err()
  );
  let result = manager
    .start(RpcStartRuntimeRequest {
      request_id: uuid::Uuid::new_v4().to_string(),
      profile_id: uuid::Uuid::new_v4().to_string(),
      expected_revision: 10,
    })
    .await;
  assert!(result.is_err());
  assert!(manager.inventory().runtimes.is_empty());
  assert!(manager.0.interactions.snapshots().is_empty());
  manager.shutdown().await;
}
#[test]
fn runtime_errors_redact_explicit_credentials_and_environment_values() {
  let profile = RpcResolvedProfile {
    revision: 1,
    profile: RpcRuntimeProfile {
      profile_id: uuid::Uuid::new_v4().to_string(),
      name: "Demo".into(),
      provider: Provider::Codex,
      mode: RpcRuntimeMode::Attached,
      executable: None,
      arguments: vec![],
      working_directory: None,
      endpoint: Some("ws://127.0.0.1:1".into()),
      environment_keys: vec!["SECRET".into()],
      credential_kind: Some("bearer".into()),
      is_default: false,
    },
    environment: BTreeMap::from([("SECRET".into(), "private-env".into())]),
    credentials: RpcCredentials::Bearer {
      token: "private-token".into(),
    },
  };
  let message = factory::redact_error("request private-token failed; config private-env", &profile);
  assert!(!message.contains("private-token"));
  assert!(!message.contains("private-env"));
}

#[tokio::test]
async fn attached_start_deduplicates_and_close_fences_instances_without_stopping_server() {
  let server = CodexPeer::start(false).await;
  let manager = manager();
  let request = save_profile(&manager, attached_profile(server.endpoint.clone()));
  let (first, second) = tokio::join!(manager.start(request.clone()), manager.start(request.clone()));
  let first = first.unwrap();
  if let Ok(second) = second {
    assert_eq!(first.instance_id, second.instance_id);
  }
  assert_eq!(
    manager.start(request.clone()).await.unwrap().instance_id,
    first.instance_id
  );
  assert_eq!(server.connections.load(Ordering::Relaxed), 1);
  let mut conflicting_receipt = request.clone();
  conflicting_receipt.expected_revision += 1;
  assert!(manager.start(conflicting_receipt).await.is_err());
  assert_eq!(manager.0.interactions.snapshots().len(), 1);
  assert_eq!(first.ownership, Ownership::Attached);
  assert!(!server.methods().iter().any(|method| method == "thread/resume"));

  let mut wrong = close_request(&first);
  wrong.expected_instance_id = uuid::Uuid::new_v4().to_string();
  assert!(manager.close(wrong).await.is_err());
  assert_eq!(manager.inventory().runtimes.len(), 1);
  let close = close_request(&first);
  assert!(manager.close(close.clone()).await.unwrap().closed);
  assert!(manager.close(close).await.unwrap().closed);
  assert!(manager.inventory().runtimes.is_empty());
  assert!(manager.0.interactions.snapshots().is_empty());

  let mut restart = request;
  restart.request_id = uuid::Uuid::new_v4().to_string();
  let restarted = manager.start(restart).await.unwrap();
  assert_ne!(first.instance_id, restarted.instance_id);
  assert_eq!(server.connections.load(Ordering::Relaxed), 2);
  assert!(manager.close(close_request(&first)).await.is_err());
  assert_eq!(manager.inventory().runtimes[0].instance_id, restarted.instance_id);
  manager.shutdown().await;
  assert!(manager.inventory().runtimes.is_empty());
}

#[tokio::test]
async fn cancelled_start_is_still_owned_and_same_receipt_never_reconnects() {
  let server = CodexPeer::start(true).await;
  let manager = manager();
  let request = save_profile(&manager, attached_profile(server.endpoint.clone()));
  let worker = {
    let manager = manager.clone();
    let request = request.clone();
    tokio::spawn(async move { manager.start(request).await })
  };
  wait_until(|| server.methods().iter().any(|method| method == "initialize")).await;
  worker.abort();
  let _ = worker.await;
  server.initialize_gate.add_permits(1);
  wait_until(|| manager.inventory().runtimes.len() == 1).await;
  let replay = manager.start(request).await.unwrap();
  assert_eq!(replay.instance_id, manager.inventory().runtimes[0].instance_id);
  assert_eq!(server.connections.load(Ordering::Relaxed), 1);
  manager.shutdown().await;
}

#[tokio::test]
async fn shutdown_waits_for_cancelled_inflight_start_and_prevents_late_registration() {
  let server = CodexPeer::start(true).await;
  let manager = manager();
  let request = save_profile(&manager, attached_profile(server.endpoint.clone()));
  let worker = {
    let manager = manager.clone();
    let request = request.clone();
    tokio::spawn(async move { manager.start(request).await })
  };
  wait_until(|| server.methods().iter().any(|method| method == "initialize")).await;
  worker.abort();
  let _ = worker.await;
  let shutdown = {
    let manager = manager.clone();
    tokio::spawn(async move { manager.shutdown().await })
  };
  wait_until(|| manager.0.runtimes.is_stopping()).await;
  assert!(
    !shutdown.is_finished(),
    "shutdown returned before its starting runtime was cleaned up"
  );
  server.initialize_gate.add_permits(1);
  tokio::time::timeout(Duration::from_secs(5), shutdown)
    .await
    .unwrap()
    .unwrap();
  assert!(manager.inventory().runtimes.is_empty());
  assert!(manager.0.interactions.snapshots().is_empty());
  assert!(manager.start(request).await.is_err());
  assert_eq!(server.connections.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn profile_edits_require_explicit_reconnect_and_active_profile_cannot_be_deleted() {
  let server = CodexPeer::start(false).await;
  let manager = manager();
  let mut input = attached_profile(server.endpoint.clone());
  let request = save_profile(&manager, input.clone());
  let info = manager.start(request.clone()).await.unwrap();
  assert!(
    manager
      .remove_profile(RpcRemoveProfileRequest {
        expected_revision: 1,
        profile_id: request.profile_id.clone()
      })
      .await
      .is_err()
  );
  input.profile_id = Some(request.profile_id.clone());
  input.name = "Renamed profile".into();
  manager
    .profiles()
    .unwrap()
    .save(RpcSaveProfileRequest {
      expected_revision: 1,
      profile: input,
    })
    .unwrap();
  let mut new = request.clone();
  new.request_id = uuid::Uuid::new_v4().to_string();
  new.expected_revision = 2;
  assert!(manager.start(new.clone()).await.is_err());
  assert_eq!(server.connections.load(Ordering::Relaxed), 1);
  manager.close(close_request(&info)).await.unwrap();
  new.request_id = uuid::Uuid::new_v4().to_string();
  let replaced = manager.start(new).await.unwrap();
  assert_eq!(replaced.name, "Renamed profile");
  assert_eq!(replaced.profile_revision, 2);
  manager.close(close_request(&replaced)).await.unwrap();
  assert!(
    manager
      .remove_profile(RpcRemoveProfileRequest {
        expected_revision: 2,
        profile_id: request.profile_id
      })
      .await
      .unwrap()
      .profiles
      .is_empty()
  );
  manager.shutdown().await;
}

#[tokio::test]
async fn binds_recover_native_state_and_close_retires_the_instance_binding() {
  let server = CodexPeer::start(false).await;
  let manager = manager();
  let request = save_profile(&manager, attached_profile(server.endpoint.clone()));
  let info = manager.start(request.clone()).await.unwrap();
  let bind_request_id = uuid::Uuid::new_v4().to_string();
  let binding = manager
    .bind(
      bind_request_id.clone(),
      "verified-viewer-key".into(),
      request.profile_id.clone(),
      info.instance_id.clone(),
      "native-thread".into(),
      Provider::Codex,
    )
    .await
    .unwrap();
  let replay = manager
    .bind(
      bind_request_id,
      "verified-viewer-key".into(),
      request.profile_id.clone(),
      info.instance_id.clone(),
      "native-thread".into(),
      Provider::Codex,
    )
    .await
    .unwrap();
  assert_eq!(binding.target, replay.target);
  assert_eq!(
    server
      .methods()
      .iter()
      .filter(|method| method.as_str() == "thread/resume")
      .count(),
    1
  );
  assert_eq!(binding.target.session_id.as_deref(), Some("native-thread"));
  assert_eq!(manager.binding("verified-viewer-key").unwrap().target, binding.target);
  assert_eq!(manager.inventory().runtimes[0].session_count, 1);
  assert!(server.methods().iter().any(|method| method == "thread/resume"));
  assert!(server.methods().iter().any(|method| method == "thread/read"));
  let state = manager.0.interactions.snapshots();
  assert!(!state[0].recovery_required);
  assert_eq!(state[0].states[0].session_id.as_deref(), Some("native-thread"));
  assert!(
    manager
      .bind(
        uuid::Uuid::new_v4().to_string(),
        "wrong-provider".into(),
        request.profile_id,
        info.instance_id.clone(),
        "native-thread".into(),
        Provider::Dsh
      )
      .await
      .is_err()
  );
  manager.close(close_request(&info)).await.unwrap();
  assert!(manager.binding("verified-viewer-key").is_none());
  manager.shutdown().await;
}

#[tokio::test]
async fn disconnected_session_owner_stays_reserved_until_its_exact_runtime_is_closed() {
  let first_peer = CodexPeer::start(false).await;
  let second_peer = CodexPeer::start(false).await;
  let manager = manager();
  let first_request = save_profile(&manager, attached_profile(first_peer.endpoint.clone()));
  let first = manager.start(first_request.clone()).await.unwrap();
  manager
    .bind(
      uuid::Uuid::new_v4().to_string(),
      "first-catalog-key".into(),
      first_request.profile_id,
      first.instance_id.clone(),
      "shared-native-thread".into(),
      Provider::Codex,
    )
    .await
    .unwrap();
  let second_request = save_profile(&manager, attached_profile(second_peer.endpoint.clone()));
  let second = manager.start(second_request.clone()).await.unwrap();
  first_peer.stop.send(true).unwrap();
  wait_until(|| {
    manager
      .inventory()
      .runtimes
      .iter()
      .any(|runtime| runtime.instance_id == first.instance_id && runtime.status == "disconnected")
  })
  .await;
  assert!(manager.binding("first-catalog-key").is_none());
  assert!(
    manager
      .bind(
        uuid::Uuid::new_v4().to_string(),
        "second-catalog-key".into(),
        second_request.profile_id.clone(),
        second.instance_id.clone(),
        "shared-native-thread".into(),
        Provider::Codex,
      )
      .await
      .is_err()
  );
  assert!(!second_peer.methods().iter().any(|method| method == "thread/resume"));
  manager.close(close_request(&first)).await.unwrap();
  assert!(
    manager
      .bind(
        uuid::Uuid::new_v4().to_string(),
        "second-catalog-key".into(),
        second_request.profile_id,
        second.instance_id,
        "shared-native-thread".into(),
        Provider::Codex,
      )
      .await
      .is_ok()
  );
  manager.shutdown().await;
}

#[tokio::test]
async fn http_factories_register_query_and_detach_opencode_and_dsh_without_session_mutations() {
  for provider in [Provider::OpenCode, Provider::Dsh] {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().to_string_lossy().into_owned();
    let peer = http_peer::HttpPeer::start(provider, cwd.clone()).await;
    let manager = manager();
    let mut input = attached_profile(peer.endpoint.clone());
    input.provider = provider;
    input.working_directory = Some(cwd);
    let request = save_profile(&manager, input);
    let info = manager.start(request.clone()).await.unwrap();
    assert_eq!(info.provider, provider);
    assert_eq!(info.ownership, Ownership::Attached);
    assert!(info.capabilities.supports(Operation::Metadata));
    assert!(info.capabilities.supports(Operation::Models));
    let result = manager
      .0
      .interactions
      .execute(InteractionRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        target: SessionTarget {
          runtime_id: info.runtime_id.clone(),
          instance_id: info.instance_id.clone(),
          session_id: None,
        },
        command: Command::Models,
      })
      .await
      .unwrap();
    assert!(matches!(result,Reply::Models(models) if models.is_empty()));
    assert_eq!(manager.0.interactions.snapshots().len(), 1);
    assert_eq!(manager.0.interactions.snapshots()[0].descriptor.provider, provider);
    manager.close(close_request(&info)).await.unwrap();
    assert!(manager.0.interactions.snapshots().is_empty());
    assert!(manager.inventory().runtimes.is_empty());
    let routes = peer.requests.lock().unwrap().clone();
    assert!(
      !routes
        .iter()
        .any(|route| route.ends_with("/session") || route.ends_with("session.open") || route.contains("prompt"))
    );
    let mut reconnect = request;
    reconnect.request_id = uuid::Uuid::new_v4().to_string();
    let reconnected = manager.start(reconnect).await.unwrap();
    assert_ne!(reconnected.instance_id, info.instance_id);
    manager.shutdown().await;
  }
}

#[cfg(unix)]
fn managed_pi_profile(directory: &Path) -> RpcRuntimeProfileInput {
  use std::os::unix::fs::PermissionsExt;
  let executable = directory.join("pi-rpc-test.sh");
  std::fs::write(&executable,r#"#!/bin/sh
printf '%s\n' "$$" >> "$TOKN_TEST_PID_FILE"
while IFS= read -r request; do
  request_id=$(printf '%s' "$request" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  request_type=$(printf '%s' "$request" | sed -n 's/.*"type":"\([^"]*\)".*/\1/p')
  case "$request_type" in
    get_state) payload='{"sessionId":"pi-native","sessionName":"Pi test","isStreaming":false,"isCompacting":false,"pendingMessageCount":0}' ;;
    get_commands) payload='{"commands":[]}' ;;
    *) payload='{}' ;;
  esac
  printf '{"type":"response","id":"%s","command":"%s","success":true,"data":%s}\n' "$request_id" "$request_type" "$payload"
done
"#).unwrap();
  std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
  RpcRuntimeProfileInput {
    profile_id: None,
    name: "Owned Pi test".into(),
    provider: Provider::Pi,
    mode: RpcRuntimeMode::Managed,
    executable: Some(executable.to_string_lossy().into()),
    arguments: vec![],
    working_directory: Some(directory.to_string_lossy().into()),
    endpoint: None,
    environment: Some(BTreeMap::from([(
      "TOKN_TEST_PID_FILE".into(),
      directory.join("pids").to_string_lossy().into(),
    )])),
    credentials: None,
    is_default: true,
  }
}

#[cfg(unix)]
#[tokio::test]
async fn managed_factory_holds_cross_process_lease_and_reaps_owned_pi_on_close() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("profiles.json");
  let first = RpcRuntimeManager::new(Some(path.clone()), InteractionService::default());
  let second = RpcRuntimeManager::new(Some(path), InteractionService::default());
  let request = save_profile(&first, managed_pi_profile(directory.path()));
  let info = first.start(request.clone()).await.unwrap();
  assert_eq!(info.ownership, Ownership::Managed);
  wait_until(|| directory.path().join("pids").exists()).await;
  let native_pid = std::fs::read_to_string(directory.path().join("pids"))
    .unwrap()
    .trim()
    .parse::<i32>()
    .unwrap();
  assert_eq!(unsafe { libc::kill(native_pid, 0) }, 0);
  let mut competing = request.clone();
  competing.request_id = uuid::Uuid::new_v4().to_string();
  assert!(
    second
      .remove_profile(RpcRemoveProfileRequest {
        expected_revision: competing.expected_revision,
        profile_id: competing.profile_id.clone(),
      })
      .await
      .is_err()
  );
  assert!(second.start(competing.clone()).await.is_err());
  assert_eq!(
    std::fs::read_to_string(directory.path().join("pids"))
      .unwrap()
      .lines()
      .count(),
    1
  );
  first.close(close_request(&info)).await.unwrap();
  assert_eq!(
    unsafe { libc::kill(native_pid, 0) },
    -1,
    "owned native process was not reaped"
  );
  competing.request_id = uuid::Uuid::new_v4().to_string();
  let replacement = second.start(competing).await.unwrap();
  assert_ne!(info.instance_id, replacement.instance_id);
  second.close(close_request(&replacement)).await.unwrap();
  first.shutdown().await;
  second.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn failed_managed_factory_quarantines_lease_without_a_handle_and_prevents_relaunch() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("profiles.json");
  let first = RpcRuntimeManager::new(Some(path.clone()), InteractionService::default());
  let second = RpcRuntimeManager::new(Some(path), InteractionService::default());
  let mut profile = managed_pi_profile(directory.path());
  profile.provider = Provider::Codex;
  profile.name = "Early-exit Codex test".into();
  std::fs::write(
    profile.executable.as_ref().unwrap(),
    "#!/bin/sh\nprintf '%s\\n' \"$$\" >> \"$TOKN_TEST_PID_FILE\"\nexit 1\n",
  )
  .unwrap();
  let request = save_profile(&first, profile);
  assert!(first.start(request.clone()).await.is_err());
  let info = first.inventory().runtimes[0].clone();
  assert_eq!(info.status, "cleanup_failed");
  assert_eq!(info.provider, Provider::Codex);
  assert_eq!(info.ownership, Ownership::Managed);
  assert_eq!(info.runtime_id, format!("rpc:{}", request.profile_id));
  assert!(first.start(request.clone()).await.is_err());
  assert!(first.close(close_request(&info)).await.is_err());
  assert!(
    first
      .remove_profile(RpcRemoveProfileRequest {
        expected_revision: request.expected_revision,
        profile_id: request.profile_id.clone(),
      })
      .await
      .is_err()
  );
  let mut competing = request;
  competing.request_id = uuid::Uuid::new_v4().to_string();
  assert!(first.start(competing.clone()).await.is_err());
  assert!(second.start(competing.clone()).await.is_err());
  assert_eq!(
    std::fs::read_to_string(directory.path().join("pids"))
      .unwrap()
      .lines()
      .count(),
    1,
    "quarantined launch was attempted again"
  );
  first.shutdown().await;
  competing.request_id = uuid::Uuid::new_v4().to_string();
  assert!(second.start(competing).await.is_err());
  assert_eq!(second.inventory().runtimes[0].status, "cleanup_failed");
  assert_eq!(
    std::fs::read_to_string(directory.path().join("pids"))
      .unwrap()
      .lines()
      .count(),
    2,
    "shutdown did not release the lease for a fresh launch"
  );
  second.shutdown().await;
}
