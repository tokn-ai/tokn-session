use std::{collections::BTreeMap, path::PathBuf, sync::OnceLock, time::Duration};
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokn_interaction_core::*;
use tokn_qoder_adapter::{LaunchOptions, QoderAdapter};

fn peer() -> &'static PathBuf {
  static PEER: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
  &PEER
    .get_or_init(|| {
      let directory = tempfile::tempdir().unwrap();
      let path = directory
        .path()
        .join(format!("acp-peer{}", std::env::consts::EXE_SUFFIX));
      let status = std::process::Command::new("rustc")
        .args(["--edition=2024", "-o"])
        .arg(&path)
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp_peer.rs"))
        .status()
        .unwrap();
      assert!(status.success());
      (directory, path)
    })
    .1
}
async fn launch(scenario: &str) -> (QoderAdapter, TempDir) {
  let directory = tempfile::tempdir().unwrap();
  let adapter = QoderAdapter::launch(LaunchOptions {
    runtime_id: "fixture-runtime".into(),
    executable: peer().clone(),
    arguments: Vec::new(),
    directory: directory.path().to_owned(),
    environment: BTreeMap::from([
      ("ACP_FIXTURE_SCENARIO".into(), scenario.into()),
      (
        "ACP_FIXTURE_TRACE".into(),
        directory.path().join("methods.txt").to_string_lossy().into_owned(),
      ),
    ]),
  })
  .await
  .unwrap();
  (adapter, directory)
}
fn request(target: &SessionTarget, command: Command) -> InteractionRequest {
  InteractionRequest {
    request_id: uuid::Uuid::new_v4().to_string(),
    target: target.clone(),
    command,
  }
}
fn message(target: &SessionTarget, delivery: DeliveryMode) -> InteractionRequest {
  request(
    target,
    Command::Message {
      input: MessageInput {
        text: "fixture prompt".into(),
        ..Default::default()
      },
      delivery,
    },
  )
}
async fn event(
  events: &mut broadcast::Receiver<RuntimeEvent>,
  predicate: impl Fn(&RuntimeEventKind) -> bool,
) -> RuntimeEvent {
  tokio::time::timeout(Duration::from_secs(5), async {
    loop {
      let event = events.recv().await.unwrap();
      if predicate(&event.kind) {
        return event;
      }
    }
  })
  .await
  .unwrap()
}
async fn idle(events: &mut broadcast::Receiver<RuntimeEvent>) {
  event(
    events,
    |kind| matches!(kind, RuntimeEventKind::State(state) if state.status == ExecutionState::Idle),
  )
  .await;
}

#[tokio::test]
async fn negotiates_caps_uses_exact_resume_and_only_cached_catalog_inspection() {
  let (adapter, _directory) = launch("basic").await;
  let descriptor = adapter.descriptor();
  assert!(descriptor.capabilities.image_input);
  assert!(!descriptor.capabilities.atomic_model_on_send);
  assert!(!descriptor.capabilities.supports(Operation::Queue));
  let global = SessionTarget {
    runtime_id: descriptor.runtime_id,
    instance_id: descriptor.instance_id,
    session_id: None,
  };
  assert_eq!(
    adapter
      .execute(request(&global, Command::Models))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Unsupported
  );
  let created = adapter.resume_session("fixture-session").await.unwrap();
  assert_eq!(created.target.session_id.as_deref(), Some("fixture-session"));
  assert!(
    matches!(adapter.execute(request(&global,Command::Models)).await.unwrap(),Reply::Models(models) if models.len()==2 && models.iter().all(|model| !model.supports_images))
  );
  let mut events = adapter.subscribe();
  assert!(
    matches!(adapter.execute(message(&created.target,DeliveryMode::Prompt)).await.unwrap(),Reply::Admission(receipt) if receipt.disposition==InputDisposition::Accepted)
  );
  event(
    &mut events,
    |kind| matches!(kind,RuntimeEventKind::Native(value) if value["protocol"]=="acp"),
  )
  .await;
  idle(&mut events).await;
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn creation_model_is_applied_before_returning_and_delivery_modes_fail_before_prompt() {
  let (adapter, _directory) = launch("image").await;
  let created = adapter
    .create_session(Some(ModelIdentity {
      model_provider: None,
      model_id: "model-b".into(),
      display_name: None,
    }))
    .await
    .unwrap();
  assert!(
    matches!(adapter.execute(request(&created.target,Command::State)).await.unwrap(),Reply::State(state) if state.model.as_ref().unwrap().model_id=="model-b")
  );
  for delivery in [
    DeliveryMode::Start,
    DeliveryMode::Queue,
    DeliveryMode::Guide,
    DeliveryMode::Steer,
    DeliveryMode::StartOrSteer,
  ] {
    assert_eq!(
      adapter
        .execute(message(&created.target, delivery))
        .await
        .unwrap_err()
        .kind,
      ErrorKind::Unsupported
    );
  }
  let image = MessageInput {
    images: vec![ImageInput {
      mime_type: "image/png".into(),
      data: "aW1hZ2U=".into(),
    }],
    ..Default::default()
  };
  let mut events = adapter.subscribe();
  adapter
    .execute(request(
      &created.target,
      Command::Message {
        input: image,
        delivery: DeliveryMode::Prompt,
      },
    ))
    .await
    .unwrap();
  idle(&mut events).await;
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn permission_approves_only_original_envelope_and_retires_without_replaying() {
  let (adapter, _directory) = launch("permission").await;
  let target = adapter.create_session(None).await.unwrap().target;
  let mut events = adapter.subscribe();
  let sender = adapter.clone();
  let input = message(&target, DeliveryMode::Prompt);
  let send = tokio::spawn(async move { sender.execute(input).await });
  let pending = event(&mut events, |kind| {
    matches!(kind, RuntimeEventKind::InteractionRequested(_))
  })
  .await;
  let RuntimeEventKind::InteractionRequested(pending) = pending.kind else {
    unreachable!()
  };
  assert!(
    !send.is_finished(),
    "Agent activity and permission requests cannot prove admission"
  );
  assert_eq!(
    adapter
      .execute(message(&target, DeliveryMode::Prompt))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Conflict
  );
  assert!(
    pending.native["details"]
      .as_str()
      .unwrap()
      .contains("write fixture.txt")
  );
  assert_eq!(
    pending.native["permission_decisions"],
    serde_json::json!(["allow_once", "deny"])
  );
  assert_eq!(
    adapter
      .execute(request(
        &target,
        Command::Respond {
          interaction_id: pending.interaction_id.clone(),
          response: InteractionResponse::Permission {
            decision: PermissionDecision::AllowAlways
          }
        }
      ))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Unsupported
  );
  adapter
    .execute(request(
      &target,
      Command::Respond {
        interaction_id: pending.interaction_id.clone(),
        response: InteractionResponse::Permission {
          decision: PermissionDecision::AllowOnce,
        },
      },
    ))
    .await
    .unwrap();
  send.await.unwrap().unwrap();
  idle(&mut events).await;
  assert!(
    matches!(adapter.execute(request(&target,Command::PendingInteractions)).await.unwrap(),Reply::Interactions(pending) if pending.is_empty())
  );
  assert!(
    adapter
      .execute(request(
        &target,
        Command::Respond {
          interaction_id: pending.interaction_id,
          response: InteractionResponse::Reject
        }
      ))
      .await
      .is_err()
  );
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn stop_cancels_pending_permission_and_waits_for_native_terminal_state() {
  let (adapter, _directory) = launch("cancel").await;
  let target = adapter.create_session(None).await.unwrap().target;
  let mut events = adapter.subscribe();
  let sender = adapter.clone();
  let input = message(&target, DeliveryMode::Prompt);
  let send = tokio::spawn(async move { sender.execute(input).await });
  event(&mut events, |kind| {
    matches!(kind, RuntimeEventKind::InteractionRequested(_))
  })
  .await;
  assert!(
    !send.is_finished(),
    "Permission request cannot complete prompt admission"
  );
  adapter.execute(request(&target, Command::CancelTurn)).await.unwrap();
  send.await.unwrap().unwrap();
  event(
    &mut events,
    |kind| matches!(kind,RuntimeEventKind::Native(value) if value["stop_reason"]=="cancelled"),
  )
  .await;
  idle(&mut events).await;
  assert!(
    matches!(adapter.execute(request(&target,Command::PendingInteractions)).await.unwrap(),Reply::Interactions(pending) if pending.is_empty())
  );
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn stop_on_first_running_is_ordered_after_prompt_dispatch() {
  let (adapter, _directory) = launch("stop-order").await;
  let target = adapter.create_session(None).await.unwrap().target;
  let mut events = adapter.subscribe();
  let sender = adapter.clone();
  let input = message(&target, DeliveryMode::Prompt);
  let send = tokio::spawn(async move { sender.execute(input).await });
  event(
    &mut events,
    |kind| matches!(kind,RuntimeEventKind::State(state) if state.status==ExecutionState::Running),
  )
  .await;
  adapter.execute(request(&target, Command::CancelTurn)).await.unwrap();
  send.await.unwrap().unwrap();
  event(
    &mut events,
    |kind| matches!(kind,RuntimeEventKind::Native(value) if value["stop_reason"]=="cancelled"),
  )
  .await;
  idle(&mut events).await;
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn unsupported_reverse_requests_and_plan_mode_are_answered_promptly() {
  for scenario in ["unknown", "plan"] {
    let (adapter, _directory) = launch(scenario).await;
    let target = adapter.create_session(None).await.unwrap().target;
    let mut events = adapter.subscribe();
    adapter.execute(message(&target, DeliveryMode::Prompt)).await.unwrap();
    idle(&mut events).await;
    assert!(
      matches!(adapter.execute(request(&target,Command::PendingInteractions)).await.unwrap(),Reply::Interactions(pending) if pending.is_empty())
    );
    adapter.close().await.unwrap();
  }
}

#[tokio::test]
async fn zero_output_refusal_has_terminal_event_and_disconnect_never_fakes_admission() {
  let (adapter, _directory) = launch("zero").await;
  let target = adapter.create_session(None).await.unwrap().target;
  let mut events = adapter.subscribe();
  adapter.execute(message(&target, DeliveryMode::Prompt)).await.unwrap();
  event(
    &mut events,
    |kind| matches!(kind,RuntimeEventKind::Native(value) if value["stop_reason"]=="refusal"),
  )
  .await;
  idle(&mut events).await;
  adapter.close().await.unwrap();
  let (adapter, _directory) = launch("lost").await;
  let target = adapter.create_session(None).await.unwrap().target;
  assert_eq!(
    adapter
      .execute(message(&target, DeliveryMode::Prompt))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Uncertain
  );
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn standard_configuration_wins_and_qoder_legacy_compatibility_is_separate() {
  for scenario in ["grouped-model", "prefer-standard", "legacy-model"] {
    let (adapter, directory) = launch(scenario).await;
    let target = adapter
      .create_session(Some(ModelIdentity {
        model_provider: None,
        model_id: "model-b".into(),
        display_name: None,
      }))
      .await
      .unwrap()
      .target;
    assert!(
      matches!(adapter.execute(request(&target,Command::State)).await.unwrap(),Reply::State(state) if state.model.as_ref().unwrap().model_id=="model-b")
    );
    let Reply::Admission(receipt) = adapter.execute(message(&target, DeliveryMode::Prompt)).await.unwrap() else {
      panic!("native admission required")
    };
    assert_eq!(
      receipt.native_id.as_deref(),
      if scenario == "legacy-model" {
        Some("legacy-user-message")
      } else {
        None
      }
    );
    adapter.close().await.unwrap();
    let trace = std::fs::read_to_string(directory.path().join("methods.txt")).unwrap();
    if scenario == "legacy-model" {
      assert!(trace.lines().any(|method| method == "session/set_model"));
      assert!(!trace.lines().any(|method| method == "session/set_config_option"));
    } else {
      assert!(trace.lines().any(|method| method == "session/set_config_option"));
      assert!(!trace.lines().any(|method| method == "session/set_model"));
    }
  }
}

#[tokio::test]
async fn standard_setter_requires_full_confirmed_state_and_never_retries_the_legacy_setter() {
  for scenario in ["incomplete-config-result", "wrong-config-result"] {
    let (adapter, directory) = launch(scenario).await;
    let error = adapter
      .create_session(Some(ModelIdentity {
        model_provider: None,
        model_id: "model-b".into(),
        display_name: None,
      }))
      .await
      .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Uncertain);
    adapter.close().await.unwrap();
    let trace = std::fs::read_to_string(directory.path().join("methods.txt")).unwrap();
    assert_eq!(trace.lines().filter(|method| *method == "session/new").count(), 1);
    assert_eq!(
      trace
        .lines()
        .filter(|method| *method == "session/set_config_option")
        .count(),
      1
    );
    assert!(!trace.lines().any(|method| method == "session/set_model"));
  }
}

#[tokio::test]
async fn standard_model_updates_use_semantic_category_and_an_opaque_selector_id() {
  let (adapter, _directory) = launch("config-update").await;
  let target = adapter.create_session(None).await.unwrap().target;
  adapter.execute(message(&target, DeliveryMode::Prompt)).await.unwrap();
  assert!(
    matches!(adapter.execute(request(&target,Command::State)).await.unwrap(),Reply::State(state) if state.model.as_ref().unwrap().model_id=="model-b")
  );
  adapter.close().await.unwrap();
}

/// Opt-in handshake/authentication-failure check; does not log in or submit a prompt.
#[tokio::test]
#[ignore = "requires an explicitly supplied Qoder executable; uses a disposable anonymous config"]
async fn real_anonymous_handshake_and_authentication_failure() {
  let executable = std::env::var_os("TOKN_QODER_ACP_EXECUTABLE").expect("explicit Qoder executable required");
  let arguments: Vec<String> =
    serde_json::from_str(&std::env::var("TOKN_QODER_ACP_ARGUMENTS").unwrap_or_else(|_| "[]".into())).unwrap();
  let directory = tempfile::tempdir().unwrap();
  let home = directory.path().join("home");
  let config = directory.path().join("config");
  let project = directory.path().join("project");
  for path in [&home, &config, &project] {
    std::fs::create_dir(path).unwrap();
  }
  let mut environment: BTreeMap<String, String> = std::env::vars()
    .filter(|(name, _)| name.starts_with("QODER"))
    .map(|(name, _)| (name, String::new()))
    .collect();
  for name in ["HOME", "USERPROFILE", "QODER_CLI_HOME"] {
    environment.insert(name.into(), home.to_string_lossy().into_owned());
  }
  environment.insert("QODER_CONFIG_DIR".into(), config.to_string_lossy().into_owned());
  let adapter = QoderAdapter::launch(LaunchOptions {
    runtime_id: "anonymous-smoke".into(),
    executable: executable.into(),
    arguments,
    directory: project,
    environment,
  })
  .await
  .unwrap();
  assert!(adapter.descriptor().capabilities.supports(Operation::Message));
  let error = adapter.create_session(None).await.unwrap_err();
  assert_eq!(error.kind, ErrorKind::Rejected);
  assert!(error.message.contains("authentication"));
  adapter.close().await.unwrap();
}
