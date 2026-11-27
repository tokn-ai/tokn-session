//! Conservative presentation policy for the viewer's bookkeeping filter.
//! Keep this separate from ingestion and pagination: filtering must not alter
//! source event keys, native inspection, or the normalized event stream.

use serde_json::Value;
use tokn_session_core::{AgentEvent, LifecycleOutcome, MetadataEvent, MetadataKind, Phase, Provider};

pub(super) fn is_bookkeeping(event: &AgentEvent) -> bool {
  match event {
    AgentEvent::SessionStarted(_)
    | AgentEvent::ProviderChanged(_)
    | AgentEvent::SessionSettingsApplied(_)
    | AgentEvent::Usage(_) => true,
    AgentEvent::Lifecycle(event) => {
      let has_completion_echo = matches!(event.provider, Provider::Codex)
        && matches!(event.phase, Phase::Finished)
        && matches!(event.outcome, Some(LifecycleOutcome::Completed))
        && matches!(
          event.native.get("type").and_then(Value::as_str),
          Some("task_complete" | "turn_complete")
        );
      matches!(event.outcome, None | Some(LifecycleOutcome::Completed))
        && !native_has_content(&event.native, has_completion_echo)
    }
    AgentEvent::Metadata(event) => routine_metadata(event),
    // Encrypted deliveries, tools, and new event variants remain visible by default.
    _ => false,
  }
}

fn routine_metadata(event: &MetadataEvent) -> bool {
  // MetadataKind describes storage purpose, not whether content is present.
  // Context includes branch summaries, plans, hooks, and attachments; queue
  // records can contain undelivered user input; diagnostics can hold model
  // request bodies. Only known bookkeeping contracts belong in this list.
  match (event.provider, event.kind, event.native_type.as_str()) {
    (Provider::Codex, MetadataKind::Configuration, "turn_context")
    | (Provider::Codex, MetadataKind::Context, "world_state" | "inter_agent_communication_metadata")
    | (Provider::Codex, MetadataKind::Diagnostic, "event_msg.token_count")
    | (Provider::Pi, MetadataKind::Configuration, "active_tools_change")
    | (Provider::Pi, MetadataKind::Context, "leaf")
    | (Provider::Pi, MetadataKind::Session, "session_info" | "label")
    | (Provider::Dsh, MetadataKind::Configuration, "permission/preset" | "sandbox/mode" | "approval/policy")
    | (Provider::Dsh, MetadataKind::Context, "request/context")
    | (Provider::Dsh, MetadataKind::Session, "session/end-seed" | "session/title")
    | (Provider::WorkBuddy, MetadataKind::Session, "ai-title")
    | (Provider::OpenCode | Provider::ZCode, MetadataKind::Configuration, "runtime/bash_shell_selection") => true,
    (
      Provider::Codex,
      MetadataKind::Context,
      "event_msg.item_started.AgentMessage"
      | "event_msg.item_completed.AgentMessage"
      | "event_msg.item_started.SubAgentActivity",
    ) => !native_has_content(&event.native, false),
    _ => false,
  }
}

fn native_has_content(value: &Value, has_completion_echo: bool) -> bool {
  match value {
    Value::Object(object) => object.iter().any(|(key, value)| match key.as_str() {
      // Some provider completion records still report Completed while retaining
      // an error in their native payload. Preserve those even if is_error is false.
      "error" if !value.is_null() => true,
      // Codex copies its final response into the turn-completion marker. This
      // echo does not give the lifecycle card additional conversation content.
      "last_agent_message" if has_completion_echo && matches!(value, Value::Null | Value::String(_)) => false,
      "text" | "last_agent_message" if meaningful_text(value) => true,
      _ => native_has_content(value, false),
    }),
    Value::Array(values) => values.iter().any(|value| native_has_content(value, false)),
    _ => false,
  }
}

fn meaningful_text(value: &Value) -> bool {
  match value {
    Value::Null => false,
    Value::String(text) => !text.trim().is_empty(),
    Value::Array(values) => !values.is_empty(),
    Value::Object(values) => !values.is_empty(),
    // Unexpected shapes under a text field should remain inspectable.
    Value::Bool(_) | Value::Number(_) => true,
  }
}
