//! Native diagnostic severity stays separate from turn and tool completion.
use super::*;
use tokn_session_core::{MetadataEvent, MetadataKind};

pub(super) fn error_message(payload: &Value) -> Option<String> {
  string_field(payload, "message").filter(|message| !message.trim().is_empty())
}

pub(super) fn error(
  session_id: Option<String>,
  error: &Value,
  native: &Value,
  kind: &str,
  timestamp: Option<String>,
) -> Vec<AgentEvent> {
  let Some(message) = error_message(error) else {
    return vec![unknown_event(
      session_id,
      Some(format!("event_msg.{kind}")),
      Some(native.clone()),
      timestamp,
    )];
  };
  // The newer upstream protocol's ErrorEvent::affects_turn_status excludes
  // these request rejections: they do not fail the active agent turn. Keep the
  // discriminator exact rather than interpreting display text as severity.
  if kind == "error" && nonterminal_request_error(error) {
    return vec![metadata(
      session_id,
      native,
      kind,
      format!("Codex request rejected: {message}"),
      timestamp,
    )];
  }
  vec![
    AgentEvent::Error(ErrorEvent {
      provider: Provider::Codex,
      session_id: session_id.clone(),
      message,
      native: Some(native.clone()),
      timestamp: timestamp.clone(),
    }),
    metadata(session_id, native, kind, "Codex error".into(), timestamp),
  ]
}

fn nonterminal_request_error(payload: &Value) -> bool {
  let info = &payload["codex_error_info"];
  if info == "thread_rollback_failed" {
    return true;
  }
  info.as_object().is_some_and(|info| {
    info.len() == 1
      && matches!(
        info
          .get("active_turn_not_steerable")
          .and_then(|details| details.get("turn_kind"))
          .and_then(Value::as_str),
        Some("review" | "compact")
      )
  })
}

/// TurnAbortedEvent has an optional identity. A supported native reason still
/// ends its response when the identity is absent; malformed IDs are different
/// from that explicitly supported optional form and stay inspectable unknowns.
pub(super) fn abort_turn_id(payload: &Value) -> Option<Option<&str>> {
  if !matches!(
    payload.get("reason").and_then(Value::as_str),
    Some("interrupted" | "replaced" | "review_ended" | "budget_limited")
  ) {
    return None;
  }
  match payload.get("turn_id").or_else(|| payload.get("turnId")) {
    None | Some(Value::Null) => Some(None),
    Some(Value::String(id)) if !id.trim().is_empty() => Some(Some(id)),
    _ => None,
  }
}

pub(super) fn notice(
  session_id: Option<String>,
  payload: &Value,
  kind: &str,
  timestamp: Option<String>,
) -> Vec<AgentEvent> {
  let Some(message) = error_message(payload) else {
    return vec![unknown_event(
      session_id,
      Some(format!("event_msg.{kind}")),
      Some(payload.clone()),
      timestamp,
    )];
  };
  let label = match kind {
    "stream_error" => "Codex stream retry",
    "guardian_warning" => "Codex guardian warning",
    _ => "Codex warning",
  };
  vec![metadata(
    session_id,
    payload,
    kind,
    format!("{label}: {message}"),
    timestamp,
  )]
}

pub(super) fn interrupted(session_id: Option<String>, payload: &Value, timestamp: Option<String>) -> Vec<AgentEvent> {
  let Some(reason) = string_field(payload, "reason").filter(|reason| !reason.trim().is_empty()) else {
    return vec![unknown_event(
      session_id,
      Some("event_msg.turn_aborted".into()),
      Some(payload.clone()),
      timestamp,
    )];
  };
  vec![metadata(
    session_id,
    payload,
    "turn_aborted",
    format!("Codex turn interrupted: {reason}"),
    timestamp,
  )]
}

fn metadata(
  session_id: Option<String>,
  native: &Value,
  kind: &str,
  summary: String,
  timestamp: Option<String>,
) -> AgentEvent {
  AgentEvent::Metadata(MetadataEvent {
    provider: Provider::Codex,
    session_id,
    kind: MetadataKind::Diagnostic,
    native_type: format!("event_msg.{kind}"),
    summary,
    native: native.clone(),
    timestamp,
  })
}
