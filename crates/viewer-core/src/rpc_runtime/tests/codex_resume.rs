use super::*;
use crate::model::{SessionLocator, ViewerProvider, encode_session_key};
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn managed_codex_uses_verified_rollout_when_native_id_lookup_is_missing() {
  exercise_managed_resume(None).await;
}

#[tokio::test]
async fn paginated_codex_requires_its_original_runtime_home() {
  exercise_managed_resume(Some("paginated")).await;
}

async fn exercise_managed_resume(history_mode: Option<&str>) {
  let temporary = tempfile::tempdir().unwrap();
  let directory = temporary.path().canonicalize().unwrap();
  let executable = directory.join("codex-peer.sh");
  let request_log = directory.join("requests.jsonl");
  let history = directory.join("rollout-fixture.jsonl");
  let native_id = "fixture-thread";
  let mut header = json!({
    "timestamp": "2026-10-01T12:00:00Z",
    "type": "session_meta",
    "payload": {"id": native_id, "cwd": directory},
  });
  if let Some(mode) = history_mode {
    header["payload"]["history_mode"] = json!(mode);
  }
  std::fs::write(&history, format!("{header}\n")).unwrap();
  std::fs::write(
    &executable,
    r#"#!/bin/sh
while IFS= read -r request; do
  printf '%s\n' "$request" >> "$TOKN_TEST_REQUESTS"
  request_id=$(printf '%s' "$request" | sed -n 's/.*"id":\("[^"]*"\).*/\1/p')
  case "$request" in
    *'"method":"initialize"'*) payload='"result":{"userAgent":"tokn-test"}' ;;
    *'"method":"thread/resume"'*'"path":'*) payload="$TOKN_TEST_THREAD" ;;
    *'"method":"thread/resume"'*) payload='"error":{"code":-32600,"message":"no rollout found for thread id fixture-thread"}' ;;
    *'"method":"thread/read"'*) payload="$TOKN_TEST_THREAD" ;;
    *) continue ;;
  esac
  printf '{"id":%s,%s}\n' "$request_id" "$payload"
done
"#,
  )
  .unwrap();
  std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
  let thread = json!({
    "thread": {"id": native_id, "path": history, "status":{"type":"idle"}, "turns":[]},
    "model": "fixture-model",
    "modelProvider": "fixture",
  });
  let manager = manager();
  let request = save_profile(
    &manager,
    RpcRuntimeProfileInput {
      profile_id: None,
      name: "Managed Codex resume fixture".into(),
      provider: Provider::Codex,
      mode: RpcRuntimeMode::Managed,
      executable: Some(executable.to_str().unwrap().into()),
      arguments: Vec::new(),
      working_directory: Some(directory.to_str().unwrap().into()),
      endpoint: None,
      environment: Some(BTreeMap::from([
        ("TOKN_TEST_REQUESTS".into(), request_log.to_str().unwrap().into()),
        ("TOKN_TEST_THREAD".into(), format!("\"result\":{thread}")),
      ])),
      credentials: None,
      is_default: true,
    },
  );
  let info = manager.start(request.clone()).await.unwrap();
  let key = encode_session_key(&SessionLocator {
    version: 1,
    provider: ViewerProvider::Codex,
    session_id: native_id.into(),
    source_path: history.clone(),
  })
  .unwrap();
  let binding = manager
    .bind(
      uuid::Uuid::new_v4().to_string(),
      key,
      request.profile_id,
      info.instance_id,
      native_id.into(),
      Provider::Codex,
    )
    .await;
  let requests: Vec<Value> = std::fs::read_to_string(request_log)
    .unwrap()
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect();
  let resumes: Vec<_> = requests
    .iter()
    .filter(|request| request["method"] == "thread/resume")
    .collect();
  if history_mode == Some("paginated") {
    assert!(binding.unwrap_err().contains("original CODEX_HOME"));
    assert_eq!(resumes.len(), 1);
  } else {
    assert_eq!(binding.unwrap().target.session_id.as_deref(), Some(native_id));
    assert_eq!(resumes.len(), 2);
    assert_eq!(resumes[1]["params"]["path"], history.to_str().unwrap());
    assert_eq!(resumes[1]["params"]["threadId"], native_id);
  }
  assert!(resumes[0]["params"].get("path").is_none());
  assert!(requests.iter().all(|request| request["method"] != "turn/start"));
  manager.shutdown().await;
}
