use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::Value;
use tokn_session_core::{
  AgentActivity, AgentEvent, LifecycleEvent, LifecycleOutcome, LifecycleScope, LiveSessionEvent, LoadedSession,
  LoadedSessionTree, Phase, Role, SessionHistoryStatus, SessionRef, SessionSettingsApplied, TerminalAction,
  ToolCallEvent, ToolKind, ToolOperation, ToolOperationStatus, ToolSummary, UsageEvent, UsageKind,
  assemble_tool_operations,
};

pub struct EventDisplay {
  pub kind: &'static str,
  pub summary: String,
  pub detail: String,
}

pub fn display_event(event: &AgentEvent) -> EventDisplay {
  if event.is_hidden() {
    return EventDisplay {
      kind: event_type(event),
      summary: render_event_summary(event),
      detail: render_event_pretty(event),
    };
  }
  let mut detail = render_event_pretty(event);
  // Browser expansion is explicit inspection; linear pretty output stays compact.
  let native = match event {
    AgentEvent::Metadata(event) => Some(&event.native),
    AgentEvent::Lifecycle(event) => Some(&event.native),
    AgentEvent::Usage(event) => Some(&event.native),
    _ => None,
  };
  if let Some(native) = native {
    write_indented(&mut detail, &format!("native: {native}"));
  }
  let provenance = match event {
    AgentEvent::Message(event) => event.provenance.as_ref(),
    AgentEvent::Reasoning(event) => event.provenance.as_ref(),
    _ => None,
  };
  if let Some(provenance) = provenance {
    write_indented(
      &mut detail,
      &format!("provenance: {}", serde_json::to_string(provenance).unwrap()),
    );
  }
  EventDisplay {
    kind: event_type(event),
    summary: render_event_summary(event),
    detail,
  }
}

pub fn render_session_list(sessions: &[SessionRef]) -> String {
  let mut output = String::new();
  output.push_str("id                                    updated_at                 messages  cwd\n");
  for session in sessions {
    output.push_str(&format!(
      "{:<36}  {:<25} {:>8}  {}\n",
      session.id,
      session.timestamp.as_deref().unwrap_or("-"),
      session.message_count,
      session.cwd.as_deref().unwrap_or("-"),
    ));
  }
  output
}

pub fn render_agent_jsonl(events: &[AgentEvent]) -> Result<String, String> {
  let mut output = String::new();
  for event in events {
    let line = serde_json::to_string(event).map_err(|err| format!("failed to serialize event: {err}"))?;
    output.push_str(&line);
    output.push('\n');
  }
  Ok(output)
}

pub fn render_session_jsonl(session: &LoadedSession) -> Result<String, String> {
  if session.history_status == SessionHistoryStatus::SubagentBodyUnavailable {
    return Err(format!(
      "subagent session `{}` body is unavailable because no trigger-turn boundary was recorded",
      session.reference.id
    ));
  }

  render_agent_jsonl(&session.events)
}

pub fn render_live_event_pretty(event: &LiveSessionEvent) -> String {
  let mut output = String::new();
  match event {
    LiveSessionEvent::Started(event) => {
      output.push_str(&format!("Session {}\n", event.session_id));
      output.push_str(&format!("cwd: {}\n\n", event.cwd.as_deref().unwrap_or("-")));
    }
    LiveSessionEvent::Event(event) => output.push_str(&render_event_pretty(event)),
    LiveSessionEvent::Finished(event) => {
      output.push_str("session finished");
      if !event.success {
        output.push_str(" error");
      }
      if let Some(exit_code) = event.exit_code {
        output.push_str(&format!(" exit={exit_code}"));
      }
      output.push_str("\n\n");
    }
    LiveSessionEvent::Unknown(event) => {
      output.push_str(&format!(
        "unknown {}\n",
        event.native_type.as_deref().unwrap_or("event")
      ));
      if let Some(native) = &event.native {
        write_indented(&mut output, &format!("native: {native}"));
      }
      output.push('\n');
    }
  }
  output
}

pub fn render_pretty(session: &LoadedSession) -> String {
  let mut output = String::new();
  output.push_str(&format!("Session {}\n", session.reference.id));
  if let Some(parent_session_id) = &session.reference.parent_session_id {
    output.push_str(&format!("parent: {parent_session_id}\n"));
  }
  if let Some(agent_path) = &session.reference.agent_path {
    output.push_str(&format!("agent: {agent_path}\n"));
  }
  if let Some(agent_nickname) = &session.reference.agent_nickname {
    output.push_str(&format!("nickname: {agent_nickname}\n"));
  }
  if let Some(agent_role) = &session.reference.agent_role {
    output.push_str(&format!("role: {agent_role}\n"));
  }
  output.push_str(&format!("cwd: {}\n", session.reference.cwd.as_deref().unwrap_or("-")));
  output.push_str(&format!(
    "updated_at: {}\n",
    session.reference.timestamp.as_deref().unwrap_or("-")
  ));
  if session.history_status == SessionHistoryStatus::SubagentBodyUnavailable {
    output.push_str("warning: subagent body unavailable; no trigger-turn boundary was recorded\n");
  }
  output.push('\n');

  let mut operations_by_timeline_source = HashMap::new();
  let mut hidden_tool_sources = HashSet::new();
  for operation in assemble_tool_operations(&session.events) {
    let Some(source_event_index) = operation.timeline_source_event_index() else {
      continue;
    };
    hidden_tool_sources.extend(
      operation
        .source_event_indices
        .iter()
        .copied()
        .filter(|index| *index != source_event_index),
    );
    operations_by_timeline_source.insert(source_event_index, operation);
  }

  for (source_event_index, event) in session.events.iter().enumerate() {
    if let Some(operation) = operations_by_timeline_source.remove(&source_event_index) {
      render_tool_operation(&mut output, &operation);
    } else if !hidden_tool_sources.contains(&source_event_index) {
      output.push_str(&render_event_pretty(event));
    }
  }

  output
}

pub fn render_session_tree(tree: &LoadedSessionTree) -> String {
  let mut output = String::new();
  output.push_str("Session tree\n");
  output.push_str("selected ");
  output.push_str(&session_tree_identity(&tree.session.reference));
  output.push('\n');
  for (index, child) in tree.children.iter().enumerate() {
    render_session_tree_outline(&mut output, child, "", index + 1 == tree.children.len());
  }
  output.push('\n');
  render_session_tree_node(&mut output, tree, 0);
  output
}

fn render_session_tree_outline(output: &mut String, tree: &LoadedSessionTree, prefix: &str, is_last: bool) {
  output.push_str(prefix);
  output.push_str(if is_last { "└─ " } else { "├─ " });
  output.push_str(&session_tree_identity(&tree.session.reference));
  output.push('\n');

  let child_prefix = format!("{prefix}{}", if is_last { "   " } else { "│  " });
  for (index, child) in tree.children.iter().enumerate() {
    render_session_tree_outline(output, child, &child_prefix, index + 1 == tree.children.len());
  }
}

fn session_tree_identity(reference: &SessionRef) -> String {
  match (&reference.agent_nickname, &reference.agent_path) {
    (Some(nickname), Some(path)) => format!("{nickname} ({path}) [{}]", reference.id),
    (Some(nickname), None) => format!("{nickname} [{}]", reference.id),
    (None, Some(path)) => format!("{path} [{}]", reference.id),
    (None, None) => reference.id.clone(),
  }
}

fn render_session_tree_node(output: &mut String, tree: &LoadedSessionTree, depth: usize) {
  if !output.is_empty() && !output.ends_with("\n\n") {
    output.push('\n');
  }

  if depth == 0 {
    output.push_str("=== Selected session ===\n\n");
  } else if let Some(identity) = tree
    .session
    .reference
    .agent_nickname
    .as_deref()
    .or(tree.session.reference.agent_path.as_deref())
  {
    output.push_str(&format!("=== Subagent {identity} ===\n\n"));
  } else if matches!(
    tree.session.history_status,
    SessionHistoryStatus::FilteredSubagent | SessionHistoryStatus::SubagentBodyUnavailable
  ) {
    output.push_str(&format!("=== Subagent {} ===\n\n", tree.session.reference.id));
  } else if depth == 1 {
    output.push_str(&format!("=== Child session {} ===\n\n", tree.session.reference.id));
  } else {
    output.push_str(&format!("=== Descendant {} ===\n\n", tree.session.reference.id));
  }
  output.push_str(&render_pretty(&tree.session));

  for child in &tree.children {
    render_session_tree_node(output, child, depth + 1);
  }
}

pub fn render_event_pretty(event: &AgentEvent) -> String {
  if event.is_hidden() {
    return "[hidden provider content]\n\n".into();
  }
  let mut output = String::new();
  match event {
    AgentEvent::SessionStarted(_) => {}
    AgentEvent::ProviderChanged(event) => {
      if let Some(model_id) = &event.model_id {
        let provider = event.model_provider.as_deref().unwrap_or("model");
        output.push_str(&format!("[model] {provider}/{model_id}\n\n"));
      }
      if let Some(level) = &event.thinking_level {
        output.push_str(&format!("[thinking] {level}\n\n"));
      }
    }
    AgentEvent::SessionSettingsApplied(event) => render_session_settings(&mut output, event),
    AgentEvent::Compaction(event) => {
      output.push_str(event.state.label());
      if let Some(summary) = &event.summary {
        output.push_str("\n");
        output.push_str(summary);
      }
      output.push_str("\n\n");
    }
    AgentEvent::Lifecycle(_) | AgentEvent::Usage(_) | AgentEvent::Metadata(_) => {
      output.push_str(&render_event_summary(event));
      output.push_str("\n\n");
    }
    AgentEvent::Message(event) => {
      output.push_str(role_label(event.role));
      output.push('\n');
      write_indented(&mut output, &event.text);
      output.push('\n');
    }
    AgentEvent::QuestionRequest(event) => {
      output.push_str(match event.is_blocking {
        Some(true) => "questions (blocking)\n",
        Some(false) => "questions (async)\n",
        None => "questions\n",
      });
      for question in &event.questions {
        let heading = question
          .header
          .as_deref()
          .filter(|header| !header.is_empty())
          .map(|header| format!("{header}: {}", question.question))
          .unwrap_or_else(|| question.question.clone());
        write_indented(&mut output, &heading);
        for option in question.options.iter().flatten() {
          let description = option.description.as_deref().filter(|text| !text.is_empty());
          write_indented(
            &mut output,
            &description
              .map(|text| format!("- {}: {text}", option.label))
              .unwrap_or_else(|| format!("- {}", option.label)),
          );
        }
        if question.allows_free_text {
          write_indented(&mut output, "Free-text answer allowed");
        }
        if question.is_secret {
          write_indented(&mut output, "Secret answer requested");
        }
      }
      output.push('\n');
    }
    AgentEvent::Reasoning(event) => {
      if let Some(summary) = &event.summary {
        output.push_str("reasoning summary\n");
        write_indented(&mut output, summary);
        output.push('\n');
      }
      if let Some(text) = &event.text {
        output.push_str("reasoning\n");
        write_indented(&mut output, text);
        output.push('\n');
      }
    }
    AgentEvent::GoalUpdated(event) => {
      output.push_str("goal updated");
      if let Some(status) = event.goal.as_ref().and_then(|goal| goal_string(goal, "status")) {
        output.push_str(&format!(" [{status}]"));
      }
      if let Some(tokens_used) = event.goal.as_ref().and_then(|goal| goal_number(goal, "tokensUsed")) {
        output.push_str(&format!(" tokens={tokens_used}"));
      }
      if let Some(time_used_seconds) = event
        .goal
        .as_ref()
        .and_then(|goal| goal_number(goal, "timeUsedSeconds"))
      {
        output.push_str(&format!(" time={time_used_seconds}s"));
      }
      output.push('\n');
      if let Some(objective) = event.goal.as_ref().and_then(|goal| goal_string(goal, "objective")) {
        write_indented(&mut output, objective);
      } else if let Some(goal) = &event.goal {
        write_indented(&mut output, &goal.to_string());
      }
      output.push('\n');
    }
    AgentEvent::AgentActivity(event) => {
      output.push_str(&render_agent_activity_summary(event));
      output.push('\n');
      if let Some(communication) = &event.communication {
        if let Some(text) = &communication.text {
          write_indented(&mut output, text);
        }
        if communication.has_encrypted_content {
          write_indented(&mut output, "[Encrypted content unavailable]");
        }
      }
      output.push('\n');
    }
    AgentEvent::ToolCall(event) => {
      render_tool(&mut output, event);
    }
    AgentEvent::Error(event) => {
      output.push_str("error\n");
      write_indented(&mut output, &event.message);
      output.push('\n');
    }
    AgentEvent::Unknown(event) => {
      output.push_str(&format!(
        "unknown {}\n",
        event.native_type.as_deref().unwrap_or("event")
      ));
      if let Some(native) = &event.native {
        write_indented(&mut output, &format!("native: {native}"));
      }
      output.push('\n');
    }
  }
  output
}

pub fn render_event_summary(event: &AgentEvent) -> String {
  if event.is_hidden() {
    return "[hidden provider content]".into();
  }
  match event {
    AgentEvent::SessionStarted(event) => format!("session started {}", event.session_id),
    AgentEvent::ProviderChanged(event) => {
      if let Some(model_id) = &event.model_id {
        let provider = event.model_provider.as_deref().unwrap_or("model");
        format!("model {provider}/{model_id}")
      } else if let Some(level) = &event.thinking_level {
        format!("thinking {level}")
      } else {
        "provider changed".to_string()
      }
    }
    AgentEvent::SessionSettingsApplied(event) => render_session_settings_summary(event),
    AgentEvent::Lifecycle(event) => render_lifecycle(event),
    AgentEvent::Usage(event) => render_usage(event),
    AgentEvent::Compaction(event) => event.state.label().to_string(),
    AgentEvent::Metadata(event) => format!("[{}] {}", event.native_type, first_line(&event.summary)),
    AgentEvent::Message(event) => format!("{} {}", role_label(event.role), first_line(&event.text)),
    AgentEvent::QuestionRequest(event) => format!(
      "{} {}",
      match event.is_blocking {
        Some(true) => "questions (blocking)",
        Some(false) => "questions (async)",
        None => "questions",
      },
      event
        .questions
        .first()
        .map(|question| first_line(&question.question))
        .unwrap_or_default()
    ),
    AgentEvent::Reasoning(event) => {
      if let Some(summary) = &event.summary {
        format!("reasoning summary {}", first_line(summary))
      } else if let Some(text) = &event.text {
        format!("reasoning {}", first_line(text))
      } else {
        "reasoning encrypted".to_string()
      }
    }
    AgentEvent::GoalUpdated(event) => {
      let mut summary = "goal updated".to_string();
      if let Some(status) = event.goal.as_ref().and_then(|goal| goal_string(goal, "status")) {
        summary.push_str(&format!(" [{status}]"));
      }
      if let Some(objective) = event.goal.as_ref().and_then(|goal| goal_string(goal, "objective")) {
        summary.push(' ');
        summary.push_str(first_line(objective));
      }
      summary
    }
    AgentEvent::AgentActivity(event) => render_agent_activity_summary(event),
    AgentEvent::ToolCall(event) => render_tool_summary(event).unwrap_or_else(|| {
      let mut summary = "tool".to_string();
      if let Some(name) = &event.tool_name {
        summary.push(' ');
        summary.push_str(name);
      }
      append_tool_id(&mut summary, event.tool_call_id.as_deref());
      summary
    }),
    AgentEvent::Error(event) => format!("error {}", first_line(&event.message)),
    AgentEvent::Unknown(event) => format!("unknown {}", event.native_type.as_deref().unwrap_or("event")),
  }
}

pub fn event_type(event: &AgentEvent) -> &'static str {
  match event {
    AgentEvent::SessionStarted(_) => "session",
    AgentEvent::ProviderChanged(_) => "provider",
    AgentEvent::SessionSettingsApplied(_) => "settings",
    AgentEvent::Lifecycle(_) => "lifecycle",
    AgentEvent::Usage(_) => "usage",
    AgentEvent::Compaction(_) => "compaction",
    AgentEvent::Metadata(_) => "metadata",
    AgentEvent::Message(_) => "message",
    AgentEvent::QuestionRequest(_) => "questions",
    AgentEvent::Reasoning(_) => "reasoning",
    AgentEvent::GoalUpdated(_) => "goal",
    AgentEvent::AgentActivity(_) => "agent",
    AgentEvent::ToolCall(_) => "tool",
    AgentEvent::Error(_) => "error",
    AgentEvent::Unknown(_) => "unknown",
  }
}

fn render_lifecycle(event: &LifecycleEvent) -> String {
  let identity = match event.scope {
    LifecycleScope::Turn => format!("turn {}", event.turn_id),
    LifecycleScope::Step => format!(
      "turn {} step {}",
      event.turn_id,
      event.step_id.as_deref().unwrap_or("-")
    ),
  };
  let status = match event.outcome {
    Some(LifecycleOutcome::Completed) => "completed",
    Some(LifecycleOutcome::Cancelled) => "cancelled",
    Some(LifecycleOutcome::Interrupted) => "interrupted",
    Some(LifecycleOutcome::Blocked) => "blocked",
    Some(LifecycleOutcome::Failed) => "failed",
    Some(LifecycleOutcome::TokenLimit) => "token limit",
    None => match event.phase {
      Phase::Started => "started",
      Phase::Finished => "ended",
      Phase::Delta | Phase::Updated => "updated",
    },
  };
  format!("[{identity}] {status}")
}

fn render_usage(event: &UsageEvent) -> String {
  let label = match event.kind {
    UsageKind::ModelCall => "usage",
    UsageKind::OperationTotal => "usage operation total",
    UsageKind::SessionSnapshot => "usage session snapshot",
  };
  let mut summary = format!("[{label}] input={} output={}", event.input_tokens, event.output_tokens);
  for (label, value) in [
    ("total", event.total_tokens),
    ("cache_read", event.cache_read_tokens),
    ("cache_write", event.cache_write_tokens),
    ("reasoning", event.reasoning_tokens),
  ] {
    if let Some(value) = value {
      summary.push_str(&format!(" {label}={value}"));
    }
  }
  summary
}

fn render_agent_activity_summary(event: &AgentActivity) -> String {
  let target = event.target_agent_path.as_deref().unwrap_or("unknown agent");
  let mut summary = if let Some(communication) = &event.communication {
    let actor = event
      .actor_agent_path
      .as_deref()
      .or(event.actor_session_id.as_deref())
      .unwrap_or("unknown agent");
    let recipient = event
      .target_agent_path
      .as_deref()
      .or(event.target_session_id.as_deref())
      .unwrap_or("unknown agent");
    let mut summary = format!("Message from {actor} → {recipient}");
    if communication.trigger_turn == Some(true) {
      summary.push_str(" (starts turn)");
    }
    summary
  } else {
    match event.actor_agent_path.as_deref() {
      Some(actor) => format!("{actor} → {target} {}", event.kind),
      None => match event.kind.as_str() {
        "started" => format!("agent started {target}"),
        "interacted" => format!("interaction with {target}"),
        "interrupted" => format!("agent interrupted {target}"),
        kind => format!("agent activity {kind} {target}"),
      },
    }
  };
  if let Some(event_id) = &event.event_id {
    summary.push_str(" #");
    summary.push_str(event_id);
  }
  summary
}

fn render_session_settings(output: &mut String, event: &SessionSettingsApplied) {
  output.push_str("session settings applied\n");
  if let Some(model) = joined_values(event.model_provider.as_deref(), event.model_id.as_deref(), "/") {
    write_setting(output, "model", &model);
  }
  if let Some(service_tier) = &event.service_tier {
    write_setting(output, "service tier", service_tier);
  }
  if let Some(reasoning) = joined_values(
    event.reasoning_effort.as_deref(),
    event.reasoning_summary.as_deref(),
    " / ",
  ) {
    write_setting(output, "reasoning", &reasoning);
  }
  if let Some(mode) = &event.collaboration_mode {
    write_setting(output, "mode", mode);
  }
  if let Some(personality) = &event.personality {
    write_setting(output, "personality", personality);
  }
  if let Some(approval) = joined_values(
    event.approval_policy.as_deref(),
    event.approvals_reviewer.as_deref(),
    " / ",
  ) {
    write_setting(output, "approval", &approval);
  }
  if let Some(profile) = &event.active_permission_profile_id {
    write_setting(output, "permissions", profile);
  }
  if let Some(cwd) = &event.cwd {
    write_setting(output, "cwd", cwd);
  }
  output.push('\n');
}

fn render_session_settings_summary(event: &SessionSettingsApplied) -> String {
  let mut parts = vec!["settings".to_string()];
  match (&event.model_provider, &event.model_id) {
    (Some(provider), Some(model)) => parts.push(format!("model={provider}/{model}")),
    (Some(provider), None) => parts.push(format!("provider={provider}")),
    (None, Some(model)) => parts.push(format!("model={model}")),
    (None, None) => {}
  }
  if let Some(service_tier) = &event.service_tier {
    parts.push(format!("tier={service_tier}"));
  }
  if let Some(effort) = &event.reasoning_effort {
    parts.push(format!("effort={effort}"));
  }
  if let Some(mode) = &event.collaboration_mode {
    parts.push(format!("mode={mode}"));
  }
  if let Some(cwd) = event.cwd.as_deref().and_then(cwd_name) {
    parts.push(format!("cwd={cwd}"));
  }
  parts.join(" ")
}

fn write_setting(output: &mut String, label: &str, value: &str) {
  output.push_str("  ");
  output.push_str(label);
  output.push(' ');
  output.push_str(value);
  output.push('\n');
}

fn joined_values(first: Option<&str>, second: Option<&str>, separator: &str) -> Option<String> {
  match (first, second) {
    (Some(first), Some(second)) => Some(format!("{first}{separator}{second}")),
    (Some(first), None) => Some(first.to_string()),
    (None, Some(second)) => Some(second.to_string()),
    (None, None) => None,
  }
}

fn cwd_name(cwd: &str) -> Option<&str> {
  Path::new(cwd)
    .file_name()
    .and_then(|name| name.to_str())
    .filter(|name| !name.is_empty())
}

fn render_tool(output: &mut String, event: &ToolCallEvent) {
  if let Some(line) = render_tool_summary(event) {
    output.push_str(&line);
    output.push('\n');
    if let Some(detail) = render_tool_detail(event) {
      write_indented(output, &detail);
    }
    output.push('\n');
    return;
  }

  output.push_str("tool");
  if let Some(name) = &event.tool_name {
    output.push(' ');
    output.push_str(name);
  }
  if let Some(id) = &event.tool_call_id {
    output.push_str(" #");
    output.push_str(id);
  }
  if event.is_error == Some(true) {
    output.push_str(" error");
  }
  output.push('\n');
  if let Some(input) = &event.input {
    write_indented(output, &format!("input: {input}"));
  }
  if let Some(output_value) = &event.output {
    write_indented(output, &format!("output: {output_value}"));
  }
  output.push('\n');
}

/// Render one historical logical operation. The source event stream remains
/// append-only for JSONL and live consumers, but pretty output should not
/// repeat a provider's invocation, progress, and result fragments.
fn render_tool_operation(output: &mut String, operation: &ToolOperation) {
  if let Some(line) = render_tool_operation_summary(operation) {
    output.push_str(&line);
    output.push('\n');
    if matches!(operation.status, ToolOperationStatus::Failed)
      && let Some(detail) = operation.output.as_ref().map(|output| format!("output: {output}"))
    {
      write_indented(output, &detail);
    }
    output.push('\n');
    return;
  }

  output.push_str("tool");
  if let Some(name) = operation
    .tool_name
    .as_deref()
    .or(operation.provider_tool_name.as_deref())
  {
    output.push(' ');
    output.push_str(name);
  }
  if let Some(id) = &operation.tool_call_id {
    output.push_str(" #");
    output.push_str(id);
  }
  if matches!(operation.status, ToolOperationStatus::Failed) {
    output.push_str(" error");
  }
  output.push('\n');
  if let Some(input) = &operation.input {
    write_indented(output, &format!("input: {input}"));
  }
  if let Some(output_value) = &operation.output {
    write_indented(output, &format!("output: {output_value}"));
  }
  output.push('\n');
}

fn render_tool_summary(event: &ToolCallEvent) -> Option<String> {
  render_tool_summary_parts(
    event.summary.as_ref(),
    event.tool_kind,
    tool_status(event),
    event.tool_call_id.as_deref(),
  )
}

fn render_tool_operation_summary(operation: &ToolOperation) -> Option<String> {
  render_tool_summary_parts(
    operation.summary.as_ref(),
    operation.tool_kind,
    tool_operation_status(operation.status),
    operation.tool_call_id.as_deref(),
  )
}

fn render_tool_summary_parts(
  summary: Option<&ToolSummary>,
  tool_kind: ToolKind,
  status: &str,
  tool_call_id: Option<&str>,
) -> Option<String> {
  let mut line = match summary {
    Some(ToolSummary::CodeExecution { language }) => {
      let language = language.as_deref().unwrap_or("code");
      format!("code{status} {language}")
    }
    Some(ToolSummary::Shell {
      command,
      cwd: _,
      exit_code,
    }) => {
      let mut line = format!("shell{status}");
      if let Some(exit_code) = exit_code {
        line.push_str(&format!(" exit={exit_code}"));
      }
      if let Some(command) = command {
        line.push(' ');
        line.push_str(command);
      }
      line
    }
    Some(ToolSummary::Terminal {
      session_id,
      action,
      chars_len,
      wait_ms,
    }) => {
      let mut line = match action {
        Some(TerminalAction::Wait) => "terminal wait".to_string(),
        Some(TerminalAction::Send) => format!("terminal send {} chars", chars_len.unwrap_or(0)),
        None => "terminal".to_string(),
      };
      line.push_str(status);
      if let Some(session_id) = session_id {
        line.push_str(&format!(" session={session_id}"));
      }
      if let Some(wait_ms) = wait_ms {
        line.push_str(&format!(" wait={wait_ms}ms"));
      }
      line
    }
    Some(ToolSummary::FileRead { path }) => format!("read{status} {}", path.as_deref().unwrap_or("-")),
    Some(ToolSummary::FileWrite { path, bytes }) => {
      let mut line = format!("write{status} {}", path.as_deref().unwrap_or("-"));
      if let Some(bytes) = bytes {
        line.push_str(&format!(" {bytes}b"));
      }
      line
    }
    Some(ToolSummary::FileEdit { path, added, removed }) => {
      let mut line = format!("edit{status} {}", path.as_deref().unwrap_or("-"));
      if added.is_some() || removed.is_some() {
        line.push_str(&format!(" +{} -{}", added.unwrap_or(0), removed.unwrap_or(0)));
      }
      line
    }
    Some(ToolSummary::Search { query }) => format!("search{status} {}", query.as_deref().unwrap_or("-")),
    Some(ToolSummary::Web { url }) => format!("web{status} {}", url.as_deref().unwrap_or("-")),
    Some(ToolSummary::Task { title }) => format!("task{status} {}", title.as_deref().unwrap_or("-")),
    None => match tool_kind {
      ToolKind::CodeExecution => Some(format!("code{status}")),
      ToolKind::Shell => Some(format!("shell{status}")),
      ToolKind::Terminal => Some(format!("terminal{status}")),
      ToolKind::FileRead => Some(format!("read{status}")),
      ToolKind::FileWrite => Some(format!("write{status}")),
      ToolKind::FileEdit => Some(format!("edit{status}")),
      ToolKind::Search => Some(format!("search{status}")),
      ToolKind::Web => Some(format!("web{status}")),
      ToolKind::Task => Some(format!("task{status}")),
      ToolKind::Unknown => None,
    }?,
  };
  append_tool_id(&mut line, tool_call_id);
  Some(line)
}

fn append_tool_id(line: &mut String, tool_call_id: Option<&str>) {
  if let Some(id) = tool_call_id {
    line.push_str(" #");
    line.push_str(id);
  }
}

fn render_tool_detail(event: &ToolCallEvent) -> Option<String> {
  if event.is_error == Some(true) {
    return event.output.as_ref().map(|output| format!("output: {output}"));
  }
  match event.phase {
    Phase::Started | Phase::Updated => None,
    Phase::Delta | Phase::Finished => None,
  }
}

fn tool_status(event: &ToolCallEvent) -> &'static str {
  if event.is_error == Some(true) {
    return " error";
  }
  match event.phase {
    Phase::Started => " started",
    Phase::Updated => " running",
    Phase::Delta => " delta",
    Phase::Finished => "",
  }
}

fn tool_operation_status(status: ToolOperationStatus) -> &'static str {
  match status {
    ToolOperationStatus::Pending => " pending",
    ToolOperationStatus::Running => " running",
    ToolOperationStatus::Completed => "",
    ToolOperationStatus::Failed => " error",
  }
}

fn goal_string<'a>(goal: &'a Value, field: &str) -> Option<&'a str> {
  goal.get(field).and_then(Value::as_str)
}

fn goal_number(goal: &Value, field: &str) -> Option<u64> {
  goal.get(field).and_then(Value::as_u64)
}

fn role_label(role: Role) -> &'static str {
  match role {
    Role::User => "user",
    Role::Assistant => "assistant",
    Role::System => "system",
    Role::Tool => "tool",
    Role::Unknown => "message",
  }
}

fn write_indented(output: &mut String, text: &str) {
  for line in text.trim_matches('\n').lines() {
    output.push_str("  ");
    output.push_str(line);
    output.push('\n');
  }
}

fn first_line(text: &str) -> &str {
  text.trim().lines().next().unwrap_or("")
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use serde_json::json;
  use tokn_session_core::{
    AgentActivity, AgentCommunication, AgentEvent, GoalUpdated, LoadedSession, MessageDelivery, MessageEvent, Provider,
    ProviderChanged, ReasoningEvent, SessionRef, SessionSettingsApplied, ToolRecordKind, ToolTransport, UnknownEvent,
  };

  use super::*;

  #[test]
  fn display_event_exposes_summary_and_detail() {
    let event = AgentEvent::ToolCall(ToolCallEvent {
      provider: Provider::Codex,
      session_id: Some("session".to_string()),
      turn_id: None,
      message_id: None,
      parent_id: None,
      record_kind: ToolRecordKind::Snapshot,
      tool_call_id: Some("call".to_string()),
      provider_tool_name: Some("exec_command".to_string()),
      tool_name: Some("exec_command".to_string()),
      tool_kind: ToolKind::Shell,
      transport: Some(ToolTransport::Native),
      summary: Some(ToolSummary::Shell {
        command: Some("cargo check".to_string()),
        cwd: None,
        exit_code: None,
      }),
      phase: Phase::Finished,
      input: None,
      output: None,
      is_error: None,
      native: None,
      timestamp: None,
    });

    let display = display_event(&event);

    assert_eq!(display.kind, "tool");
    assert_eq!(display.summary, "shell cargo check #call");
    assert_eq!(display.detail, "shell cargo check #call\n\n");
  }

  #[test]
  fn render_pretty_summarizes_shell_tools() {
    let session = loaded_session(vec![AgentEvent::ToolCall(ToolCallEvent {
      provider: Provider::Codex,
      session_id: Some("session".to_string()),
      turn_id: None,
      message_id: None,
      parent_id: None,
      record_kind: ToolRecordKind::Invocation,
      tool_call_id: Some("call".to_string()),
      provider_tool_name: Some("exec_command".to_string()),
      tool_name: Some("exec_command".to_string()),
      tool_kind: ToolKind::Shell,
      transport: Some(ToolTransport::Native),
      summary: Some(ToolSummary::Shell {
        command: Some("cargo test".to_string()),
        cwd: None,
        exit_code: None,
      }),
      phase: Phase::Started,
      input: Some(json!(["cargo", "test"])),
      output: None,
      is_error: None,
      native: None,
      timestamp: None,
    })]);

    let output = render_pretty(&session);

    assert!(output.contains("shell pending cargo test #call\n"));
    assert!(!output.contains("input:"));
  }

  #[test]
  fn render_pretty_assembles_tool_invocation_and_result_once() {
    let session = loaded_session(vec![
      AgentEvent::ToolCall(ToolCallEvent {
        provider: Provider::Codex,
        session_id: Some("session".to_string()),
        turn_id: Some("turn".to_string()),
        message_id: None,
        parent_id: None,
        record_kind: ToolRecordKind::Invocation,
        tool_call_id: Some("call".to_string()),
        provider_tool_name: Some("exec".to_string()),
        tool_name: Some("write_stdin".to_string()),
        tool_kind: ToolKind::Terminal,
        transport: Some(ToolTransport::CodeExecution),
        summary: Some(ToolSummary::Terminal {
          session_id: Some("90855".to_string()),
          action: Some(TerminalAction::Wait),
          chars_len: Some(0),
          wait_ms: Some(30_000),
        }),
        phase: Phase::Started,
        input: Some(json!({ "session_id": 90855, "chars": "" })),
        output: None,
        is_error: None,
        native: None,
        timestamp: None,
      }),
      AgentEvent::Message(MessageEvent {
        provenance: None,
        provider: Provider::Codex,
        session_id: Some("session".to_string()),
        message_id: None,
        parent_id: None,
        role: Role::Assistant,
        delivery: MessageDelivery::Commentary,
        phase: Phase::Finished,
        text: "intervening commentary".to_string(),
        timestamp: None,
      }),
      AgentEvent::ToolCall(ToolCallEvent {
        provider: Provider::Codex,
        session_id: Some("session".to_string()),
        turn_id: Some("turn".to_string()),
        message_id: None,
        parent_id: None,
        record_kind: ToolRecordKind::Result,
        tool_call_id: Some("call".to_string()),
        provider_tool_name: Some("exec".to_string()),
        tool_name: Some("write_stdin".to_string()),
        tool_kind: ToolKind::Terminal,
        transport: Some(ToolTransport::CodeExecution),
        summary: Some(ToolSummary::Terminal {
          session_id: Some("90855".to_string()),
          action: Some(TerminalAction::Wait),
          chars_len: Some(0),
          wait_ms: Some(30_000),
        }),
        phase: Phase::Finished,
        input: Some(json!({ "session_id": 90855, "chars": "" })),
        output: Some(json!({ "text": "Refreshing checks status" })),
        is_error: None,
        native: None,
        timestamp: None,
      }),
    ]);

    let output = render_pretty(&session);

    assert_eq!(output.matches("terminal wait").count(), 1);
    assert!(output.contains("terminal wait session=90855 wait=30000ms #call\n"));
    assert!(!output.contains("pending"));
    assert!(output.find("intervening commentary").unwrap() < output.find("terminal wait session=90855").unwrap());
  }

  #[test]
  fn render_event_summary_handles_message_first_line() {
    let event = AgentEvent::Message(MessageEvent {
      provenance: None,
      provider: Provider::Codex,
      session_id: Some("session".to_string()),
      message_id: None,
      parent_id: None,
      role: Role::Assistant,
      delivery: MessageDelivery::Final,
      phase: Phase::Finished,
      text: "first line\nsecond line".to_string(),
      timestamp: None,
    });

    assert_eq!(render_event_summary(&event), "assistant first line");
  }

  #[test]
  fn renders_agent_activity_as_target_when_actor_is_unknown() {
    let event = agent_activity(None);

    assert_eq!(render_event_summary(&event), "interaction with /root #call-agent");
    assert_eq!(render_event_pretty(&event), "interaction with /root #call-agent\n\n");
  }

  #[test]
  fn renders_agent_activity_direction_when_actor_is_known() {
    let event = agent_activity(Some("/root/researcher"));

    assert_eq!(
      render_event_summary(&event),
      "/root/researcher → /root interacted #call-agent"
    );
  }

  #[test]
  fn renders_agent_communication_body_without_putting_it_in_summary() {
    let mut event = agent_activity(Some("/root/researcher"));
    let AgentEvent::AgentActivity(activity) = &mut event else {
      unreachable!();
    };
    activity.communication = Some(AgentCommunication {
      text: Some("## Findings\n\n- Keep **Markdown** and `code`.\n- [Source](https://example.com)".into()),
      has_encrypted_content: false,
      trigger_turn: Some(true),
    });

    let display = display_event(&event);
    assert_eq!(
      display.summary,
      "Message from /root/researcher → /root (starts turn) #call-agent"
    );
    assert_eq!(
      display.detail,
      concat!(
        "Message from /root/researcher → /root (starts turn) #call-agent\n",
        "  ## Findings\n  \n",
        "  - Keep **Markdown** and `code`.\n",
        "  - [Source](https://example.com)\n\n"
      )
    );
  }

  #[test]
  fn renders_mixed_and_encrypted_agent_communication_without_ciphertext() {
    for text in [None, Some("Readable portion")] {
      let mut event = agent_activity(None);
      let AgentEvent::AgentActivity(activity) = &mut event else {
        unreachable!();
      };
      activity.actor_session_id = Some("sender-session".into());
      activity.communication = Some(AgentCommunication {
        text: text.map(str::to_owned),
        has_encrypted_content: true,
        trigger_turn: Some(false),
      });
      activity.native = Some(json!({"encrypted_content": "private-ciphertext"}));

      let display = display_event(&event);
      assert_eq!(display.summary, "Message from sender-session → /root #call-agent");
      assert!(display.detail.contains("  [Encrypted content unavailable]\n"));
      assert_eq!(display.detail.contains("  Readable portion\n"), text.is_some());
      assert!(!display.detail.contains("private-ciphertext"));
    }
  }

  #[test]
  fn render_pretty_handles_reasoning_and_goal_updates() {
    let session = loaded_session(vec![
      AgentEvent::Reasoning(ReasoningEvent {
        provenance: None,
        provider: Provider::Codex,
        session_id: Some("session".to_string()),
        message_id: None,
        parent_id: None,
        phase: Phase::Finished,
        text: Some("thinking".to_string()),
        summary: Some("summary".to_string()),
        redacted: None,
        encrypted_content: Some("ciphertext".to_string()),
        signature: None,
        timestamp: None,
      }),
      AgentEvent::GoalUpdated(GoalUpdated {
        provider: Provider::Codex,
        session_id: Some("session".to_string()),
        turn_id: Some("turn".to_string()),
        goal: Some(json!({
          "status": "complete",
          "objective": "finish tests",
          "tokensUsed": 12,
          "timeUsedSeconds": 3
        })),
        timestamp: None,
      }),
    ]);

    let output = render_pretty(&session);

    assert!(output.contains("reasoning summary\n  summary\n"));
    assert!(output.contains("reasoning\n  thinking\n"));
    assert!(!output.contains("ciphertext"));
    assert!(output.contains("goal updated [complete] tokens=12 time=3s\n  finish tests\n"));
  }

  #[test]
  fn render_pretty_handles_provider_changes() {
    let session = loaded_session(vec![AgentEvent::ProviderChanged(ProviderChanged {
      provider: Provider::Codex,
      session_id: Some("session".to_string()),
      native_id: None,
      native_parent_id: None,
      model_provider: Some("openai".to_string()),
      model_id: Some("gpt-5".to_string()),
      thinking_level: Some("high".to_string()),
      timestamp: None,
    })]);

    let output = render_pretty(&session);

    assert!(output.contains("[model] openai/gpt-5\n\n"));
    assert!(output.contains("[thinking] high\n\n"));
  }

  #[test]
  fn renders_session_settings_without_native_details() {
    let event = AgentEvent::SessionSettingsApplied(SessionSettingsApplied {
      provider: Provider::Codex,
      session_id: Some("session".to_string()),
      model_provider: Some("openai".to_string()),
      model_id: Some("gpt-5".to_string()),
      service_tier: Some("priority".to_string()),
      cwd: Some("/tmp/project".to_string()),
      reasoning_effort: Some("high".to_string()),
      reasoning_summary: Some("detailed".to_string()),
      personality: Some("friendly".to_string()),
      collaboration_mode: Some("default".to_string()),
      approval_policy: Some("on-request".to_string()),
      approvals_reviewer: Some("auto_review".to_string()),
      active_permission_profile_id: Some(":workspace".to_string()),
      native: Some(json!({
        "collaboration_mode": {
          "settings": {
            "developer_instructions": "sensitive instructions"
          }
        }
      })),
      timestamp: None,
    });

    assert_eq!(
      render_event_summary(&event),
      "settings model=openai/gpt-5 tier=priority effort=high mode=default cwd=project"
    );
    let pretty = render_event_pretty(&event);
    assert!(pretty.contains("session settings applied\n"));
    assert!(pretty.contains("  model openai/gpt-5\n"));
    assert!(pretty.contains("  reasoning high / detailed\n"));
    assert!(pretty.contains("  approval on-request / auto_review\n"));
    assert!(pretty.contains("  permissions :workspace\n"));
    assert!(!pretty.contains("sensitive instructions"));
  }

  #[test]
  fn render_event_summary_keeps_tool_ids() {
    let event = AgentEvent::ToolCall(ToolCallEvent {
      provider: Provider::Codex,
      session_id: Some("session".to_string()),
      turn_id: None,
      message_id: None,
      parent_id: None,
      record_kind: ToolRecordKind::Snapshot,
      tool_call_id: Some("call".to_string()),
      provider_tool_name: Some("exec_command".to_string()),
      tool_name: Some("exec_command".to_string()),
      tool_kind: ToolKind::Shell,
      transport: Some(ToolTransport::Native),
      summary: Some(ToolSummary::Shell {
        command: Some("cargo check".to_string()),
        cwd: None,
        exit_code: None,
      }),
      phase: Phase::Finished,
      input: None,
      output: None,
      is_error: None,
      native: None,
      timestamp: None,
    });

    assert_eq!(render_event_summary(&event), "shell cargo check #call");
  }

  #[test]
  fn render_pretty_summarizes_file_tools() {
    let session = loaded_session(vec![AgentEvent::ToolCall(ToolCallEvent {
      provider: Provider::Codex,
      session_id: Some("session".to_string()),
      turn_id: None,
      message_id: None,
      parent_id: None,
      record_kind: ToolRecordKind::Snapshot,
      tool_call_id: Some("call".to_string()),
      provider_tool_name: Some("apply_patch".to_string()),
      tool_name: Some("apply_patch".to_string()),
      tool_kind: ToolKind::FileEdit,
      transport: Some(ToolTransport::Native),
      summary: Some(ToolSummary::FileEdit {
        path: Some("crates/core/src/agent_event.rs".to_string()),
        added: Some(4),
        removed: Some(1),
      }),
      phase: Phase::Finished,
      input: None,
      output: None,
      is_error: None,
      native: None,
      timestamp: None,
    })]);

    let output = render_pretty(&session);

    assert!(output.contains("edit crates/core/src/agent_event.rs +4 -1 #call\n"));
  }

  #[test]
  fn render_pretty_keeps_unknown_tool_payloads_visible() {
    let session = loaded_session(vec![AgentEvent::ToolCall(ToolCallEvent {
      provider: Provider::Pi,
      session_id: Some("session".to_string()),
      turn_id: None,
      message_id: None,
      parent_id: None,
      record_kind: ToolRecordKind::Snapshot,
      tool_call_id: Some("call".to_string()),
      provider_tool_name: Some("mystery".to_string()),
      tool_name: Some("mystery".to_string()),
      tool_kind: ToolKind::Unknown,
      transport: Some(ToolTransport::Native),
      summary: None,
      phase: Phase::Finished,
      input: Some(json!({ "value": 1 })),
      output: Some(json!({ "ok": true })),
      is_error: None,
      native: None,
      timestamp: None,
    })]);

    let output = render_pretty(&session);

    assert!(output.contains("tool mystery #call\n"));
    assert!(output.contains("input: {\"value\":1}\n"));
    assert!(output.contains("output: {\"ok\":true}\n"));
  }

  #[test]
  fn render_pretty_keeps_unknown_event_payloads_visible() {
    let session = loaded_session(vec![AgentEvent::Unknown(UnknownEvent {
      provider: Provider::Codex,
      session_id: Some("session".to_string()),
      native_type: Some("event_msg.new_native_event".to_string()),
      native: Some(json!({ "type": "new_native_event", "value": 123 })),
      timestamp: None,
    })]);

    let output = render_pretty(&session);

    assert!(output.contains("unknown event_msg.new_native_event\n"));
    assert!(output.contains("native: {\"type\":\"new_native_event\",\"value\":123}\n"));
  }

  #[test]
  fn metadata_is_compact_in_pretty_but_inspectable_in_browser_and_json() {
    let event = AgentEvent::Metadata(tokn_session_core::MetadataEvent {
      provider: Provider::Dsh,
      session_id: Some("session".into()),
      kind: tokn_session_core::MetadataKind::Diagnostic,
      native_type: "session/title-llm-request".into(),
      summary: "title model request deepseek/test".into(),
      native: json!({"system":"large diagnostic prompt"}),
      timestamp: None,
    });
    assert_eq!(
      render_event_pretty(&event),
      "[session/title-llm-request] title model request deepseek/test\n\n"
    );
    let display = display_event(&event);
    assert_eq!(display.kind, "metadata");
    assert!(display.detail.contains("large diagnostic prompt"));
    assert!(
      serde_json::to_string(&event)
        .unwrap()
        .contains("large diagnostic prompt")
    );
  }

  #[test]
  fn lifecycle_renders_cancellation_and_closed_steps_without_claiming_success() {
    let mut lifecycle = LifecycleEvent {
      provider: Provider::Dsh,
      session_id: Some("session".into()),
      turn_id: "3".into(),
      step_id: Some("2".into()),
      scope: LifecycleScope::Step,
      phase: Phase::Finished,
      outcome: None,
      native: json!({"type":"step/end"}),
      timestamp: None,
    };
    assert_eq!(render_lifecycle(&lifecycle), "[turn 3 step 2] ended");
    lifecycle.scope = LifecycleScope::Turn;
    lifecycle.step_id = None;
    lifecycle.outcome = Some(LifecycleOutcome::Cancelled);
    let event = AgentEvent::Lifecycle(lifecycle);
    assert_eq!(render_event_pretty(&event), "[turn 3] cancelled\n\n");
    assert_eq!(display_event(&event).kind, "lifecycle");
    assert!(display_event(&event).detail.contains("native:"));
  }

  #[test]
  fn usage_renders_normalized_totals_without_summing_subsets() {
    let mut event = AgentEvent::Usage(UsageEvent {
      kind: UsageKind::ModelCall,
      provider: Provider::Dsh,
      session_id: Some("session".into()),
      turn_id: Some("1".into()),
      step_id: Some("1".into()),
      message_id: None,
      record_id: None,
      input_tokens: 33,
      output_tokens: 5,
      total_tokens: Some(38),
      cache_read_tokens: Some(20),
      cache_write_tokens: Some(3),
      reasoning_tokens: Some(2),
      native: json!({"inputTokens":10}),
      timestamp: None,
    });
    assert_eq!(
      render_event_summary(&event),
      "[usage] input=33 output=5 total=38 cache_read=20 cache_write=3 reasoning=2"
    );
    assert_eq!(display_event(&event).kind, "usage");
    assert!(display_event(&event).detail.contains("inputTokens"));
    if let AgentEvent::Usage(usage) = &mut event {
      usage.kind = UsageKind::SessionSnapshot;
    }
    assert!(render_event_summary(&event).starts_with("[usage session snapshot]"));
    if let AgentEvent::Usage(usage) = &mut event {
      usage.kind = UsageKind::OperationTotal;
    }
    assert!(render_event_summary(&event).starts_with("[usage operation total]"));
  }

  #[test]
  fn hidden_content_is_redacted_in_all_human_views_but_retained_in_jsonl() {
    let native = json!({"type":"custom_message","customType":"extension","display":false,"content":"secret content"});
    let events = vec![
      AgentEvent::Message(MessageEvent {
        provider: Provider::Pi,
        session_id: None,
        message_id: None,
        parent_id: None,
        role: Role::System,
        delivery: MessageDelivery::Unspecified,
        phase: Phase::Finished,
        text: "secret content".into(),
        timestamp: None,
        provenance: Some(tokn_session_core::MessageProvenance {
          source: json!({"kind":"extension"}),
          display: Some(false),
          native: Some(native.clone()),
          surface_op: None,
          source_event_seqs: None,
        }),
      }),
      AgentEvent::Unknown(tokn_session_core::UnknownEvent {
        provider: Provider::Pi,
        session_id: None,
        native_type: Some("custom_message".into()),
        native: Some(native),
        timestamp: None,
      }),
    ];
    for event in &events {
      assert_eq!(render_event_summary(event), "[hidden provider content]");
      assert_eq!(render_event_pretty(event), "[hidden provider content]\n\n");
      assert!(!display_event(event).detail.contains("secret"));
    }
    assert!(render_agent_jsonl(&events).unwrap().contains("secret content"));
  }

  #[test]
  fn render_pretty_shows_subagent_identity() {
    let mut session = loaded_session(Vec::new());
    session.reference.parent_session_id = Some("parent".to_string());
    session.reference.agent_path = Some("/root/researcher".to_string());
    session.reference.agent_nickname = Some("Hubble".to_string());
    session.reference.agent_role = Some("explorer".to_string());

    let output = render_pretty(&session);

    assert!(output.contains("parent: parent\n"));
    assert!(output.contains("agent: /root/researcher\n"));
    assert!(output.contains("nickname: Hubble\n"));
    assert!(output.contains("role: explorer\n"));
  }

  #[test]
  fn renders_session_tree_as_separate_sections() {
    let root = loaded_session(Vec::new());
    let mut child = loaded_session(Vec::new());
    child.reference.id = "child".to_string();
    child.reference.parent_session_id = Some("session".to_string());
    child.reference.agent_path = Some("/root/researcher".to_string());
    child.reference.agent_nickname = Some("Hubble".to_string());
    let tree = LoadedSessionTree {
      session: root,
      children: vec![LoadedSessionTree {
        session: child,
        children: Vec::new(),
      }],
    };

    let output = render_session_tree(&tree);

    assert!(output.contains("Session tree\nselected session\n"));
    assert!(output.contains("└─ Hubble (/root/researcher) [child]\n"));
    assert!(output.contains("=== Selected session ===\n\nSession session\n"));
    assert!(output.contains("=== Subagent Hubble ===\n\nSession child\n"));
    assert!(output.contains("parent: session\n"));
  }

  #[test]
  fn uses_provider_neutral_headings_without_agent_identity() {
    let root = loaded_session(Vec::new());
    let mut child = loaded_session(Vec::new());
    child.reference.id = "child".to_string();
    child.reference.parent_session_id = Some("session".to_string());
    let mut descendant = loaded_session(Vec::new());
    descendant.reference.id = "descendant".to_string();
    descendant.reference.parent_session_id = Some("child".to_string());
    let tree = LoadedSessionTree {
      session: root,
      children: vec![LoadedSessionTree {
        session: child,
        children: vec![LoadedSessionTree {
          session: descendant,
          children: Vec::new(),
        }],
      }],
    };

    let output = render_session_tree(&tree);

    assert!(output.contains("=== Child session child ===\n\nSession child\n"));
    assert!(output.contains("=== Descendant descendant ===\n\nSession descendant\n"));
    assert!(!output.contains("=== Subagent child ==="));
  }

  #[test]
  fn uses_subagent_heading_when_filtered_history_proves_the_identity() {
    let root = loaded_session(Vec::new());
    let mut child = loaded_session(Vec::new());
    child.reference.id = "child".to_string();
    child.reference.parent_session_id = Some("session".to_string());
    child.history_status = SessionHistoryStatus::FilteredSubagent;
    let tree = LoadedSessionTree {
      session: root,
      children: vec![LoadedSessionTree {
        session: child,
        children: Vec::new(),
      }],
    };

    let output = render_session_tree(&tree);

    assert!(output.contains("=== Subagent child ===\n\nSession child\n"));
  }

  #[test]
  fn warns_and_rejects_jsonl_when_subagent_body_is_unavailable() {
    let mut session = loaded_session(Vec::new());
    session.history_status = SessionHistoryStatus::SubagentBodyUnavailable;

    let output = render_pretty(&session);
    let error = render_session_jsonl(&session).expect_err("unavailable history must not produce misleading jsonl");

    assert!(output.contains("warning: subagent body unavailable; no trigger-turn boundary was recorded\n"));
    assert_eq!(
      error,
      "subagent session `session` body is unavailable because no trigger-turn boundary was recorded"
    );
  }

  #[test]
  fn complete_session_jsonl_keeps_the_agent_event_shape() {
    let session = loaded_session(vec![agent_activity(Some("/root"))]);

    assert_eq!(
      render_session_jsonl(&session).expect("complete session should render"),
      render_agent_jsonl(&session.events).expect("events should render")
    );
  }

  fn loaded_session(events: Vec<AgentEvent>) -> LoadedSession {
    LoadedSession {
      reference: SessionRef {
        id: "session".to_string(),
        parent_session_id: None,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
        title: None,
        preview: None,
        path: PathBuf::from("session.jsonl"),
        cwd: None,
        timestamp: None,
        message_count: 0,
      },
      events,
      history_status: SessionHistoryStatus::Complete,
    }
  }

  fn agent_activity(actor_agent_path: Option<&str>) -> AgentEvent {
    AgentEvent::AgentActivity(AgentActivity {
      provider: Provider::Codex,
      session_id: Some("child-session".to_string()),
      event_id: Some("call-agent".to_string()),
      actor_session_id: actor_agent_path.map(|_| "child-session".to_string()),
      actor_agent_path: actor_agent_path.map(str::to_string),
      target_session_id: Some("root-session".to_string()),
      target_agent_path: Some("/root".to_string()),
      kind: "interacted".to_string(),
      communication: None,
      occurred_at_ms: Some(1_784_915_647_361),
      native: None,
      timestamp: Some("2026-07-24T17:54:07.361Z".to_string()),
    })
  }
}
