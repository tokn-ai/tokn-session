//! The owned-session supervisor keeps workflow policy while the adapter owns
//! native IO. These tests exercise that boundary independently of viewer state.
use super::*;
use tokio::io::{AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};

struct Peer {
  reader: framing::RpcReader<ReadHalf<DuplexStream>>,
  writer: WriteHalf<DuplexStream>,
}

impl Peer {
  async fn next(&mut self) -> Value {
    tokio::time::timeout(Duration::from_secs(2), self.reader.read_frame())
      .await
      .expect("supervised command was not written")
      .unwrap()
      .expect("supervised input closed unexpectedly")
  }

  async fn emit(&mut self, value: Value) {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    self.writer.write_all(&bytes).await.unwrap();
  }
}

fn attached() -> (SupervisedSession, Peer) {
  let (client, peer) = tokio::io::duplex(128 * 1024);
  let (reader, writer) = tokio::io::split(client);
  let (peer_reader, peer_writer) = tokio::io::split(peer);
  let session = PiAdapter::attach_supervised(EndpointOptions::new("supervised-test"), reader, writer).unwrap();
  (
    session,
    Peer {
      reader: framing::RpcReader::new(peer_reader),
      writer: peer_writer,
    },
  )
}

async fn next(session: &mut SupervisedSession) -> Result<Value, String> {
  tokio::time::timeout(Duration::from_secs(2), session.recv())
    .await
    .expect("supervised event was not delivered")
    .expect("supervised output unexpectedly closed")
}

#[tokio::test]
async fn supervised_keeps_request_ids_and_delivers_correlated_responses_in_native_order() {
  let (mut session, mut peer) = attached();
  session.send("one".into(), wire::Command::GetState).await.unwrap();
  assert!(session.send("one".into(), wire::Command::Abort).await.is_err());
  session.send("two".into(), wire::Command::GetCommands).await.unwrap();
  assert_eq!(peer.next().await, json!({"type":"get_state","id":"one"}));
  assert_eq!(peer.next().await, json!({"type":"get_commands","id":"two"}));
  peer
    .emit(json!({"type":"response","id":"two","command":"get_commands","success":true,"data":{"commands":[]}}))
    .await;
  peer.emit(json!({"type":"agent_start","private":"discard"})).await;
  peer
    .emit(json!({"type":"response","id":"one","command":"get_state","success":false,"error":"rejected"}))
    .await;
  assert_eq!(next(&mut session).await.unwrap()["id"], "two");
  assert_eq!(next(&mut session).await.unwrap(), json!({"type":"agent_start"}));
  let rejected = next(&mut session).await.unwrap();
  assert_eq!(rejected["id"], "one");
  assert_eq!(rejected["success"], false);
  assert_eq!(rejected["error"], "rejected");
  session.close().await.unwrap();
}

#[tokio::test]
async fn supervised_bounded_control_queue_preserves_all_buffered_lifecycle_frames() {
  let (mut session, mut peer) = attached();
  let writer = tokio::spawn(async move {
    for index in 0..40 {
      peer
        .emit(json!({"type": if index % 2 == 0 { "agent_start" } else { "agent_settled" }}))
        .await;
    }
    // Keep the upstream open while its queued frames are drained.
    peer
  });
  tokio::time::timeout(Duration::from_secs(2), async {
    while session.is_empty() {
      tokio::task::yield_now().await;
    }
  })
  .await
  .unwrap();
  for index in 0..40 {
    assert_eq!(
      next(&mut session).await.unwrap(),
      json!({"type": if index % 2 == 0 { "agent_start" } else { "agent_settled" }})
    );
  }
  let _peer = writer.await.unwrap();
  assert!(session.is_empty());
  session.close().await.unwrap();
}

#[tokio::test]
async fn supervised_unknown_and_duplicate_responses_do_not_consume_a_pending_request() {
  let (mut session, mut peer) = attached();
  session.send("wanted".into(), wire::Command::GetCommands).await.unwrap();
  let _ = peer.next().await;
  peer
    .emit(json!({"type":"response","id":"stale","command":"get_commands","success":true,"data":{"commands":[]}}))
    .await;
  peer
    .emit(json!({"type":"response","id":"wanted","command":"get_commands","success":true,"data":{"commands":[]}}))
    .await;
  peer
    .emit(
      json!({"type":"response","id":"wanted","command":"get_commands","success":true,"data":{"private":"duplicate"}}),
    )
    .await;
  peer.emit(json!({"type":"agent_settled"})).await;
  assert_eq!(next(&mut session).await.unwrap()["id"], "wanted");
  assert_eq!(next(&mut session).await.unwrap(), json!({"type":"agent_settled"}));
  session.close().await.unwrap();
}

#[tokio::test]
async fn supervised_mismatched_response_disconnects_before_any_following_write() {
  let (mut session, mut peer) = attached();
  session
    .send("correlation".into(), wire::Command::GetCommands)
    .await
    .unwrap();
  let _ = peer.next().await;
  peer
    .emit(json!({"type":"response","id":"correlation","command":"get_state","success":true,"data":{}}))
    .await;
  assert!(next(&mut session).await.is_err());
  assert!(session.send("after".into(), wire::Command::GetState).await.is_err());
  session.close().await.unwrap();
}

#[tokio::test]
async fn supervised_invalid_response_envelopes_are_terminal() {
  for response in [
    json!({"type":"response","id":"check","command":"get_state","success":"true"}),
    json!({"type":"response","command":"get_state","success":true,"data":{}}),
  ] {
    let (mut session, mut peer) = attached();
    session.send("check".into(), wire::Command::GetState).await.unwrap();
    let _ = peer.next().await;
    peer.emit(response).await;
    assert!(next(&mut session).await.is_err());
    assert!(session.send("after".into(), wire::Command::GetState).await.is_err());
    session.close().await.unwrap();
  }
}

#[tokio::test]
async fn supervised_response_error_is_bounded_on_character_boundaries_without_changing_data() {
  let (mut session, mut peer) = attached();
  session
    .send("failure".into(), wire::Command::GetCommands)
    .await
    .unwrap();
  let _ = peer.next().await;
  let data = json!({"future":"preserve this response payload"});
  peer
    .emit(json!({
      "type":"response", "id":"failure", "command":"get_commands", "success":false,
      "error":"界".repeat(3000), "data":data,
    }))
    .await;
  let response = next(&mut session).await.unwrap();
  assert_eq!(response["error"].as_str().unwrap().chars().count(), 2048);
  assert_eq!(response["error"].as_str().unwrap(), "界".repeat(2048));
  assert_eq!(response["data"], data);
  session.close().await.unwrap();
}

#[tokio::test]
async fn supervised_control_projection_strips_message_bodies_and_token_tool_traffic() {
  let (mut session, mut peer) = attached();
  for value in [
    json!({"type":"message_update","assistantMessageEvent":{"delta":"private token"}}),
    json!({"type":"tool_execution_end","result":{"content":"private output"}}),
    json!({"type":"message_end","message":{"role":"user","content":[{"text":"private user"}]}}),
    json!({"type":"message_end","message":{"role":"assistant","content":[{"text":"private assistant"}],"stopReason":"error","errorMessage":"provider rejected"}}),
    json!({"type":"auto_retry_end","success":false,"finalError":"retries exhausted","private":"discard"}),
    json!({"type":"auto_compaction_end","aborted":false,"errorMessage":"compaction rejected","private":"discard"}),
    json!({"type":"agent_end","messages":[{"text":"private transcript"}]}),
  ] {
    peer.emit(value).await;
  }
  let ended = next(&mut session).await.unwrap();
  assert_eq!(ended["type"], "message_end");
  assert_eq!(ended["stop_reason"], "error");
  assert_eq!(ended["error_message"], "provider rejected");
  assert_eq!(
    next(&mut session).await.unwrap(),
    json!({"type":"auto_retry_end","success":false,"final_error":"retries exhausted"})
  );
  assert_eq!(
    next(&mut session).await.unwrap(),
    json!({"type":"auto_compaction_end","aborted":false,"error_message":"compaction rejected"})
  );
  assert_eq!(next(&mut session).await.unwrap(), json!({"type":"agent_end"}));
  assert!(!ended.to_string().contains("private"));
  session.close().await.unwrap();
}

#[tokio::test]
async fn supervised_dialog_response_writes_directly_without_state_preflight() {
  let (mut session, mut peer) = attached();
  peer
    .emit(
      json!({"type":"extension_ui_request","id":"dialog","method":"confirm","title":"Confirm","message":"Proceed?"}),
    )
    .await;
  assert_eq!(next(&mut session).await.unwrap()["id"], "dialog");
  session
    .respond(wire::ExtensionUiResponse::ExtensionUiResponse {
      id: "dialog".into(),
      value: None,
      confirmed: Some(false),
      cancelled: None,
    })
    .await
    .unwrap();
  assert_eq!(
    peer.next().await,
    json!({"type":"extension_ui_response","id":"dialog","confirmed":false})
  );
  session.close().await.unwrap();
}

#[tokio::test]
async fn supervised_rejects_invalid_ids_and_oversized_commands_before_writing() {
  let (mut session, mut peer) = attached();
  assert!(
    session
      .send("bad\nidentity".into(), wire::Command::GetState)
      .await
      .is_err()
  );
  assert!(
    session
      .send(
        "oversized".into(),
        wire::Command::Prompt {
          message: "x".repeat(framing::MAX_FRAME_BYTES),
          images: Vec::new(),
          streaming_behavior: None,
        }
      )
      .await
      .is_err()
  );
  session.send("valid".into(), wire::Command::GetCommands).await.unwrap();
  assert_eq!(peer.next().await, json!({"type":"get_commands","id":"valid"}));
  peer
    .emit(json!({"type":"response","id":"valid","command":"get_commands","success":true,"data":{"commands":[]}}))
    .await;
  assert_eq!(next(&mut session).await.unwrap()["id"], "valid");
  session.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn supervised_owned_close_delivers_eof_and_reaps_process_while_retaining_stderr() {
  use std::os::unix::fs::PermissionsExt;
  let directory = tempfile::tempdir().unwrap();
  let executable = directory.path().join("pi-supervised.sh");
  std::fs::write(
    &executable,
    r#"#!/bin/sh
printf 'supervised stderr marker\n' >&2
while IFS= read -r line; do :; done
printf 'closed\n' > "$TOKN_TEST_EOF"
"#,
  )
  .unwrap();
  std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
  let session = PiAdapter::spawn_supervised(
    EndpointOptions::new("supervised-child"),
    LaunchOptions {
      executable,
      working_directory: directory.path().to_path_buf(),
      arguments: Vec::new(),
      environment: BTreeMap::from([("TOKN_TEST_EOF".into(), directory.path().join("eof").into_os_string())]),
      session_file: None,
    },
  )
  .await
  .unwrap();
  assert!(session.pid().is_some());
  tokio::time::timeout(Duration::from_secs(2), async {
    while !session.stderr_tail().contains("supervised stderr marker") {
      tokio::time::sleep(Duration::from_millis(5)).await;
    }
  })
  .await
  .unwrap();
  session.close_input().await;
  session.close().await.unwrap();
  assert_eq!(
    std::fs::read_to_string(directory.path().join("eof")).unwrap(),
    "closed\n"
  );
  assert!(session.stderr_tail().contains("supervised stderr marker"));
}

#[cfg(unix)]
#[tokio::test]
async fn supervised_observes_owned_parent_exit_even_when_a_live_descendant_holds_stdout() {
  for exit_code in [0, 7] {
    check_owned_parent_exit(exit_code).await;
  }
}

#[cfg(unix)]
async fn check_owned_parent_exit(exit_code: u8) {
  use std::os::unix::fs::PermissionsExt;

  // A local control socket fences the synthetic parent exit. Its descendant
  // has a finite lifetime, with explicit PID-scoped cleanup as an extra guard.
  let directory = tempfile::tempdir().unwrap();
  let socket_path = directory.path().join("descendant.sock");
  let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
  let executable = directory.path().join("pi-exit-test.py");
  std::fs::write(
    &executable,
    r#"#!/usr/bin/env python3
import os, socket, subprocess, sys
sock = socket.socket(socket.AF_UNIX)
sock.connect(os.environ['TOKN_TEST_CONTROL_SOCKET'])
child = subprocess.Popen(['/bin/sleep', '5'], stdout=sys.stdout, stderr=sys.stderr)
sock.sendall(str(child.pid).encode() + b'\n')
while sock.recv(1) != b'X':
  pass
os._exit(int(os.environ['TOKN_TEST_EXIT_CODE']))
"#,
  )
  .unwrap();
  std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
  let mut session = PiAdapter::spawn_supervised(
    EndpointOptions::new("supervised-parent-exit"),
    LaunchOptions {
      executable,
      working_directory: directory.path().to_path_buf(),
      arguments: Vec::new(),
      environment: BTreeMap::from([
        ("TOKN_TEST_CONTROL_SOCKET".into(), socket_path.into_os_string()),
        ("TOKN_TEST_EXIT_CODE".into(), exit_code.to_string().into()),
      ]),
      session_file: None,
    },
  )
  .await
  .unwrap();
  let (mut control, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
    .await
    .unwrap()
    .unwrap();
  let mut id = Vec::new();
  loop {
    use tokio::io::AsyncReadExt;
    let byte = control.read_u8().await.unwrap();
    if byte == b'\n' {
      break;
    }
    id.push(byte);
  }
  let descendant_pid: u32 = std::str::from_utf8(&id).unwrap().parse().unwrap();
  struct DescendantGuard(u32);
  impl Drop for DescendantGuard {
    fn drop(&mut self) {
      // This PID was created by our synthetic parent just before its controlled
      // exit. The finite sleep outlives the one-second assertion budget.
      let _ = std::process::Command::new("/bin/kill")
        .args(["-TERM", &self.0.to_string()])
        .status();
    }
  }
  let _descendant = DescendantGuard(descendant_pid);
  assert!(!session.exited_successfully());
  control.write_all(b"X").await.unwrap();
  let terminal = tokio::time::timeout(Duration::from_secs(1), session.recv()).await;
  assert_eq!(session.exited_successfully(), exit_code == 0);
  session.close().await.unwrap();
  assert_eq!(session.exited_successfully(), exit_code == 0);
  let message = terminal
    .expect("native parent exit was hidden by inherited stdout")
    .unwrap()
    .unwrap_err();
  assert!(message.contains("exit") || message.contains("stopped"), "{message}");
}
