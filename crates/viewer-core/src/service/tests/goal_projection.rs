use super::*;
use crate::model::GoalPresentation;
use tokn_session_codex::normalize::CodexNormalizer;

fn goal_events() -> Vec<AgentEvent> {
  let mut normalizer = CodexNormalizer::new();
  include_str!("../../../tests/fixtures/codex/goal_turn.jsonl")
    .lines()
    .flat_map(|line| normalizer.normalize(serde_json::from_str(line).unwrap()))
    .collect()
}

fn goal_page(events: Vec<AgentEvent>) -> Vec<EventSummary> {
  service_with_session(loaded_session(events))
    .load_event_page(EventPageRequest {
      session_key: key_for("fixture"),
      window_mode: None,
      cursor: None,
      offset: None,
      direction: PageDirection::Forward,
      limit: None,
    })
    .unwrap()
    .events
}

#[test]
fn presents_goal_prompt_work_and_verified_receipt_on_the_reply() {
  let page = goal_page(goal_events());
  let tagged: Vec<_> = page
    .iter()
    .filter_map(|row| Some((row.event_type.as_str(), row.goal.as_ref()?)))
    .collect();
  assert_eq!(
    tagged,
    [
      ("message", &GoalPresentation::Prompt),
      ("trajectory", &GoalPresentation::Work),
      (
        "message",
        &GoalPresentation::Completed {
          duration_ms: Some("94000".into())
        }
      ),
    ]
  );
  assert_eq!(
    page
      .iter()
      .find(|row| row.goal == Some(GoalPresentation::Prompt))
      .unwrap()
      .summary,
    "Make the release checklist reproducible."
  );
}

#[test]
fn completing_a_turn_is_not_a_receipt_and_a_late_other_turn_receipt_does_not_apply() {
  let mut events = goal_events();
  let receipt = events
    .iter_mut()
    .find_map(|event| match event {
      AgentEvent::GoalUpdated(g) if g.goal.as_ref().is_some_and(|g| g["status"] == "complete") => Some(g),
      _ => None,
    })
    .expect("fixture must contain a verified goal receipt");
  receipt.turn_id = Some("earlier-turn".into());
  let page = goal_page(events);
  let reply = page
    .iter()
    .find(|row| row.role.as_deref() == Some("assistant"))
    .unwrap();
  assert!(reply.goal.is_none());
}

#[test]
fn unrelated_and_cleared_native_goals_do_not_mark_the_final_reply() {
  for cleared in [false, true] {
    let mut events = goal_events();
    let index = events
      .iter()
      .position(|event| {
        matches!(event, AgentEvent::GoalUpdated(g)
      if g.goal.as_ref().is_some_and(|g| g["status"] == "complete"))
      })
      .unwrap();
    let AgentEvent::GoalUpdated(receipt) = &mut events[index] else {
      unreachable!()
    };
    if cleared {
      let mut update = receipt.clone();
      update.goal = None;
      events.insert(index + 1, AgentEvent::GoalUpdated(update));
    } else {
      receipt.goal.as_mut().unwrap()["threadId"] = json!("another-thread");
    }
    let page = goal_page(events);
    assert!(
      page
        .iter()
        .find(|row| row.role.as_deref() == Some("assistant"))
        .unwrap()
        .goal
        .is_none()
    );
  }
}
