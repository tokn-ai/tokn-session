use super::*;
use tokn_session_client::{AgentClient, Source};

fn boundary(id: &str, phase: Phase) -> AgentEvent {
  let AgentEvent::Lifecycle(mut event) = lifecycle_event() else {
    unreachable!()
  };
  event.turn_id = id.into();
  event.phase = phase;
  AgentEvent::Lifecycle(event)
}

fn usage(kind: UsageKind) -> AgentEvent {
  usage_event(kind, Provider::Codex)
}

fn flags(events: &[AgentEvent]) -> Vec<bool> {
  events
    .iter()
    .enumerate()
    .filter_map(|(index, event)| {
      matches!(event, AgentEvent::Usage(_)).then(|| event_summary(events, index, event).is_bookkeeping)
    })
    .collect()
}

#[test]
fn final_accounting_replaces_mid_turn_usage_without_combining_scopes() {
  let events = vec![
    boundary("turn-1", Phase::Started),
    message_event_with_role("prompt", Role::User, MessageDelivery::Unspecified),
    usage(UsageKind::ModelCall),
    usage(UsageKind::SessionSnapshot),
    message_event_with_role("working", Role::Assistant, MessageDelivery::Commentary),
    usage(UsageKind::ModelCall),
    usage(UsageKind::SessionSnapshot),
    message_event("answer"),
    usage(UsageKind::ModelCall),
    usage(UsageKind::SessionSnapshot),
    boundary("turn-1", Phase::Finished),
  ];
  assert_eq!(flags(&events), [true, true, true, true, false, false]);
  // Final message arrives before its accounting. Keep the last available
  // counters until the terminal records replace them in a newer snapshot.
  assert_eq!(flags(&events[..8]), [true, true, false, false]);
}

#[test]
fn active_usage_hides_until_completion_even_without_a_final_message() {
  let mut events = vec![
    boundary("turn-1", Phase::Started),
    usage(UsageKind::ModelCall),
    usage(UsageKind::SessionSnapshot),
    usage(UsageKind::ModelCall),
    usage(UsageKind::SessionSnapshot),
  ];
  assert_eq!(flags(&events), [true, true, true, true]);
  events.push(boundary("turn-1", Phase::Finished));
  assert_eq!(flags(&events), [true, true, false, false]);
  events.push(boundary("turn-2", Phase::Started));
  events.push(usage(UsageKind::ModelCall));
  assert_eq!(flags(&events), [true, true, false, false, true]);
}

#[test]
fn orphan_accounting_and_unrelated_turn_ids_are_preserved() {
  let mut unrelated = usage(UsageKind::ModelCall);
  if let AgentEvent::Usage(usage) = &mut unrelated {
    usage.turn_id = Some("older".into());
  }
  let mut step_finish = boundary("turn-1", Phase::Finished);
  if let AgentEvent::Lifecycle(lifecycle) = &mut step_finish {
    lifecycle.scope = LifecycleScope::Step;
  }
  let events = vec![
    usage(UsageKind::ModelCall),
    boundary("turn-1", Phase::Started),
    usage(UsageKind::ModelCall),
    unrelated,
    step_finish,
    boundary("older", Phase::Finished),
  ];
  assert_eq!(flags(&events), [false, true, false]);
}

#[test]
fn inferred_turns_and_new_prompts_keep_separate_final_accounting() {
  let events = vec![
    message_event_with_role("prompt", Role::User, MessageDelivery::Unspecified),
    usage(UsageKind::ModelCall),
    message_event("answer"),
    usage(UsageKind::ModelCall),
    message_event_with_role("next prompt", Role::User, MessageDelivery::Unspecified),
    usage(UsageKind::ModelCall),
    message_event("next answer"),
    usage(UsageKind::ModelCall),
  ];
  assert_eq!(flags(&events), [true, false, true, false]);
}

#[test]
fn model_changes_and_message_level_finals_do_not_split_accounting() {
  let mut changed = settings_event();
  if let AgentEvent::SessionSettingsApplied(settings) = &mut changed {
    settings.model_id = Some("another-model".into());
  }
  let provider_changed = AgentEvent::ProviderChanged(ProviderChanged {
    model_origin: None,
    provider: Provider::Dsh,
    session_id: Some("fixture".into()),
    native_id: None,
    native_parent_id: None,
    model_provider: None,
    model_id: Some("another-model".into()),
    thinking_level: None,
    timestamp: None,
  });
  let events = vec![
    boundary("turn-1", Phase::Started),
    usage(UsageKind::ModelCall),
    provider_changed,
    changed,
    usage(UsageKind::ModelCall),
    boundary("turn-1", Phase::Finished),
  ];
  assert_eq!(flags(&events), [true, false]);

  // Some providers mark each assistant message Final, including progress.
  // Only the final accounting per kind in the inferred user turn survives.
  let events = vec![
    message_event_with_role("prompt", Role::User, MessageDelivery::Unspecified),
    message_event("first message"),
    usage(UsageKind::ModelCall),
    message_event("second message"),
    usage(UsageKind::ModelCall),
  ];
  assert_eq!(flags(&events), [true, false]);
}

#[test]
fn inferred_turn_tracks_usage_identity_and_ignores_late_completion() {
  let mut first = usage(UsageKind::ModelCall);
  if let AgentEvent::Usage(usage) = &mut first {
    usage.turn_id = Some("current".into());
  }
  let mut other = usage(UsageKind::ModelCall);
  if let AgentEvent::Usage(usage) = &mut other {
    usage.turn_id = Some("older".into());
  }
  let mut events = vec![
    message_event_with_role("prompt", Role::User, MessageDelivery::Unspecified),
    first,
    other,
    boundary("older", Phase::Finished),
  ];
  assert_eq!(flags(&events), [true, false]);
  events.push(boundary("current", Phase::Finished));
  assert_eq!(flags(&events), [false, false]);
  events.push(message_event_with_role(
    "next",
    Role::User,
    MessageDelivery::Unspecified,
  ));
  events.push(usage(UsageKind::ModelCall));
  assert_eq!(flags(&events), [false, false, true]);
}

#[test]
fn usage_classification_spans_compaction_and_both_page_kinds() {
  let events = vec![
    boundary("turn-1", Phase::Started),
    usage(UsageKind::ModelCall),
    AgentEvent::Compaction(tokn_session_core::CompactionEvent::new(
      Provider::Codex,
      None,
      tokn_session_core::CompactionState::Completed,
    )),
    usage(UsageKind::ModelCall),
    message_event("answer"),
    usage(UsageKind::ModelCall),
    boundary("turn-1", Phase::Finished),
  ];
  let directory = tempfile::tempdir().unwrap();
  let session_key = key_for_cached_source(&directory, "fixture");
  let service = service_with_session(loaded_session(events));
  let page = service
    .load_event_page(EventPageRequest {
      window_mode: None,
      session_key: session_key.clone(),
      cursor: None,
      offset: None,
      direction: PageDirection::Forward,
      limit: Some(1),
    })
    .unwrap();
  assert_eq!(page.total_events, 6);
  let trajectory_key = page.events[0].event_key.clone();
  let child_page = service
    .load_trajectory_event_page(LoadTrajectoryEventPageRequest {
      session_key: session_key.clone(),
      trajectory_key,
      cursor: None,
      offset: Some(1),
      direction: PageDirection::Forward,
      limit: Some(1),
    })
    .unwrap();
  assert_eq!(child_page.events.len(), 1);
  assert_eq!(child_page.events[0].event_type, "usage");
  assert!(child_page.events[0].is_bookkeeping);
  assert_eq!(child_page.total_events, 2);
  let final_usage = service
    .load_event_page(EventPageRequest {
      window_mode: None,
      session_key,
      cursor: None,
      offset: Some(4),
      direction: PageDirection::Forward,
      limit: Some(1),
    })
    .unwrap();
  assert_eq!(final_usage.events[0].event_type, "usage");
  assert!(!final_usage.events[0].is_bookkeeping);
  assert!(final_usage.next_cursor.is_some());
}

#[test]
fn normalized_codex_accounting_follows_turn_boundaries_and_terminal_records() {
  let counters = |total: u64| {
    json!({
      "input_tokens": total - 5,
      "cached_input_tokens": 0,
      "output_tokens": 5,
      "reasoning_output_tokens": 0,
      "total_tokens": total,
    })
  };
  let accounting = |turn: &str, response: &str, call: u64, total: u64| {
    [
      json!({"type":"token_usage_record","payload":{
        "turn_id":turn,"response_id":response,"usage":counters(call),
        "thread_token_usage":counters(total),
      }}),
      json!({"type":"event_msg","payload":{"type":"token_count","info":{
        "total_token_usage":counters(total),"last_token_usage":counters(call),
      }}}),
    ]
  };
  let message = |role: &str, phase: Option<&str>, text: &str| {
    let content_type = if role == "user" { "input_text" } else { "output_text" };
    json!({"type":"response_item","payload":{
      "type":"message","role":role,"phase":phase,
      "content":[{"type":content_type,"text":text}],
    }})
  };
  let lifecycle = |kind: &str, turn: &str| json!({"type":"event_msg","payload":{"type":kind,"turn_id":turn}});
  let mut records = vec![
    json!({"type":"session_meta","payload":{"id":"usage-filter-session","history_mode":"paginated"}}),
    lifecycle("task_started", "turn-1"),
    // Codex starts the turn before its model settings and prompt. Neither
    // may split the accounting group created by the explicit boundary.
    json!({"type":"turn_context","payload":{"turn_id":"turn-1","model":"model"}}),
    message("user", None, "First prompt"),
  ];
  records.extend(accounting("turn-1", "response-1", 15, 15));
  // Duplicate starts with the same ID are observations of the active turn.
  records.push(lifecycle("turn_started", "turn-1"));
  records.push(message("assistant", Some("commentary"), "Working"));
  records.extend(accounting("turn-1", "response-2", 20, 35));
  records.push(message("assistant", Some("final_answer"), "First answer"));
  records.extend(accounting("turn-1", "response-3", 25, 60));
  records.push(json!({"type":"event_msg","payload":{
    "type":"task_complete","turn_id":"turn-1","last_agent_message":"First answer",
  }}));
  records.push(lifecycle("task_started", "turn-2"));
  records.push(message("user", None, "Second prompt"));
  records.extend(accounting("turn-2", "response-4", 10, 70));
  records.extend(accounting("turn-2", "response-5", 12, 82));
  // A completed tool-only turn has no final assistant message to infer from.
  records.push(lifecycle("task_complete", "turn-2"));
  records.push(lifecycle("task_started", "turn-3"));
  records.push(message("user", None, "Third prompt"));
  records.extend(accounting("turn-3", "response-6", 13, 95));

  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("rollout-usage-filter.jsonl");
  std::fs::write(
    &path,
    records.iter().map(|record| format!("{record}\n")).collect::<String>(),
  )
  .unwrap();
  let loaded = AgentClient::load_session(Source::Codex, Some(directory.path().into()), path.to_str().unwrap()).unwrap();
  assert!(
    !loaded
      .events
      .iter()
      .any(|event| matches!(event, AgentEvent::Unknown(_)))
  );
  assert_eq!(
    flags(&loaded.events),
    [
      true, true, true, true, false, false, true, true, false, false, true, true
    ]
  );
  let summaries = loaded
    .events
    .iter()
    .enumerate()
    .map(|(index, event)| event_summary(&loaded.events, index, event))
    .collect::<Vec<_>>();
  let visible_usage = summaries
    .iter()
    .filter(|event| !event.is_bookkeeping)
    .filter_map(|event| event.usage.as_ref())
    .map(|usage| (usage.kind.as_str(), usage.total_tokens.as_deref()))
    .collect::<Vec<_>>();
  assert_eq!(
    visible_usage,
    [
      ("model_call", Some("25")),
      ("session_snapshot", Some("60")),
      ("model_call", Some("12")),
      ("session_snapshot", Some("82")),
    ]
  );
}
