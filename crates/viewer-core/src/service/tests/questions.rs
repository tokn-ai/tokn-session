use super::*;
use tokn_session_codex::normalize::CodexNormalizer;

#[test]
fn questions_stay_outside_work_trajectories_and_keep_structured_native_detail() {
  let mut normalizer = CodexNormalizer::new();
  let events: Vec<_> = include_str!("../../../../codex/fixtures/questions.jsonl")
    .lines()
    .flat_map(|line| normalizer.normalize(serde_json::from_str(line).unwrap()))
    .collect();
  let index = events
    .iter()
    .position(|event| matches!(event, AgentEvent::QuestionRequest(_)))
    .unwrap();
  let entries = timeline_entries(&events);
  assert!(
    entries
      .iter()
      .any(|entry| matches!(entry, TimelineEntry::Event { source_event_index } if *source_event_index == index))
  );
  assert!(!crate::service::event_filter::is_bookkeeping(&events[index]));
  let summary = event_summary_with_delegation_targets(
    &events,
    index,
    &events[index],
    &ActivityTargets::default(),
    &HashSet::new(),
  );
  assert_eq!(summary.event_type, "question_request");
  assert_eq!(summary.summary, "Which **storage engine**?");
  assert!(summary.role.is_none());
  assert!(summary.trajectory.is_none());
  assert_eq!(
    native_detail(&events[index]).unwrap()["item"]["questions"][1]["future_hint"],
    "preserved"
  );
  let value = serde_json::to_value(&events[index]).unwrap();
  assert_eq!(value["questions"][0]["options"][0]["label"], "SQLite");
  assert_eq!(value["is_blocking"], false);
}

#[test]
fn parsed_answers_are_visible_user_rows_with_linked_question_and_native_result() {
  let mut normalizer = CodexNormalizer::new();
  let events: Vec<_> = include_str!("../../../../codex/fixtures/question_replies.jsonl")
    .lines()
    .flat_map(|line| normalizer.normalize(serde_json::from_str(line).unwrap()))
    .collect();
  let index = events
    .iter()
    .position(|event| matches!(event, AgentEvent::QuestionReply(_)))
    .unwrap();
  assert!(
    timeline_entries(&events)
      .iter()
      .any(|entry| matches!(entry, TimelineEntry::Event { source_event_index } if *source_event_index == index))
  );
  let summary = event_summary_with_delegation_targets(
    &events,
    index,
    &events[index],
    &ActivityTargets::default(),
    &HashSet::new(),
  );
  assert_eq!(summary.event_type, "question_reply");
  assert_eq!(summary.role.as_deref(), Some("user"));
  assert_eq!(summary.summary, "SQLite");
  assert!(!summary.is_bookkeeping);
  assert!(summary.trajectory.is_none());
  let value = serde_json::to_value(&events[index]).unwrap();
  assert_eq!(value["request_id"], "call-1");
  assert_eq!(value["replies"][0]["question"], "Which storage engine?");
  assert_eq!(value["replies"][0]["answers"][1], "Keep it **local**.");
  assert_eq!(native_detail(&events[index]).unwrap()["future_hint"], "retained");
}
