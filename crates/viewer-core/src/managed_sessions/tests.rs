use super::*;

#[cfg(unix)]
#[tokio::test]
async fn shared_supervisor_can_cancel_a_prompt_waiting_for_extension_input() {
  use crate::{
    input::InputTarget,
    model::SubmitSessionInputRequest,
    session_supervisor::{SessionSupervisor, SessionTransport, VerifiedSession},
    workspace::ManagedSessionStatus,
  };
  use std::os::unix::fs::PermissionsExt;
  let directory = tempfile::tempdir().unwrap();
  let folder = directory.path().canonicalize().unwrap();
  let executable = folder.join("fake-pi");
  std::fs::write(&executable, r#"#!/usr/bin/env python3
import json, pathlib, sys
root = pathlib.Path.cwd()
history = root / 'history.jsonl'
pending = None
for line in sys.stdin:
    command = json.loads(line)
    kind = command['type']
    with (root / 'commands.jsonl').open('a') as log:
        log.write(json.dumps(command) + '\n')
    if kind == 'extension_ui_response':
        if pending is not None:
            print(json.dumps({'type':'response', 'id':pending['id'], 'command':'prompt', 'success':True}), flush=True)
            pending = None
        print(json.dumps({'type':'agent_settled'}), flush=True)
        continue
    if kind == 'prompt':
        if not history.exists():
            history.write_text(json.dumps({'type':'session', 'version':3, 'id':'owned-pi', 'cwd':str(root)}) + '\n')
        if command['message'] == 'dialog:confirm':
            pending = command
            print(json.dumps({'type':'extension_ui_request', 'id':'confirm', 'method':'confirm', 'title':'Confirm', 'message':'Continue?'}), flush=True)
            continue
    response = {'type':'response', 'id':command['id'], 'command':kind, 'success':True}
    if kind == 'get_state':
        response['data'] = {'sessionId':'owned-pi', 'sessionFile':str(history), 'isStreaming':False, 'isCompacting':False, 'pendingMessageCount':0}
    print(json.dumps(response), flush=True)
"#).unwrap();
  std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
  let manager = ManagedSessions::with_test_executable(Some(folder.join("receipts.json")), executable).unwrap();
  let request_id = uuid::Uuid::new_v4().to_string();
  manager
    .create(
      CreateProjectSessionRequest {
        provider: tokn_interaction_core::Provider::Pi,
        runtime_profile_id: None,
        runtime_revision: None,
        model: None,
        request_id: request_id.clone(),
        project_id: "project".into(),
        prompt: "handled".into(),
        image_ids: Vec::new(),
      },
      RegisteredProject {
        project_id: "project".into(),
        group_key: "project".into(),
        folder_path: folder.to_string_lossy().into_owned(),
        name: "Project".into(),
        created_at_ms: 0,
        available: true,
        origin: crate::workspace::ProjectOrigin::Manual,
        discovery_state: crate::workspace::ProjectDiscoveryState::Absent,
        unavailable_reason: None,
      },
    )
    .await
    .unwrap();
  let identity = ManagedSessionRequest {
    request_id: Some(request_id),
    session_key: None,
  };
  let key = tokio::time::timeout(Duration::from_secs(8), async {
    loop {
      let snapshot = manager.get(&identity).unwrap().unwrap();
      if snapshot.status == ManagedSessionStatus::Idle {
        if let Some(key) = snapshot.session_key {
          break key;
        }
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .unwrap();
  let mut sessions = SessionSupervisor::new(None, tokn_interaction_core::InteractionService::default()).unwrap();
  sessions.managed = manager.clone();
  let target = || VerifiedSession {
    session_key: key.clone(),
    input: InputTarget {
      provider: ViewerProvider::Pi,
      session_id: "owned-pi".into(),
      session_file: folder.join("history.jsonl"),
    },
    cwd: folder.clone(),
  };
  let request = SubmitSessionInputRequest {
    request_id: uuid::Uuid::new_v4().to_string(),
    session_key: key.clone(),
    text: "dialog:confirm".into(),
    image_ids: Vec::new(),
    model_selection: None,
  };
  let delivery = sessions.clone();
  let delivery_target = target();
  let submission = tokio::spawn(async move {
    delivery
      .submit(delivery_target, request, Vec::new(), SessionTransport::Rpc)
      .await
  });
  tokio::time::timeout(Duration::from_secs(8), async {
    loop {
      if manager.get(&identity).unwrap().unwrap().pending_dialog.is_some() {
        break;
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .unwrap();
  tokio::time::timeout(
    Duration::from_secs(2),
    sessions.cancel(&target(), uuid::Uuid::new_v4().to_string()),
  )
  .await
  .expect("Stop must not wait behind the prompt needing extension input")
  .unwrap();
  let _outcome = tokio::time::timeout(Duration::from_secs(2), submission)
    .await
    .unwrap()
    .unwrap();
  sessions.shutdown().await;
  let commands = std::fs::read_to_string(folder.join("commands.jsonl")).unwrap();
  assert!(
    commands
      .lines()
      .any(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["type"] == "abort")
  );
}
