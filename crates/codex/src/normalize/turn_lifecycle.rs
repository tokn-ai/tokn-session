use super::*;
use tokn_session_core::{LifecycleEvent, LifecycleOutcome, LifecycleScope};

pub(super) fn normalize(
  session_id: Option<String>,
  payload: &Value,
  kind: &str,
  timestamp: Option<String>,
) -> AgentEvent {
  let Some(turn_id) = string_field_any(payload, &["turn_id", "turnId"]).filter(|id| !id.trim().is_empty()) else {
    return unknown_event(
      session_id,
      Some(format!("event_msg.{kind}")),
      Some(payload.clone()),
      timestamp,
    );
  };
  let started = matches!(kind, "task_started" | "turn_started");
  AgentEvent::Lifecycle(LifecycleEvent {
    provider: Provider::Codex,
    session_id,
    turn_id,
    step_id: None,
    scope: LifecycleScope::Turn,
    phase: if started { Phase::Started } else { Phase::Finished },
    outcome: if started {
      None
    } else if kind == "turn_aborted" {
      Some(LifecycleOutcome::Interrupted)
    } else if let Some(error) = payload.get("error").filter(|error| !error.is_null()) {
      diagnostics::error_message(error).map(|_| LifecycleOutcome::Failed)
    } else {
      Some(LifecycleOutcome::Completed)
    },
    native: payload.clone(),
    timestamp,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn turn_boundaries_preserve_identity_and_unknown_payload_fields() {
    for kind in [
      "task_started",
      "turn_started",
      "task_complete",
      "turn_complete",
      "turn_aborted",
    ] {
      let mut payload = json!({"type": kind, "turn_id": "turn-1", "future": 42});
      if kind == "turn_aborted" {
        payload["reason"] = json!("interrupted");
      }
      let event = normalize_event_message(
        Some("session".into()),
        EventMessage {
          event_type: Some(kind.into()),
          native: payload.clone(),
        },
        false,
        Some("123".into()),
      );
      let AgentEvent::Lifecycle(lifecycle) = event.last().unwrap() else {
        panic!("expected lifecycle")
      };
      assert_eq!(lifecycle.turn_id, "turn-1");
      assert_eq!(lifecycle.native, payload);
      assert_eq!(lifecycle.phase == Phase::Started, kind.ends_with("started"));
      assert_eq!(lifecycle.outcome.is_some(), !kind.ends_with("started"));
    }
  }

  #[test]
  fn missing_turn_identity_stays_unknown_instead_of_inventing_a_turn() {
    assert!(matches!(
      normalize(None, &json!({"type":"task_started"}), "task_started", None),
      AgentEvent::Unknown(_)
    ));
  }
}
