use super::*;

#[test]
fn disabled_runtime_keeps_read_paths_available() {
  let manager = ManagedPiSessions::disabled("invalid private receipts".into());
  assert!(
    manager
      .get(&ManagedSessionRequest {
        request_id: Some(uuid::Uuid::new_v4().to_string()),
        session_key: None
      })
      .unwrap()
      .is_none()
  );
  assert!(manager.input_status("foreign").is_none());
  assert!(manager.available().unwrap_err().contains("invalid private receipts"));
}

#[cfg(unix)]
mod fake_rpc {
  use super::*;
  use serde_json::{Value, json};
  use std::{os::unix::fs::PermissionsExt, path::Path, time::Duration};

  // This executable only implements the protocol exercised below. It never
  // imports Pi, reads provider configuration, or contacts a model. Its native
  // history consists solely of a synthetic session header in its temporary cwd.
  const FAKE_PI: &str = r#"#!/usr/bin/env python3
import json, os, pathlib, sys

root = pathlib.Path.cwd()
config = json.loads((root / 'fake-config.json').read_text())
session_id = config['session_id']
session_file = root / 'synthetic-history.jsonl'
streaming = False
pending = 0
waiting = None

def record(value):
    with (root / 'fake-log.jsonl').open('a') as log:
        log.write(json.dumps(value) + '\n')

def emit(value):
    print(json.dumps(value), flush=True)

def reply(command, success=True, data=None):
    value = {'type': 'response', 'id': command['id'], 'command': command['type'], 'success': success}
    if data is not None:
        value['data'] = data
    if not success:
        value['error'] = 'synthetic prompt rejection'
    emit(value)

def persist():
    if not session_file.exists():
        session_file.write_text(json.dumps({'type': 'session', 'version': 3, 'id': session_id, 'cwd': str(root), 'timestamp': '2026-01-01T00:00:00.000Z'}) + '\n')

record({'kind': 'launch', 'args': sys.argv[1:], 'pid': os.getpid()})
mode = config['mode']
if mode == 'early_exit':
    sys.stderr.write('synthetic startup failure\n')
    sys.exit(23)
if mode == 'partial':
    sys.stdout.write('{"type":')
    sys.stdout.flush()
    sys.exit(24)
if mode == 'malformed':
    print('invalid-json', flush=True)

for line in sys.stdin:
    command = json.loads(line)
    record(command)
    kind = command['type']
    if kind == 'get_state':
        if mode == 'deferred' and (root / 'allow-persist').exists():
            persist()
        reply(command, data={'sessionId': session_id, 'sessionFile': str(session_file), 'isStreaming': streaming, 'isCompacting': False, 'pendingMessageCount': pending})
    elif kind == 'prompt':
        text = command['message']
        if text == 'exit-before-ack':
            sys.exit(25)
        if text == 'reject-follow-up':
            reply(command, success=False)
            continue
        if text.startswith('dialog:'):
            method = text.split(':', 1)[1]
            request = {'type': 'extension_ui_request', 'id': 'synthetic-dialog', 'method': method, 'title': 'Synthetic confirmation', 'message': 'Allow this synthetic operation?'}
            if method == 'select':
                request['options'] = ['Yes', 'No']
            if method == 'timeout':
                request['method'] = 'confirm'
                request['timeout'] = 25
            waiting = command
            emit(request)
            continue
        if mode not in ('deferred', 'provisional'):
            persist()
        if text == 'handled':
            reply(command)
            continue
        if streaming:
            pending += 1
        streaming = True
        reply(command)
        emit({'type': 'agent_start'})
    elif kind == 'extension_ui_response':
        if waiting is not None:
            persist()
            reply(waiting)
            waiting = None
    elif kind == 'abort':
        streaming = False
        pending = 0
        emit({'type': 'agent_settled'})
        reply(command)
record({'kind': 'stdin_closed'})
"#;

  struct Fixture {
    manager: ManagedPiSessions,
    directory: tempfile::TempDir,
    executable: PathBuf,
    state_path: PathBuf,
    project: RegisteredProject,
    request: CreateProjectSessionRequest,
    session_id: String,
  }

  impl Fixture {
    fn new(mode: &str, prompt: &str) -> Self {
      let directory = tempfile::tempdir().unwrap();
      let folder = directory.path().join("project");
      std::fs::create_dir(&folder).unwrap();
      let folder = folder.canonicalize().unwrap();
      let executable = directory.path().join("fake-pi");
      std::fs::write(&executable, FAKE_PI).unwrap();
      std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
      let session_id = uuid::Uuid::new_v4().to_string();
      std::fs::write(
        folder.join("fake-config.json"),
        serde_json::to_vec(&json!({"mode": mode, "session_id": session_id})).unwrap(),
      )
      .unwrap();
      let state_path = directory.path().join("managed.json");
      let manager = ManagedPiSessions::with_executable(Some(state_path.clone()), executable.clone()).unwrap();
      let project_id = uuid::Uuid::new_v4().to_string();
      let project = RegisteredProject {
        project_id: project_id.clone(),
        group_key: "synthetic-project".into(),
        folder_path: folder.to_str().unwrap().into(),
        name: "Synthetic project".into(),
        created_at_ms: 0,
        available: true,
      };
      let request = CreateProjectSessionRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        project_id,
        prompt: prompt.into(),
      };
      Self {
        manager,
        directory,
        executable,
        state_path,
        project,
        request,
        session_id,
      }
    }

    fn identity(&self) -> ManagedSessionRequest {
      ManagedSessionRequest {
        request_id: Some(self.request.request_id.clone()),
        session_key: None,
      }
    }

    fn snapshot(&self) -> ManagedSessionSnapshot {
      self.manager.get(&self.identity()).unwrap().unwrap()
    }

    fn folder(&self) -> &Path {
      Path::new(&self.project.folder_path)
    }

    fn log(&self) -> Vec<Value> {
      std::fs::read_to_string(self.folder().join("fake-log.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
    }

    async fn create(&self) {
      self
        .manager
        .create(self.request.clone(), self.project.clone())
        .await
        .unwrap();
    }

    async fn wait_for(&self, predicate: impl Fn(&ManagedSessionSnapshot) -> bool) -> ManagedSessionSnapshot {
      tokio::time::timeout(Duration::from_secs(8), async {
        loop {
          let snapshot = self.snapshot();
          if predicate(&snapshot) {
            return snapshot;
          }
          tokio::time::sleep(Duration::from_millis(10)).await;
        }
      })
      .await
      .unwrap_or_else(|_| {
        panic!(
          "timed out waiting for managed state: {:?}; fake log: {:?}",
          self.snapshot(),
          self.log()
        )
      })
    }

    async fn wait_log(&self, predicate: impl Fn(&Value) -> bool) -> Value {
      tokio::time::timeout(Duration::from_secs(8), async {
        loop {
          if let Some(value) = self.log().into_iter().find(&predicate) {
            return value;
          }
          tokio::time::sleep(Duration::from_millis(10)).await;
        }
      })
      .await
      .unwrap_or_else(|_| panic!("timed out waiting for fake RPC: {:?}", self.log()))
    }

    async fn wait_reaped(&self) {
      let launch = self.wait_log(|value| value["kind"] == "launch").await;
      let pid = launch["pid"].as_i64().unwrap() as libc::pid_t;
      tokio::time::timeout(Duration::from_secs(5), async {
        loop {
          // Signal zero only checks existence; it sends no signal to the child.
          if unsafe { libc::kill(pid, 0) } == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
          {
            return;
          }
          tokio::time::sleep(Duration::from_millis(10)).await;
        }
      })
      .await
      .expect("fake Pi child was not reaped");
    }

    async fn persisted(&self) -> ManagedSessionSnapshot {
      self
        .wait_for(|snapshot| snapshot.session_key.is_some() && snapshot.status == ManagedSessionStatus::Running)
        .await
    }
  }

  #[tokio::test]
  async fn duplicate_creation_launches_once_and_rejects_changed_content() {
    let fixture = Fixture::new("normal", "synthetic initial prompt");
    let (first, duplicate) = tokio::join!(
      fixture.manager.create(fixture.request.clone(), fixture.project.clone()),
      fixture.manager.create(fixture.request.clone(), fixture.project.clone())
    );
    assert_eq!(first.unwrap().request_id, duplicate.unwrap().request_id);
    let persisted = fixture.persisted().await;
    assert_eq!(persisted.session_id.as_deref(), Some(fixture.session_id.as_str()));
    let mut changed = fixture.request.clone();
    changed.prompt = "different synthetic prompt".into();
    assert!(
      fixture
        .manager
        .create(changed, fixture.project.clone())
        .await
        .unwrap_err()
        .contains("different content")
    );
    assert_eq!(
      fixture.log().iter().filter(|value| value["kind"] == "launch").count(),
      1
    );
    assert_eq!(
      fixture.log().iter().filter(|value| value["type"] == "prompt").count(),
      1
    );
    fixture.manager.shutdown().await;
    fixture.wait_reaped().await;
  }

  #[tokio::test]
  async fn accepted_creation_receipt_survives_folder_removal() {
    let fixture = Fixture::new("normal", "synthetic initial prompt");
    fixture.create().await;
    let persisted = fixture.persisted().await;
    fixture.manager.shutdown().await;
    std::fs::remove_dir_all(fixture.folder()).unwrap();
    let restored =
      ManagedPiSessions::with_executable(Some(fixture.state_path.clone()), fixture.executable.clone()).unwrap();
    let receipt = restored.creation_receipt(&fixture.request).unwrap().unwrap();
    assert_eq!(receipt.session_key, persisted.session_key);
    assert_eq!(receipt.status, ManagedSessionStatus::Stopped);
    let mut unavailable = fixture.project.clone();
    unavailable.available = false;
    assert_eq!(
      restored
        .create(fixture.request.clone(), unavailable)
        .await
        .unwrap()
        .session_key,
      persisted.session_key
    );
    let mut changed = fixture.request.clone();
    changed.prompt = "different content".into();
    assert!(
      restored
        .creation_receipt(&changed)
        .unwrap_err()
        .contains("different content")
    );
  }

  #[tokio::test]
  async fn a_later_owner_resumes_from_the_latest_durable_revision() {
    let fixture = Fixture::new("normal", "synthetic initial prompt");
    fixture.create().await;
    let persisted = fixture.persisted().await;
    let later =
      ManagedPiSessions::with_executable(Some(fixture.state_path.clone()), fixture.executable.clone()).unwrap();
    let cached = later.get(&fixture.identity()).unwrap().unwrap();
    assert!(cached.revision <= persisted.revision);
    fixture.manager.stop(fixture.identity()).await.unwrap();
    fixture
      .wait_for(|snapshot| snapshot.status == ManagedSessionStatus::Idle)
      .await;
    fixture.manager.shutdown().await;
    let previous_revision = fixture.snapshot().revision;
    assert!(previous_revision > cached.revision);
    let resumed = later.resume(fixture.identity()).await.unwrap();
    assert!(resumed.revision > previous_revision);
    assert_eq!(resumed.session_key, persisted.session_key);
    let durable = later.inner.store.latest(&fixture.request.request_id).unwrap().unwrap();
    assert!(durable.revision >= resumed.revision);
    later.shutdown().await;
  }

  #[tokio::test]
  async fn preflight_dialog_outlives_the_short_transport_deadline() {
    let fixture = Fixture::new("normal", "dialog:confirm");
    fixture.create().await;
    fixture.wait_for(|snapshot| snapshot.pending_dialog.is_some()).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(16)).await;
    tokio::task::yield_now().await;
    assert!(fixture.snapshot().pending_dialog.is_some());
    assert_ne!(fixture.snapshot().status, ManagedSessionStatus::Failed);
    tokio::time::resume();
    fixture
      .manager
      .respond_dialog(ManagedSessionDialogResponse {
        request_id: fixture.request.request_id.clone(),
        dialog_id: "synthetic-dialog".into(),
        value: None,
        confirmed: Some(false),
        cancelled: false,
      })
      .await
      .unwrap();
    fixture
      .wait_for(|snapshot| snapshot.session_key.is_some() && snapshot.pending_dialog.is_none())
      .await;
    fixture.manager.shutdown().await;
  }

  #[tokio::test]
  async fn planned_native_path_is_not_exposed_until_a_valid_header_exists() {
    let fixture = Fixture::new("deferred", "synthetic deferred prompt");
    fixture.create().await;
    fixture.wait_log(|value| value["type"] == "prompt").await;
    let provisional = fixture.snapshot();
    assert!(provisional.session_key.is_none());
    assert!(provisional.session_id.is_none());
    assert!(!fixture.folder().join("synthetic-history.jsonl").exists());
    std::fs::write(fixture.folder().join("allow-persist"), b"ready").unwrap();
    let persisted = fixture.persisted().await;
    assert_eq!(persisted.session_id.as_deref(), Some(fixture.session_id.as_str()));
    fixture.manager.shutdown().await;
  }

  #[tokio::test]
  async fn messages_keep_native_identity_queue_when_busy_and_deduplicate() {
    let fixture = Fixture::new("normal", "synthetic initial prompt");
    fixture.create().await;
    let persisted = fixture.persisted().await;
    let key = persisted.session_key.unwrap();
    let message_id = uuid::Uuid::new_v4().to_string();
    assert!(matches!(
      fixture
        .manager
        .submit_owned_input(&key, &message_id, "synthetic follow-up")
        .await
        .unwrap(),
      Admission::QueuedFollowUp
    ));
    assert!(matches!(
      fixture
        .manager
        .submit_owned_input(&key, &message_id, "synthetic follow-up")
        .await
        .unwrap(),
      Admission::QueuedFollowUp
    ));
    let mismatch = fixture
      .manager
      .submit_owned_input(&key, &message_id, "different follow-up")
      .await
      .unwrap_err();
    assert!(!mismatch.may_have_been_sent);
    let command = fixture
      .wait_log(|value| value["message"] == "synthetic follow-up")
      .await;
    assert_eq!(command["streamingBehavior"], "followUp");
    assert_eq!(
      fixture
        .log()
        .iter()
        .filter(|value| value["message"] == "synthetic follow-up")
        .count(),
      1
    );
    assert_eq!(fixture.snapshot().session_key.as_deref(), Some(key.as_str()));
    fixture.manager.stop(fixture.identity()).await.unwrap();
    fixture
      .wait_for(|snapshot| snapshot.status == ManagedSessionStatus::Idle)
      .await;
    assert!(matches!(
      fixture
        .manager
        .submit_owned_input(&key, &uuid::Uuid::new_v4().to_string(), "synthetic next turn")
        .await
        .unwrap(),
      Admission::Started
    ));
    assert_eq!(fixture.snapshot().session_key.as_deref(), Some(key.as_str()));
    assert_eq!(
      fixture.log().iter().filter(|value| value["kind"] == "launch").count(),
      1
    );
    fixture.manager.shutdown().await;
  }

  #[tokio::test]
  async fn stop_persisted_turn_keeps_owner_idle_but_stop_provisional_reaps_it() {
    let persisted = Fixture::new("normal", "synthetic running prompt");
    persisted.create().await;
    let key = persisted.persisted().await.session_key.unwrap();
    persisted.manager.stop(persisted.identity()).await.unwrap();
    persisted
      .wait_for(|snapshot| snapshot.status == ManagedSessionStatus::Idle)
      .await;
    assert!(persisted.manager.input_status(&key).unwrap().is_ok());
    assert!(
      persisted.manager.inner.records.lock().unwrap()[&persisted.request.request_id]
        .sender
        .is_some()
    );
    persisted.manager.shutdown().await;
    persisted.wait_reaped().await;

    let provisional = Fixture::new("provisional", "synthetic provisional prompt");
    provisional.create().await;
    provisional.wait_log(|value| value["type"] == "prompt").await;
    let stopped = provisional.manager.stop(provisional.identity()).await.unwrap();
    assert_eq!(stopped.status, ManagedSessionStatus::Stopped);
    assert!(stopped.session_key.is_none());
    assert!(
      provisional.manager.inner.records.lock().unwrap()[&provisional.request.request_id]
        .sender
        .is_none()
    );
    provisional.wait_reaped().await;
    assert!(
      provisional
        .manager
        .resume(provisional.identity())
        .await
        .unwrap_err()
        .contains("not persisted")
    );
    provisional.manager.shutdown().await;
  }

  #[tokio::test]
  async fn early_exit_invalid_json_and_partial_frame_fail_and_reap_the_child() {
    for mode in ["early_exit", "malformed", "partial"] {
      let fixture = Fixture::new(mode, "synthetic initial prompt");
      fixture.create().await;
      let failed = fixture
        .wait_for(|snapshot| snapshot.status == ManagedSessionStatus::Failed)
        .await;
      assert!(failed.message.is_some(), "missing failure detail for {mode}");
      assert!(failed.session_key.is_none());
      fixture.wait_reaped().await;
      fixture.manager.shutdown().await;
    }
  }

  #[tokio::test]
  async fn exit_after_writing_a_message_is_uncertain_and_never_replayed() {
    let fixture = Fixture::new("normal", "synthetic initial prompt");
    fixture.create().await;
    let key = fixture.persisted().await.session_key.unwrap();
    let delivery = fixture
      .manager
      .submit_owned_input(&key, &uuid::Uuid::new_v4().to_string(), "exit-before-ack")
      .await
      .unwrap_err();
    assert!(delivery.may_have_been_sent);
    fixture
      .wait_for(|snapshot| snapshot.status == ManagedSessionStatus::Failed)
      .await;
    fixture.wait_reaped().await;
    assert_eq!(
      fixture
        .log()
        .iter()
        .filter(|value| value["message"] == "exit-before-ack")
        .count(),
      1
    );
    assert_eq!(
      fixture.log().iter().filter(|value| value["kind"] == "launch").count(),
      1
    );
    fixture.manager.shutdown().await;
  }

  #[tokio::test]
  async fn restart_resumes_exact_owned_file_without_replaying_initial_prompt() {
    let mut fixture = Fixture::new("normal", "synthetic initial prompt");
    fixture.create().await;
    let key = fixture.persisted().await.session_key.unwrap();
    fixture.manager.shutdown().await;
    fixture.wait_reaped().await;
    fixture.manager =
      ManagedPiSessions::with_executable(Some(fixture.state_path.clone()), fixture.executable.clone()).unwrap();
    assert_eq!(fixture.snapshot().status, ManagedSessionStatus::Stopped);
    assert_eq!(
      fixture.log().iter().filter(|value| value["kind"] == "launch").count(),
      1
    );
    fixture.manager.resume(fixture.identity()).await.unwrap();
    fixture
      .wait_for(|snapshot| snapshot.status == ManagedSessionStatus::Idle)
      .await;
    let launches: Vec<_> = fixture
      .log()
      .into_iter()
      .filter(|value| value["kind"] == "launch")
      .collect();
    assert_eq!(launches.len(), 2);
    assert_eq!(
      launches[1]["args"],
      json!([
        "--mode",
        "rpc",
        "--session",
        fixture.folder().join("synthetic-history.jsonl").to_str().unwrap()
      ])
    );
    assert_eq!(
      fixture.log().iter().filter(|value| value["type"] == "prompt").count(),
      1
    );
    assert_eq!(fixture.snapshot().session_key.as_deref(), Some(key.as_str()));
    fixture.manager.shutdown().await;
    assert!(fixture.directory.path().exists());
  }

  #[tokio::test]
  async fn dialogs_validate_answers_and_expire_or_cancel_unsupported_requests() {
    let confirm = Fixture::new("normal", "dialog:confirm");
    confirm.create().await;
    confirm.wait_for(|snapshot| snapshot.pending_dialog.is_some()).await;
    confirm
      .manager
      .respond_dialog(ManagedSessionDialogResponse {
        request_id: confirm.request.request_id.clone(),
        dialog_id: "synthetic-dialog".into(),
        value: None,
        confirmed: Some(true),
        cancelled: false,
      })
      .await
      .unwrap();
    let response = confirm.wait_log(|value| value["type"] == "extension_ui_response").await;
    assert_eq!(response["confirmed"], true);
    confirm
      .wait_for(|snapshot| snapshot.status == ManagedSessionStatus::Idle && snapshot.pending_dialog.is_none())
      .await;
    confirm.manager.shutdown().await;

    let select = Fixture::new("normal", "dialog:select");
    select.create().await;
    select.wait_for(|snapshot| snapshot.pending_dialog.is_some()).await;
    let mut response = ManagedSessionDialogResponse {
      request_id: select.request.request_id.clone(),
      dialog_id: "synthetic-dialog".into(),
      value: Some("unlisted".into()),
      confirmed: None,
      cancelled: false,
    };
    assert!(
      select
        .manager
        .respond_dialog(response.clone())
        .await
        .unwrap_err()
        .contains("available option")
    );
    assert!(select.snapshot().pending_dialog.is_some());
    response.cancelled = true;
    select.manager.respond_dialog(response).await.unwrap();
    assert_eq!(
      select.wait_log(|value| value["type"] == "extension_ui_response").await["cancelled"],
      true
    );
    select.manager.shutdown().await;

    for method in ["timeout", "custom"] {
      let fixture = Fixture::new("normal", &format!("dialog:{method}"));
      fixture.create().await;
      let response = fixture.wait_log(|value| value["type"] == "extension_ui_response").await;
      assert_eq!(response["cancelled"], true);
      fixture
        .wait_for(|snapshot| snapshot.status == ManagedSessionStatus::Idle && snapshot.pending_dialog.is_none())
        .await;
      fixture.manager.shutdown().await;
    }
  }

  #[tokio::test]
  async fn handled_prompt_settles_without_agent_events_and_rejected_follow_up_stays_running() {
    let handled = Fixture::new("normal", "handled");
    handled.create().await;
    handled
      .wait_for(|snapshot| snapshot.status == ManagedSessionStatus::Idle && snapshot.session_key.is_some())
      .await;
    handled.manager.shutdown().await;

    let fixture = Fixture::new("normal", "synthetic initial prompt");
    fixture.create().await;
    let key = fixture.persisted().await.session_key.unwrap();
    let error = fixture
      .manager
      .submit_owned_input(&key, &uuid::Uuid::new_v4().to_string(), "reject-follow-up")
      .await
      .unwrap_err();
    assert!(!error.may_have_been_sent);
    fixture
      .wait_for(|snapshot| snapshot.status == ManagedSessionStatus::Running && snapshot.message.is_some())
      .await;
    fixture.manager.shutdown().await;
  }
}
