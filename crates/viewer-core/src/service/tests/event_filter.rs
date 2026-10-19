use super::*;
use tokn_session_client::{AgentClient, Source};
use tokn_session_core::{AgentCommunication, CompactionEvent, CompactionState};

fn bookkeeping(event: &AgentEvent) -> bool {
  event_summary(std::slice::from_ref(event), 0, event).is_bookkeeping
}

fn metadata(provider: Provider, kind: MetadataKind, native_type: &str, native: Value) -> AgentEvent {
  AgentEvent::Metadata(MetadataEvent {
    provider,
    session_id: Some("fixture".into()),
    kind,
    native_type: native_type.into(),
    summary: "provider record".into(),
    native,
    timestamp: None,
  })
}

#[test]
fn lifecycle_filter_preserves_every_exceptional_outcome_and_native_body() {
  for outcome in [None, Some(LifecycleOutcome::Completed)] {
    let AgentEvent::Lifecycle(mut lifecycle) = lifecycle_event() else {
      unreachable!()
    };
    lifecycle.outcome = outcome;
    assert!(bookkeeping(&AgentEvent::Lifecycle(lifecycle.clone())));
    for native in [
      json!({"type":"turn_complete","error":{"message":"failed","code":"HTTP"}}),
      json!({"payload":{"error":false}}),
      json!({"data":{"reason":{"error":"provider unavailable"}}}),
      json!({"last_agent_message":"final reply retained only here"}),
      json!({"payload":{"text":"meaningful lifecycle text"}}),
      json!({"text":{"future":"text representation"}}),
    ] {
      lifecycle.native = native.clone();
      assert!(!bookkeeping(&AgentEvent::Lifecycle(lifecycle.clone())), "{native}");
    }
    lifecycle.native = json!({"error":null,"text":" \n","last_agent_message":null});
    assert!(bookkeeping(&AgentEvent::Lifecycle(lifecycle)));
  }
  for outcome in [
    LifecycleOutcome::Failed,
    LifecycleOutcome::Blocked,
    LifecycleOutcome::TokenLimit,
    LifecycleOutcome::Cancelled,
    LifecycleOutcome::Interrupted,
  ] {
    let AgentEvent::Lifecycle(mut lifecycle) = lifecycle_event() else {
      unreachable!()
    };
    lifecycle.outcome = Some(outcome);
    assert!(!bookkeeping(&AgentEvent::Lifecycle(lifecycle)), "{outcome:?}");
  }
}

#[test]
fn codex_finished_lifecycle_hides_the_response_echo_but_preserves_errors_and_unfamiliar_content() {
  for native_type in ["turn_complete", "task_complete"] {
    let AgentEvent::Lifecycle(mut lifecycle) = lifecycle_event() else {
      unreachable!()
    };
    lifecycle.outcome = Some(LifecycleOutcome::Completed);
    lifecycle.native = json!({"type":native_type,"last_agent_message":"final reply"});
    assert!(bookkeeping(&AgentEvent::Lifecycle(lifecycle.clone())));

    for native in [
      json!({"type":native_type,"last_agent_message":"final reply","error":{"message":"failed"}}),
      json!({"type":native_type,"last_agent_message":"final reply","payload":{"error":"failed"}}),
      json!({"type":native_type,"last_agent_message":"final reply","text":"additional lifecycle text"}),
      json!({"type":native_type,"last_agent_message":{"future":"response representation"}}),
    ] {
      lifecycle.native = native.clone();
      assert!(!bookkeeping(&AgentEvent::Lifecycle(lifecycle.clone())), "{native}");
    }
    lifecycle.native = json!({"type":native_type,"last_agent_message":"final reply"});
    lifecycle.provider = Provider::Dsh;
    assert!(!bookkeeping(&AgentEvent::Lifecycle(lifecycle.clone())));
    lifecycle.provider = Provider::Codex;
    lifecycle.outcome = Some(LifecycleOutcome::Interrupted);
    assert!(!bookkeeping(&AgentEvent::Lifecycle(lifecycle)));
  }
}

#[test]
fn metadata_filter_requires_a_known_provider_kind_and_record_contract() {
  let routine = [
    (Provider::Codex, MetadataKind::Configuration, "turn_context"),
    (Provider::Codex, MetadataKind::Context, "world_state"),
    (
      Provider::Codex,
      MetadataKind::Context,
      "inter_agent_communication_metadata",
    ),
    (Provider::Codex, MetadataKind::Diagnostic, "event_msg.token_count"),
    (Provider::Pi, MetadataKind::Configuration, "active_tools_change"),
    (Provider::Pi, MetadataKind::Context, "leaf"),
    (Provider::Pi, MetadataKind::Session, "session_info"),
    (Provider::Pi, MetadataKind::Session, "label"),
    (Provider::Dsh, MetadataKind::Configuration, "permission/preset"),
    (Provider::Dsh, MetadataKind::Configuration, "sandbox/mode"),
    (Provider::Dsh, MetadataKind::Configuration, "approval/policy"),
    (Provider::Dsh, MetadataKind::Context, "request/context"),
    (Provider::Dsh, MetadataKind::Session, "session/end-seed"),
    (Provider::Dsh, MetadataKind::Session, "session/title"),
    (Provider::WorkBuddy, MetadataKind::Session, "ai-title"),
    (
      Provider::OpenCode,
      MetadataKind::Configuration,
      "runtime/bash_shell_selection",
    ),
    (
      Provider::ZCode,
      MetadataKind::Configuration,
      "runtime/bash_shell_selection",
    ),
  ];
  for (provider, kind, native_type) in routine {
    assert!(
      bookkeeping(&metadata(provider, kind, native_type, json!({}))),
      "{native_type}"
    );
    assert!(!bookkeeping(&metadata(provider, kind, "future_record", json!({}))));
  }
  assert!(!bookkeeping(&metadata(
    Provider::Pi,
    MetadataKind::Context,
    "world_state",
    json!({})
  )));
  assert!(!bookkeeping(&metadata(
    Provider::Codex,
    MetadataKind::Queue,
    "turn_context",
    json!({})
  )));
}

#[test]
fn metadata_with_content_or_diagnostics_survives() {
  for (provider, kind, native_type, native) in [
    (
      Provider::Pi,
      MetadataKind::Context,
      "branch_summary",
      json!({"summary":"retained decisions"}),
    ),
    (
      Provider::Pi,
      MetadataKind::Session,
      "custom",
      json!({"data":{"body":"extension content"}}),
    ),
    (
      Provider::Codex,
      MetadataKind::Context,
      "event_msg.item_completed.Plan",
      json!({"item":{"text":"plan steps"}}),
    ),
    (
      Provider::Codex,
      MetadataKind::Context,
      "event_msg.item_completed.HookPrompt",
      json!({"item":{"fragments":[{"text":"hook prompt"}]}}),
    ),
    (
      Provider::Codex,
      MetadataKind::Context,
      "event_msg.item_completed.UserMessage",
      json!({"item":{"content":[{"type":"image","image_url":"image"}]}}),
    ),
    (
      Provider::Codex,
      MetadataKind::Context,
      "event_msg.thread_rolled_back",
      json!({"num_turns":2}),
    ),
    (
      Provider::Dsh,
      MetadataKind::Queue,
      "agent/inbox/spliced",
      json!({"data":{"inserted":[{"content":[{"text":"queued input"}]}]}}),
    ),
    (
      Provider::Dsh,
      MetadataKind::Diagnostic,
      "session/title-llm-request",
      json!({"data":{"system":"prompt"}}),
    ),
    (
      Provider::Dsh,
      MetadataKind::Diagnostic,
      "web/deepseek-search-llm-request",
      json!({"data":{"body":{"messages":[]}}}),
    ),
    (
      Provider::Dsh,
      MetadataKind::Stream,
      "assistant/chunk",
      json!({"data":{"chunk":{"type":"tool-call-delta","argumentsDelta":"{"}}}),
    ),
    (
      Provider::WorkBuddy,
      MetadataKind::Context,
      "file-history-snapshot",
      json!({"snapshot":{"files":["edited.rs"]}}),
    ),
    (
      Provider::ZCode,
      MetadataKind::Diagnostic,
      "runtime/user_input_auto_resolution",
      json!({"data":{"answer":"yes"}}),
    ),
  ] {
    assert!(
      !bookkeeping(&metadata(provider, kind, native_type, native)),
      "{native_type}"
    );
  }
}

#[test]
fn usage_and_content_variants_never_become_bookkeeping_even_when_empty() {
  for provider in [
    Provider::Codex,
    Provider::Pi,
    Provider::Dsh,
    Provider::OpenCode,
    Provider::ZCode,
    Provider::WorkBuddy,
  ] {
    for kind in [
      UsageKind::ModelCall,
      UsageKind::SessionSnapshot,
      UsageKind::OperationTotal,
    ] {
      assert!(!bookkeeping(&usage_event(kind, provider)));
    }
  }
  let mut encrypted = agent_activity("child", Some("/root/worker"));
  let AgentEvent::AgentActivity(activity) = &mut encrypted else {
    unreachable!()
  };
  activity.communication = Some(AgentCommunication {
    text: None,
    has_encrypted_content: true,
    trigger_turn: Some(false),
  });
  for event in [
    message_event(""),
    reasoning_event(None, None, Some("encrypted reasoning"), None, None),
    tool_call(
      Provider::Codex,
      "shell",
      "tool-1",
      ToolKind::Shell,
      None,
      Phase::Started,
      None,
    ),
    encrypted,
    AgentEvent::Compaction(CompactionEvent::new(Provider::Codex, None, CompactionState::Completed)),
    AgentEvent::Error(ErrorEvent {
      provider: Provider::Codex,
      session_id: None,
      message: "failure".into(),
      native: None,
      timestamp: None,
    }),
    AgentEvent::Unknown(UnknownEvent {
      provider: Provider::Codex,
      session_id: None,
      native_type: Some("turn_context".into()),
      native: Some(json!({})),
      timestamp: None,
    }),
  ] {
    assert!(!bookkeeping(&event), "{event:?}");
  }
  assert!(bookkeeping(&settings_event()));
}

fn load_codex(records: &[Value]) -> LoadedSession {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("rollout-filter.jsonl");
  let mut lines = vec![json!({"type":"session_meta","payload":{"id":"filter-session","history_mode":"paginated"}})];
  lines.extend_from_slice(records);
  let text = lines.iter().map(|line| format!("{line}\n")).collect::<String>();
  std::fs::write(&path, text).unwrap();
  AgentClient::load_session(Source::Codex, Some(directory.path().into()), path.to_str().unwrap()).unwrap()
}

#[test]
fn actual_codex_records_keep_usage_and_content_while_projecting_routine_markers() {
  let counters =
    json!({"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":15});
  let loaded = load_codex(&[
    json!({"type":"event_msg","payload":{"type":"turn_started","turn_id":"turn-1"}}),
    json!({"type":"turn_context","payload":{"turn_id":"turn-1","model":"model"}}),
    json!({"type":"world_state","payload":{"full":false,"state":{"cwd":"/tmp"}}}),
    json!({"type":"inter_agent_communication_metadata","payload":{"trigger_turn":false}}),
    json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":counters,"last_token_usage":counters},"rate_limits":{"limit_id":"codex"}}}),
    json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"filter-session","turn_id":"turn-1","item":{"type":"Plan","id":"plan-1","text":"1. inspect\n2. fix"}}}),
    json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"filter-session","turn_id":"turn-1","item":{"type":"AgentMessage","id":"empty-1","content":[]}}}),
    json!({"type":"turn_context","payload":{}}),
    json!({"type":"event_msg","payload":{"type":"turn_complete","turn_id":"turn-1","last_agent_message":"final content"}}),
    json!({"type":"event_msg","payload":{"type":"turn_complete","turn_id":"turn-2","error":{"message":"failure"}}}),
  ]);
  let flags = loaded.events.iter().map(bookkeeping).collect::<Vec<_>>();
  assert_eq!(
    flags,
    [
      true, true, true, true, true, false, true, false, true, false, true, false, false, false
    ]
  );
  assert!(matches!(&loaded.events[5], AgentEvent::Usage(_)));
  assert!(matches!(&loaded.events[7], AgentEvent::Metadata(event) if event.native_type.ends_with(".Plan")));
  assert!(matches!(&loaded.events[9], AgentEvent::Unknown(_)));
  assert!(matches!(&loaded.events[10], AgentEvent::Lifecycle(event) if matches!(event.phase, Phase::Finished)));
  assert!(matches!(&loaded.events[11], AgentEvent::Error(error) if error.message == "failure"));
  assert!(matches!(&loaded.events[12], AgentEvent::Metadata(event) if event.native_type == "event_msg.turn_complete"));
  assert!(
    matches!(&loaded.events[13], AgentEvent::Lifecycle(event) if matches!(event.outcome, Some(LifecycleOutcome::Failed)))
  );
}

#[test]
fn actual_codex_finished_markers_hide_without_hiding_the_final_message() {
  for native_type in ["turn_complete", "task_complete"] {
    let loaded = load_codex(&[
      json!({"type":"event_msg","payload":{"type":"turn_started","turn_id":"turn-1"}}),
      json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"filter-session","turn_id":"turn-1","item":{"type":"AgentMessage","id":"message-1","phase":"final_answer","content":[{"type":"Text","text":"final reply"}]}}}),
      json!({"type":"event_msg","payload":{"type":native_type,"turn_id":"turn-1","last_agent_message":"final reply"}}),
    ]);
    let final_message = loaded
      .events
      .iter()
      .find(|event| matches!(event, AgentEvent::Message(_)))
      .unwrap();
    assert!(!bookkeeping(final_message));
    let finished = loaded.events.last().unwrap();
    assert!(matches!(finished, AgentEvent::Lifecycle(event) if matches!(event.phase, Phase::Finished)));
    assert!(bookkeeping(finished));
  }
}

#[test]
fn synthetic_rows_stay_visible_and_projection_keeps_raw_page_boundaries() {
  let events = vec![
    lifecycle_event(),
    usage_event(UsageKind::SessionSnapshot, Provider::Codex),
    tool_call(
      Provider::Codex,
      "shell",
      "tool-1",
      ToolKind::Shell,
      None,
      Phase::Finished,
      None,
    ),
    message_event("done"),
  ];
  let timeline = timeline_entries(&events);
  let TimelineEntry::Trajectory { trajectory } = &timeline[0] else {
    panic!("expected trajectory")
  };
  assert!(!trajectory_event_summary(trajectory, &events).is_bookkeeping);
  let tool = trajectory
    .entries
    .iter()
    .find(|entry| matches!(entry, TimelineEntry::ToolOperation { .. }))
    .unwrap();
  assert!(!timeline_entry_event_summary(tool, &events, &ActivityTargets::default(), &HashSet::new()).is_bookkeeping);

  let service = service_with_session(loaded_session(vec![
    settings_event(),
    usage_event(UsageKind::SessionSnapshot, Provider::Codex),
  ]));
  let page = service
    .load_event_page(EventPageRequest {
      window_mode: None,
      session_key: key_for("fixture"),
      cursor: None,
      offset: None,
      direction: PageDirection::Forward,
      limit: Some(1),
    })
    .unwrap();
  assert_eq!(page.events.len(), 1);
  assert!(page.events[0].is_bookkeeping);
  assert_eq!(page.total_events, 2);
  assert_eq!(decode_event_key(&page.events[0].event_key).unwrap(), 0);
  assert_eq!(decode_event_cursor(page.next_cursor.as_deref().unwrap()).unwrap(), 1);
}
