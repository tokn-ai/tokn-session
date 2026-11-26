use super::*;
use tokn_session_core::{
  CompactionEvent, CompactionState, GoalUpdated, LifecycleEvent, LifecycleScope, MessageProvenance,
};

fn marker(provider: Provider) -> AgentEvent {
  AgentEvent::Lifecycle(LifecycleEvent {
    provider,
    session_id: Some("fixture".into()),
    turn_id: "turn".into(),
    step_id: None,
    scope: LifecycleScope::Turn,
    phase: Phase::Started,
    outcome: None,
    native: json!({}),
    timestamp: Some("2026-01-01T00:00:01Z".into()),
  })
}

fn goal_marker() -> AgentEvent {
  let mut event = marker(Provider::Codex);
  let AgentEvent::Lifecycle(marker) = &mut event else {
    unreachable!()
  };
  marker.native = json!({"turn_attribution": {"turn_trigger": "goal"}});
  event
}

fn compaction(provider: Provider) -> AgentEvent {
  let mut c = CompactionEvent::new(provider, Some("fixture".into()), CompactionState::Completed);
  c.timestamp = Some("2026-01-01T00:00:03Z".into());
  AgentEvent::Compaction(c)
}

fn goal(status: &str, timestamp: &str) -> AgentEvent {
  AgentEvent::GoalUpdated(GoalUpdated {
    provider: Provider::Codex,
    session_id: Some("fixture".into()),
    turn_id: Some("turn".into()),
    goal: Some(json!({"objective":"Review the reader","status":status})),
    timestamp: Some(timestamp.into()),
  })
}

fn page(events: &[AgentEvent]) -> EventPage {
  service_with_session(loaded_session(events.to_vec()))
    .load_event_page(EventPageRequest {
      session_key: key_for("fixture"),
      window_mode: None,
      cursor: None,
      offset: None,
      direction: PageDirection::Forward,
      limit: None,
    })
    .unwrap()
}

#[test]
fn identified_turn_keeps_compaction_and_accounting_in_one_stable_inspectable_run() {
  // The native active goal may omit turn_id. In that case the immediately
  // preceding, user-authored goal is the evidence that it belongs here.
  let mut prompt = message_event_with_role("Review the reader", Role::User, MessageDelivery::Unspecified);
  if let AgentEvent::Message(message) = &mut prompt {
    message.provenance = Some(MessageProvenance {
      source: json!(MessageEvent::CODEX_USER_GOAL),
      display: None,
      native: None,
      surface_op: None,
      source_event_seqs: None,
    });
  }
  let mut active_goal = goal("active", "2026-01-01T00:00:00Z");
  if let AgentEvent::GoalUpdated(g) = &mut active_goal {
    g.turn_id = None;
  }
  let mut events = vec![
    prompt,
    active_goal,
    marker(Provider::Codex),
    with_timestamp(
      message_event_with_role("Reviewing", Role::Assistant, MessageDelivery::Commentary),
      "2026-01-01T00:00:02Z",
    ),
    compaction(Provider::Codex),
  ];
  let active_page = page(&events);
  assert_eq!(active_page.events.len(), 2);
  assert_eq!(active_page.events[1].goal, Some(crate::model::GoalPresentation::Work));
  let work_key = &active_page.events[1].event_key;
  assert_eq!(work_key, &encode_trajectory_key(2));
  assert_eq!(active_page.events[1].trajectory.as_ref().unwrap().status, "working");

  events.extend([
    with_timestamp(
      message_event_with_role("Checked", Role::Assistant, MessageDelivery::Commentary),
      "2026-01-01T00:00:11Z",
    ),
    goal("complete", "2026-01-01T00:00:12Z"),
    with_timestamp(message_event("Final answer"), "2026-01-01T00:00:16Z"),
    with_timestamp(
      usage_event(UsageKind::ModelCall, Provider::Codex),
      "2026-01-01T00:00:17Z",
    ),
    with_timestamp(
      usage_event(UsageKind::SessionSnapshot, Provider::Codex),
      "2026-01-01T00:00:18Z",
    ),
  ]);
  let finished = page(&events);
  assert_eq!(
    finished
      .events
      .iter()
      .map(|row| row.event_type.as_str())
      .collect::<Vec<_>>(),
    ["message", "trajectory", "message"]
  );
  let work = &finished.events[1];
  assert_eq!(&work.event_key, work_key);
  let card = work.trajectory.as_ref().unwrap();
  assert_eq!(card.status, "complete");
  assert_eq!(card.duration_ms.as_deref(), Some("10000"));
  assert_eq!(card.usage_count, 2);

  let directory = tempfile::tempdir().unwrap();
  let session_key = key_for_cached_source(&directory, "fixture");
  let service = service_with_session(loaded_session(events));
  let load_children = |cursor| {
    service
      .load_trajectory_event_page(LoadTrajectoryEventPageRequest {
        session_key: session_key.clone(),
        trajectory_key: work_key.clone(),
        cursor,
        offset: None,
        direction: PageDirection::Forward,
        limit: Some(4),
      })
      .unwrap()
  };
  let before = load_children(None);
  let after = load_children(before.next_cursor);
  let children: Vec<_> = before.events.iter().chain(&after.events).collect();
  assert_eq!(
    children.iter().map(|c| c.event_type.as_str()).collect::<Vec<_>>(),
    [
      "goal_updated",
      "lifecycle",
      "message",
      "compaction",
      "message",
      "goal_updated",
      "usage",
      "usage",
    ]
  );
  assert_eq!(children[3].event_key, encode_event_key(4));
  assert!(children[3].compaction.is_some());
  assert_eq!(children[7].event_key, encode_event_key(9));
}

#[test]
fn compaction_requires_a_matching_codex_turn_and_accounting_cannot_cross_turns() {
  let final_reply = message_event("Final answer");
  let mut foreign_usage = usage_event(UsageKind::ModelCall, Provider::Codex);
  let AgentEvent::Usage(ref mut usage) = foreign_usage else {
    unreachable!()
  };
  usage.turn_id = Some("different-turn".into());
  let isolated = page(&[goal_marker(), final_reply.clone(), foreign_usage]);
  assert_eq!(isolated.events.last().unwrap().event_type, "usage");
  for provider in [Provider::Pi, Provider::Dsh] {
    let rows = page(&[marker(provider), compaction(provider), final_reply.clone()]);
    assert_eq!(rows.events[1].event_type, "compaction");
  }
  let standalone = page(&[compaction(Provider::Codex)]);
  assert_eq!(standalone.events[0].event_type, "compaction");
}

#[test]
fn ordinary_codex_work_includes_time_spent_generating_its_final_reply() {
  let events = [
    marker(Provider::Codex),
    with_timestamp(
      message_event_with_role("Checking", Role::Assistant, MessageDelivery::Commentary),
      "2026-01-01T00:00:02Z",
    ),
    with_timestamp(message_event("Final answer"), "2026-01-01T00:02:01Z"),
    with_timestamp(
      usage_event(UsageKind::SessionSnapshot, Provider::Codex),
      "2026-01-01T00:02:03Z",
    ),
  ];
  let response = page(&events);
  assert_eq!(response.events.len(), 3);
  let work = response.events[0].trajectory.as_ref().unwrap();
  assert_eq!(work.duration_ms.as_deref(), Some("120000"));
  assert_eq!(work.usage_count, 0);
  assert_eq!(response.events[2].event_type, "usage");
}

#[test]
fn a_question_separates_work_without_ending_native_turn_ownership() {
  let mut normalizer = tokn_session_codex::normalize::CodexNormalizer::new();
  let question = include_str!("../../../tests/fixtures/codex/questions.jsonl")
    .lines()
    .flat_map(|line| normalizer.normalize(serde_json::from_str(line).unwrap()))
    .find(|event| matches!(event, AgentEvent::QuestionRequest(_)))
    .unwrap();
  let mut history = vec![goal_marker(), question];
  let before_reply = history.clone();
  history.extend([
    message_event_with_role("Continuing", Role::Assistant, MessageDelivery::Commentary),
    compaction(Provider::Codex),
    message_event_with_role("Finished checking", Role::Assistant, MessageDelivery::Commentary),
    message_event("Answer"),
    usage_event(UsageKind::ModelCall, Provider::Codex),
  ]);
  let response = page(&history);
  assert_eq!(
    response
      .events
      .iter()
      .map(|row| row.event_type.as_str())
      .collect::<Vec<_>>(),
    ["trajectory", "question_request", "trajectory", "message"]
  );
  assert_eq!(response.events[2].trajectory.as_ref().unwrap().usage_count, 1);

  // Without more work, its token report must not create a second work group.
  let mut history = before_reply;
  history.extend([
    message_event("Answer"),
    usage_event(UsageKind::ModelCall, Provider::Codex),
  ]);
  assert_eq!(
    page(&history)
      .events
      .iter()
      .map(|row| row.event_type.as_str())
      .collect::<Vec<_>>(),
    ["trajectory", "question_request", "message", "usage"]
  );
}

#[test]
fn a_late_tool_result_keeps_its_own_key_and_cannot_move_the_goal_work_back_to_its_invocation() {
  let shell = |phase, timestamp| {
    with_timestamp(
      tool_call(
        Provider::Codex,
        "exec_command",
        "background",
        ToolKind::Shell,
        Some(ToolSummary::Shell {
          command: Some("check".into()),
          cwd: None,
          exit_code: None,
        }),
        phase,
        None,
      ),
      timestamp,
    )
  };
  let mut history = vec![
    shell(Phase::Started, "2026-01-01T00:00:00Z"),
    message_event("Earlier answer"),
    message_event_with_role("Review", Role::User, MessageDelivery::Unspecified),
    with_timestamp(goal_marker(), "2026-01-01T00:00:10Z"),
    with_timestamp(
      message_event_with_role("Reviewing", Role::Assistant, MessageDelivery::Commentary),
      "2026-01-01T00:00:11Z",
    ),
  ];
  let key = encode_trajectory_key(3);
  assert_eq!(page(&history).events.last().unwrap().event_key, key);
  history.extend([
    shell(Phase::Finished, "2026-01-01T00:00:12Z"),
    with_timestamp(message_event("Reviewed"), "2026-01-01T00:00:14Z"),
  ]);
  let finished = page(&history);
  let work = finished.events.iter().find(|row| row.event_key == key).unwrap();
  let summary = work.trajectory.as_ref().unwrap();
  assert_eq!(summary.started_at.as_deref(), Some("2026-01-01T00:00:10Z"));
  assert_eq!(summary.duration_ms.as_deref(), Some("2000"));
  assert_eq!(summary.tool_count, 1);
  let group = trajectory_for_start(&history, 3).unwrap();
  assert!(group.entries.iter().any(|row| matches!(
    row,
    TimelineEntry::ToolOperation {
      source_event_index: 0,
      ..
    }
  )));
}

#[test]
fn a_repeated_native_start_stays_in_the_same_active_goal() {
  let mut events = vec![
    goal_marker(),
    with_timestamp(
      message_event_with_role("Checking", Role::Assistant, MessageDelivery::Commentary),
      "2026-01-01T00:00:03Z",
    ),
    with_timestamp(goal_marker(), "2026-01-01T00:00:05Z"),
  ];
  let active = page(&events);
  assert_eq!(active.events.len(), 1);
  assert_eq!(active.events[0].trajectory.as_ref().unwrap().status, "working");
  events.push(with_timestamp(message_event("Answer"), "2026-01-01T00:00:07Z"));
  let done = page(&events);
  assert_eq!(done.events.len(), 2);
  assert_eq!(done.events[0].event_key, active.events[0].event_key);
  let work = done.events[0].trajectory.as_ref().unwrap();
  assert_eq!(work.lifecycle_count, 2);
  assert_eq!(work.duration_ms.as_deref(), Some("2000"));
}
