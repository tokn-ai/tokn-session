use super::*;

fn message_input() -> MessageInput {
  MessageInput {
    text: "hello".into(),
    ..MessageInput::default()
  }
}

#[cfg(unix)]
#[tokio::test]
async fn creates_deferred_sessions_and_dispatches_only_honest_v4_modes() {
  let directory = std::env::current_dir().unwrap();
  let history_path = directory.join("delivery-fixture.sqlite");
  let snapshot = json!({
    "protocol":{"name":"ZCode Protocol","version":1},
    "session":{
      "sessionId":"sess_fixture", "sessionKind":"interactive",
      "workspace":{"workspaceKey":directory,"workspacePath":directory}
    },
    "projection":{"sessionId":"unknown","status":"idle","turnCount":0},
    "runtime":{"stateRevision":0},"settings":{"model":{"available":[]}}
  });
  let script = r#"
    [ "$0" = app-server ] && [ "$1" = --stdio ] || exit 2
    printf '{"method":"startup/storageState","params":{"databaseKind":"session","phase":"ready","databaseId":"%s"}}\n' "$FIXTURE_DATABASE_ID"
    IFS= read -r request || exit 3
    case "$request" in *'"method":"runtime/capabilities"'*) ;; *) exit 4 ;; esac
    printf '%s\n' '{"id":1,"result":{"sessionLifecycleWorkspace":true,"sessionWorkspaceRouting":true}}'
    IFS= read -r request || exit 5
    case "$request" in *'"method":"session/create"'*'"persistence":"deferred"'*) ;; *) exit 6 ;; esac
    printf '{"id":2,"result":%s}\n' "$FIXTURE_SNAPSHOT"
    IFS= read -r request || exit 7
    case "$request" in *'"method":"session/subscribe"'*) ;; *) exit 8 ;; esac
    printf '%s\n' '{"id":3,"result":{"sessionId":"sess_fixture"}}'
    IFS= read -r request || exit 9
    case "$request" in *'"method":"v4/command"'*'"commandId":"guide-1"'*'"requestedDelivery":"guide"'*'"type":"sendText"'*) ;; *) exit 10 ;; esac
    printf '%s\n' '{"id":4,"result":{"commandId":"guide-1","status":"accepted","revisionAtDecision":1,"result":{"type":"inputAccepted","delivery":"startNow","inputId":"guide-1"}}}'
    IFS= read -r request || exit 11
    case "$request" in *'"method":"v4/command"'*'"commandId":"queue-1"'*'"requestedDelivery":"queue"'*'"type":"sendText"'*) ;; *) exit 12 ;; esac
    printf '%s\n' '{"id":5,"result":{"commandId":"queue-1","status":"accepted","revisionAtDecision":2,"result":{"type":"inputAccepted","delivery":"queue","inputId":"queue-1"}}}'
    IFS= read -r request || exit 13
    case "$request" in *'"method":"v4/command"'*'"commandId":"stop-1"'*'"type":"stop"'*) ;; *) exit 14 ;; esac
    printf '%s\n' '{"id":6,"result":{"commandId":"stop-1","status":"accepted","revisionAtDecision":3}}'
    while IFS= read -r request; do exit 15; done
  "#;
  let adapter = ZCodeAdapter::launch(LaunchOptions {
    runtime_id: "fixture-runtime".into(),
    executable: "/bin/sh".into(),
    arguments: vec!["-c".into(), script.into()],
    directory,
    history_path: history_path.clone(),
    environment: BTreeMap::from([
      ("FIXTURE_DATABASE_ID".into(), database_hash(&history_path).unwrap()),
      ("FIXTURE_SNAPSHOT".into(), snapshot.to_string()),
    ]),
  })
  .await
  .unwrap();
  let target = adapter
    .create_session_with_id("sess_fixture", None)
    .await
    .unwrap()
    .target;
  for mode in [DeliveryMode::Start, DeliveryMode::Steer, DeliveryMode::StartOrSteer] {
    assert_eq!(
      adapter
        .inner
        .message("sess_fixture", "rejected-mode", message_input(), mode)
        .await
        .unwrap_err()
        .kind,
      ErrorKind::Unsupported
    );
  }
  for mode in [DeliveryMode::Guide, DeliveryMode::Queue] {
    let mut input = message_input();
    input.expected_revision = Some("7".into());
    assert_eq!(
      adapter
        .inner
        .message("sess_fixture", "rejected-fence", input, mode)
        .await
        .unwrap_err()
        .kind,
      ErrorKind::Unsupported
    );
    let mut input = message_input();
    input.model = Some(ModelIdentity {
      model_provider: Some("fixture".into()),
      model_id: "fixture-model".into(),
      display_name: None,
    });
    assert_eq!(
      adapter
        .inner
        .message("sess_fixture", "rejected-model", input, mode)
        .await
        .unwrap_err()
        .kind,
      ErrorKind::Unsupported
    );
  }
  for (request_id, mode, disposition) in [
    ("guide-1", DeliveryMode::Guide, InputDisposition::Started),
    ("queue-1", DeliveryMode::Queue, InputDisposition::Queued),
  ] {
    let Reply::Admission(receipt) = adapter
      .execute(InteractionRequest {
        request_id: request_id.into(),
        target: target.clone(),
        command: Command::Message {
          input: message_input(),
          delivery: mode,
        },
      })
      .await
      .unwrap()
    else {
      panic!("expected admission receipt");
    };
    assert_eq!(receipt.disposition, disposition);
    assert_eq!(receipt.native_id.as_deref(), Some(request_id));
  }
  assert!(matches!(
    adapter
      .execute(InteractionRequest {
        request_id: "stop-1".into(),
        target,
        command: Command::CancelTurn,
      })
      .await
      .unwrap(),
    Reply::Acknowledged
  ));
  adapter.close().await.unwrap();
}

fn ack(command_id: &str, status: &str, delivery: &str) -> CommandAck {
  serde_json::from_value(json!({
    "commandId":command_id,"status":status,"revisionAtDecision":7,
    "result":{"type":"inputAccepted","inputId":command_id,"delivery":delivery}
  }))
  .unwrap()
}

#[test]
fn distinguishes_guide_acceptance_from_follow_up_queue_admission() {
  for status in ["accepted", "duplicate"] {
    let guide = ack("command-1", status, "queue");
    validate_ack(&guide, "command-1").unwrap();
    assert_eq!(
      input_receipt(guide, "command-1", DeliveryMode::Guide)
        .unwrap()
        .disposition,
      InputDisposition::Accepted
    );
    let queued = ack("command-1", status, "queue");
    assert_eq!(
      input_receipt(queued, "command-1", DeliveryMode::Queue)
        .unwrap()
        .disposition,
      InputDisposition::Queued
    );
    for delivery in [DeliveryMode::Guide, DeliveryMode::Queue] {
      assert_eq!(
        input_receipt(ack("command-1", status, "startNow"), "command-1", delivery)
          .unwrap()
          .disposition,
        InputDisposition::Started
      );
    }
  }
}

#[test]
fn verifies_both_ack_and_input_ids_and_preserves_unknown_delivery_outcomes() {
  assert_eq!(
    validate_ack(&ack("other-command", "accepted", "queue"), "command-1")
      .unwrap_err()
      .kind,
    ErrorKind::Uncertain
  );
  let mut different = ack("command-1", "accepted", "queue");
  different.result.as_mut().unwrap()["inputId"] = json!("other-input");
  assert_eq!(
    input_receipt(different, "command-1", DeliveryMode::Queue)
      .unwrap_err()
      .kind,
    ErrorKind::Uncertain
  );
  assert_eq!(
    input_receipt(
      ack("command-1", "accepted", "future-delivery"),
      "command-1",
      DeliveryMode::Guide
    )
    .unwrap_err()
    .kind,
    ErrorKind::Uncertain
  );
  assert_eq!(
    input_receipt(ack("command-1", "accepted", "guide"), "command-1", DeliveryMode::Queue)
      .unwrap_err()
      .kind,
    ErrorKind::Uncertain
  );
}

#[test]
fn rejection_conflict_and_uncertain_native_failures_remain_distinct() {
  for (status, kind) in [
    ("rejected", ErrorKind::Rejected),
    ("stale", ErrorKind::Conflict),
    ("noop", ErrorKind::Conflict),
    ("failed", ErrorKind::Uncertain),
    ("unknown", ErrorKind::Uncertain),
  ] {
    assert_eq!(
      validate_ack(&ack("command-1", status, "queue"), "command-1")
        .unwrap_err()
        .kind,
      kind
    );
  }
  let missing: CommandAck =
    serde_json::from_value(json!({"commandId":"command-1","status":"accepted","revisionAtDecision":7})).unwrap();
  assert_eq!(
    input_receipt(missing, "command-1", DeliveryMode::Queue)
      .unwrap_err()
      .kind,
    ErrorKind::Uncertain
  );
}

#[test]
fn held_queue_guard_has_a_safe_typed_error_without_hiding_other_uncertain_failures() {
  for status in ["failed", "rejected"] {
    let held: CommandAck = serde_json::from_value(json!({
      "commandId":"command-1","status":status,"revisionAtDecision":7,
      "reasonCode":"heldQueueDispositionRequired"
    }))
    .unwrap();
    let error = validate_ack(&held, "command-1").unwrap_err();
    assert_eq!(error.kind, ErrorKind::QueueHeld);
    assert!(error.message.contains("keep or clear the held queue"));
    assert_eq!(
      validate_ack(&held, "other-command").unwrap_err().kind,
      ErrorKind::Uncertain
    );
  }
  for (status, reason) in [
    ("failed", "fault.command.executionFailed"),
    ("failed", "unknownReason"),
    ("unknownStatus", "heldQueueDispositionRequired"),
  ] {
    let unknown: CommandAck = serde_json::from_value(json!({
      "commandId":"command-1","status":status,"revisionAtDecision":7,"reasonCode":reason
    }))
    .unwrap();
    assert_eq!(
      validate_ack(&unknown, "command-1").unwrap_err().kind,
      ErrorKind::Uncertain
    );
  }
}

#[test]
fn native_envelope_keeps_caller_command_identity_and_explicit_requested_delivery() {
  let workspace = json!({"workspaceKey":"/fixture","workspacePath":"/fixture"});
  let envelope = command_envelope(
    workspace.clone(),
    "client-fixture",
    "sess_fixture",
    "command-1",
    "sendText",
    json!({"text":"Use the local fixture","requestedDelivery":"guide"}),
  );
  assert_eq!(envelope["workspace"], workspace);
  assert_eq!(envelope["commandId"], "command-1");
  assert_eq!(envelope["clientId"], "client-fixture");
  assert_eq!(envelope["sessionId"], "sess_fixture");
  assert_eq!(envelope["type"], "sendText");
  assert_eq!(envelope["payload"]["requestedDelivery"], "guide");
  assert!(envelope["issuedAt"].as_u64().unwrap() > 0);
  assert!(envelope.get("baseRevision").is_none());
  assert!(envelope["payload"].get("expectedTurnId").is_none());
}
