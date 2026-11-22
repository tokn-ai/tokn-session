use super::*;

fn reviewer(calls: Arc<AtomicUsize>) -> crate::CodexHumanCommandReviewer {
  Arc::new(move |_| {
    calls.fetch_add(1, Ordering::SeqCst);
    Box::pin(async { crate::CodexHumanCommandDecision::Decline })
  })
}

#[cfg(unix)]
fn managed_profile(directory: &Path) -> RpcRuntimeProfileInput {
  use std::os::unix::fs::PermissionsExt;
  let executable = directory.join("codex-peer.sh");
  std::fs::write(
    &executable,
    r#"#!/bin/sh
while IFS= read -r request; do
  printf '%s\n' "$request" >> "$TOKN_TEST_REQUESTS"
  request_id=$(printf '%s' "$request" | sed -n 's/.*"id":\("[^"]*"\).*/\1/p')
  case "$request" in
    *'"method":"initialize"'*) payload='{"userAgent":"viewer-test-codex"}' ;;
    *'"method":"model/list"'*) payload='{"data":[],"nextCursor":null}' ;;
    *) continue ;;
  esac
  printf '{"id":%s,"result":%s}\n' "$request_id" "$payload"
done
"#,
  )
  .unwrap();
  std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
  RpcRuntimeProfileInput {
    profile_id: None,
    name: "Managed Codex review fixture".into(),
    provider: Provider::Codex,
    mode: RpcRuntimeMode::Managed,
    executable: Some(executable.to_string_lossy().into()),
    arguments: Vec::new(),
    working_directory: Some(directory.to_string_lossy().into()),
    endpoint: None,
    environment: Some(BTreeMap::from([(
      "TOKN_TEST_REQUESTS".into(),
      directory.join("requests.jsonl").to_string_lossy().into(),
    )])),
    credentials: None,
    is_default: true,
  }
}

#[cfg(unix)]
#[tokio::test]
async fn human_review_is_opt_in_for_managed_codex_and_absent_from_metadata_processes() {
  for opted_in in [false, true] {
    let directory = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let index = directory.path().join("index.sqlite");
    let service = if opted_in {
      crate::ViewerService::native_with_codex_human_command_reviewer(index, reviewer(calls.clone())).unwrap()
    } else {
      crate::ViewerService::native(index).unwrap()
    };
    let manager = &service.sessions.runtimes;
    let request = save_profile(manager, managed_profile(directory.path()));
    let selection = core_runtime::RuntimeSelection {
      profile_id: request.profile_id.clone(),
      expected_revision: request.expected_revision,
    };
    assert!(
      manager
        .creation_models(Provider::Codex, directory.path().to_path_buf(), Some(selection))
        .await
        .unwrap()
        .is_empty()
    );
    assert!(manager.inventory().runtimes.is_empty());
    let info = manager.start(request).await.unwrap();
    assert_eq!(info.ownership, Ownership::Managed);
    let requests: Vec<Value> = std::fs::read_to_string(directory.path().join("requests.jsonl"))
      .unwrap()
      .lines()
      .map(|line| serde_json::from_str(line).unwrap())
      .collect();
    let initializations: Vec<_> = requests
      .iter()
      .filter(|request| request["method"] == "initialize")
      .collect();
    assert_eq!(initializations.len(), 2);
    assert_ne!(
      initializations[0]["params"]["capabilities"]["requiredHumanApproval"],
      true
    );
    assert_eq!(
      initializations[1]["params"]["capabilities"]["requiredHumanApproval"] == true,
      opted_in
    );
    assert!(requests.iter().all(|request| !matches!(
      request["method"].as_str(),
      Some("thread/start" | "thread/resume" | "turn/start")
    )));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    manager.shutdown().await;
  }
}

#[tokio::test]
async fn human_review_opt_in_never_reaches_an_attached_codex_runtime() {
  let directory = tempfile::tempdir().unwrap();
  let calls = Arc::new(AtomicUsize::new(0));
  let service = crate::ViewerService::native_with_codex_human_command_reviewer(
    directory.path().join("index.sqlite"),
    reviewer(calls.clone()),
  )
  .unwrap();
  let peer = CodexPeer::start(false).await;
  let manager = &service.sessions.runtimes;
  let request = save_profile(manager, attached_profile(peer.endpoint.clone()));
  let info = manager.start(request).await.unwrap();
  assert_eq!(info.ownership, Ownership::Attached);
  let requests = peer.requests.lock().unwrap().clone();
  let initialize = requests
    .iter()
    .find(|request| request["method"] == "initialize")
    .unwrap();
  assert_ne!(initialize["params"]["capabilities"]["requiredHumanApproval"], true);
  assert_eq!(calls.load(Ordering::SeqCst), 0);
  manager.shutdown().await;
}
