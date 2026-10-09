use serde_json::{Value, json};
use tokn_session_core::{
  AgentActivity, AgentEvent, MessageEvent, MetadataEvent, MetadataKind, Phase, Provider, Role, ToolCallEvent, ToolKind,
  ToolSummary, patch_summary, tool_kind_for_name, tool_summary_for_io,
};

use super::{
  codex_message_delivery, command_text, message_event, path_field, string_field, tool_record_kind_for_phase,
  unknown_event,
};

fn string_array(value: &Value) -> Option<Vec<String>> {
  value
    .as_array()?
    .iter()
    .map(|value| value.as_str().map(str::to_string))
    .collect()
}

fn optional_object(value: &Value, field: &str) -> Option<Option<Value>> {
  match value.get(field) {
    None | Some(Value::Null) => Some(None),
    Some(Value::Object(object)) => Some(Some(Value::Object(object.clone()))),
    Some(_) => None,
  }
}

pub(super) fn normalize_legacy_item_completed(
  session_id: Option<String>,
  payload: &Value,
  timestamp: Option<String>,
) -> Vec<AgentEvent> {
  let item = payload.get("item").filter(|item| item.is_object());
  let item_type = item.and_then(|item| item.get("type")).and_then(Value::as_str);
  match (item_type, item) {
    (Some("AgentMessage"), Some(item))
      if item.get("questions").is_some_and(|value| !value.is_null())
        || item.get("delivery").is_some_and(|value| !value.is_null()) =>
    {
      normalize_canonical_agent_message(
        session_id.clone(),
        "item_completed",
        item,
        Phase::Finished,
        payload,
        timestamp.clone(),
      )
      .unwrap_or_else(|| unknown_item_lifecycle(session_id, "item_completed", Some("AgentMessage"), payload, timestamp))
    }
    (Some("Plan"), Some(item)) if valid_plan_item(item) => vec![item_lifecycle_metadata(
      session_id,
      "item_completed",
      "Plan",
      "plan completed",
      payload,
      timestamp,
    )],
    (Some("Extension"), Some(item)) if string_field(item, "kind").as_deref() == Some("clock.sleep") => {
      normalize_extension_item(session_id.clone(), item, Phase::Finished, timestamp.clone()).unwrap_or_else(|| {
        vec![unknown_event(
          session_id,
          Some("event_msg.item_completed".to_string()),
          Some(payload.clone()),
          timestamp,
        )]
      })
    }
    _ => vec![unknown_event(
      session_id,
      Some("event_msg.item_completed".to_string()),
      Some(payload.clone()),
      timestamp,
    )],
  }
}

pub(super) fn normalize_item_lifecycle(
  session_id: Option<String>,
  payload: &Value,
  phase: Phase,
  timestamp: Option<String>,
) -> Vec<AgentEvent> {
  let lifecycle_type = if matches!(phase, Phase::Started) {
    "item_started"
  } else {
    "item_completed"
  };
  let Some(item) = payload.get("item").filter(|item| item.is_object()) else {
    return unknown_item_lifecycle(session_id, lifecycle_type, None, payload, timestamp);
  };
  let Some(item_type) = item.get("type").and_then(Value::as_str) else {
    return unknown_item_lifecycle(session_id, lifecycle_type, None, payload, timestamp);
  };
  let item_identity = if item_type == "Extension" {
    string_field(item, "kind")
      .map(|kind| format!("Extension.{kind}"))
      .unwrap_or_else(|| "Extension".to_string())
  } else {
    item_type.to_string()
  };
  if string_field(payload, "thread_id").is_none() || string_field(payload, "turn_id").is_none() {
    return unknown_item_lifecycle(session_id, lifecycle_type, Some(&item_identity), payload, timestamp);
  }

  let events = match item_type {
    "UserMessage" => normalize_user_item(
      session_id.clone(),
      lifecycle_type,
      item,
      phase,
      payload,
      timestamp.clone(),
    ),
    "HookPrompt" => valid_hook_prompt_item(item).then(|| {
      vec![item_lifecycle_metadata(
        session_id.clone(),
        lifecycle_type,
        item_type,
        "hook prompt",
        payload,
        timestamp.clone(),
      )]
    }),
    "AgentMessage" => normalize_canonical_agent_message(
      session_id.clone(),
      lifecycle_type,
      item,
      phase,
      payload,
      timestamp.clone(),
    ),
    "Plan" => valid_plan_item(item).then(|| {
      vec![item_lifecycle_metadata(
        session_id.clone(),
        lifecycle_type,
        item_type,
        if matches!(phase, Phase::Started) {
          "plan started"
        } else {
          "plan completed"
        },
        payload,
        timestamp.clone(),
      )]
    }),
    // Raw response reasoning remains authoritative because only it preserves
    // encrypted_content. Validate this duplicate before suppressing it.
    "Reasoning" => valid_canonical_reasoning(item).then(Vec::new),
    "CommandExecution" => normalize_command_item(session_id.clone(), item, phase, timestamp.clone()),
    "DynamicToolCall" => normalize_dynamic_tool_item(session_id.clone(), item, phase, timestamp.clone()),
    "CollabAgentToolCall" => normalize_collab_item(session_id.clone(), item, phase, timestamp.clone()),
    "SubAgentActivity" => normalize_subagent_item(
      session_id.clone(),
      lifecycle_type,
      payload,
      item,
      phase,
      timestamp.clone(),
    ),
    "WebSearch" => normalize_search_item(session_id.clone(), item, phase, timestamp.clone()),
    "ImageView" => normalize_image_view_item(session_id.clone(), item, phase, timestamp.clone()),
    "Extension" => normalize_extension_item(session_id.clone(), item, phase, timestamp.clone()),
    "ImageGeneration" => normalize_image_generation_item(session_id.clone(), item, phase, false, timestamp.clone()),
    "EnteredReviewMode" => valid_entered_review_item(item).then(|| {
      vec![item_lifecycle_metadata(
        session_id.clone(),
        lifecycle_type,
        item_type,
        "entered review mode",
        payload,
        timestamp.clone(),
      )]
    }),
    "ExitedReviewMode" => valid_exited_review_item(item).then(|| {
      vec![item_lifecycle_metadata(
        session_id.clone(),
        lifecycle_type,
        item_type,
        "exited review mode",
        payload,
        timestamp.clone(),
      )]
    }),
    "FileChange" => normalize_file_change_item(session_id.clone(), item, phase, timestamp.clone()),
    "McpToolCall" => normalize_mcp_item(session_id.clone(), item, phase, timestamp.clone()),
    // This is handled by RecordsNormalizer so accounting snapshot state is
    // reset together with the context-compaction metadata event.
    "ContextCompaction" => None,
    _ => None,
  };

  events.unwrap_or_else(|| unknown_item_lifecycle(session_id, lifecycle_type, Some(&item_identity), payload, timestamp))
}

fn normalize_user_item(
  session_id: Option<String>,
  lifecycle_type: &str,
  item: &Value,
  phase: Phase,
  payload: &Value,
  timestamp: Option<String>,
) -> Option<Vec<AgentEvent>> {
  let message_id = string_field(item, "id")?;
  let content = item.get("content")?.as_array()?;
  let mut text = String::new();
  for entry in content {
    match entry.get("type").and_then(Value::as_str) {
      Some("text") => text.push_str(entry.get("text")?.as_str()?),
      Some("image") if string_field(entry, "image_url").is_some() => {}
      Some("local_image" | "local_audio") if string_field(entry, "path").is_some() => {}
      Some("audio") if string_field(entry, "audio_url").is_some() => {}
      Some("skill" | "mention") if string_field(entry, "name").is_some() && string_field(entry, "path").is_some() => {}
      Some(_) => return None,
      None => return None,
    }
  }
  if text.is_empty() || matches!(phase, Phase::Started) {
    return Some(vec![item_lifecycle_metadata(
      session_id,
      lifecycle_type,
      "UserMessage",
      if text.is_empty() {
        "user message attachments"
      } else {
        "user message started"
      },
      payload,
      timestamp,
    )]);
  }
  Some(vec![message_event(
    session_id,
    Some(message_id),
    Role::User,
    phase,
    text,
    timestamp,
  )])
}

fn normalize_canonical_agent_message(
  session_id: Option<String>,
  lifecycle_type: &str,
  item: &Value,
  phase: Phase,
  payload: &Value,
  timestamp: Option<String>,
) -> Option<Vec<AgentEvent>> {
  let message_id = string_field(item, "id")?;
  let content = item.get("content")?.as_array()?;
  let mut text = String::new();
  for entry in content {
    if entry.get("type").and_then(Value::as_str) != Some("Text") {
      return None;
    }
    text.push_str(entry.get("text")?.as_str()?);
  }
  let delivery = codex_message_delivery(item.get("phase").and_then(Value::as_str));
  if item.get("questions").is_some_and(|value| !value.is_null())
    || item.get("delivery").is_some_and(|value| !value.is_null())
  {
    let question = super::questions::async_message(session_id.clone(), item, payload, text, phase, timestamp.clone())?;
    if matches!(phase, Phase::Started) {
      return Some(vec![item_lifecycle_metadata(
        session_id,
        lifecycle_type,
        "AgentMessage",
        "question request started",
        payload,
        timestamp,
      )]);
    }
    return Some(vec![question]);
  }
  if text.is_empty() {
    return Some(vec![item_lifecycle_metadata(
      session_id,
      lifecycle_type,
      "AgentMessage",
      "empty agent message",
      payload,
      timestamp,
    )]);
  }
  Some(vec![AgentEvent::Message(MessageEvent {
    provenance: None,
    provider: Provider::Codex,
    session_id,
    message_id: Some(message_id),
    parent_id: None,
    role: Role::Assistant,
    delivery,
    phase,
    text,
    timestamp,
  })])
}

fn valid_canonical_reasoning(item: &Value) -> bool {
  string_field(item, "id").is_some()
    && item.get("summary_text").and_then(string_array).is_some()
    && item.get("raw_content").is_none_or(|raw| string_array(raw).is_some())
}

fn valid_hook_prompt_item(item: &Value) -> bool {
  string_field(item, "id").is_some()
    && item
      .get("fragments")
      .and_then(Value::as_array)
      .is_some_and(|fragments| {
        fragments
          .iter()
          .all(|fragment| string_field(fragment, "text").is_some() && string_field(fragment, "hookRunId").is_some())
      })
}

fn normalize_command_item(
  session_id: Option<String>,
  item: &Value,
  phase: Phase,
  timestamp: Option<String>,
) -> Option<Vec<AgentEvent>> {
  let call_id = string_field(item, "id")?;
  let command = item.get("command")?.as_array()?;
  if !command.iter().all(Value::is_string) || path_field(item, "cwd").is_none() {
    return None;
  }
  let status = string_field(item, "status")?;
  let is_finished = matches!(phase, Phase::Finished);
  if (is_finished && !matches!(status.as_str(), "completed" | "failed" | "declined"))
    || (!is_finished && status != "in_progress")
  {
    return None;
  }
  let command = Value::Array(command.clone());
  let exit_code = item.get("exit_code").and_then(Value::as_i64);
  let output = is_finished.then(|| {
    json!({
      "stdout": item.get("stdout").cloned().unwrap_or(Value::Null),
      "stderr": item.get("stderr").cloned().unwrap_or(Value::Null),
      "aggregated_output": item.get("aggregated_output").cloned().unwrap_or(Value::Null),
      "formatted_output": item.get("formatted_output").cloned().unwrap_or(Value::Null),
    })
  });
  Some(vec![AgentEvent::ToolCall(ToolCallEvent {
    provider: Provider::Codex,
    session_id,
    turn_id: None,
    message_id: None,
    parent_id: None,
    record_kind: tool_record_kind_for_phase(phase),
    tool_call_id: Some(call_id),
    provider_tool_name: None,
    tool_name: Some("exec_command".to_string()),
    tool_kind: ToolKind::Shell,
    transport: None,
    summary: Some(ToolSummary::Shell {
      command: command_text(Some(&command)),
      cwd: path_field(item, "cwd"),
      exit_code,
    }),
    phase,
    input: (!is_finished).then_some(command),
    output,
    is_error: is_finished.then(|| status != "completed" || exit_code.is_some_and(|code| code != 0)),
    native: None,
    timestamp,
  })])
}

fn normalize_dynamic_tool_item(
  session_id: Option<String>,
  item: &Value,
  phase: Phase,
  timestamp: Option<String>,
) -> Option<Vec<AgentEvent>> {
  let call_id = string_field(item, "id")?;
  let tool = string_field(item, "tool")?;
  let namespace = item
    .get("namespace")
    .and_then(Value::as_str)
    .filter(|value| !value.is_empty());
  let name = namespace.map(|namespace| format!("{namespace}.{tool}")).unwrap_or(tool);
  let arguments = item.get("arguments")?.clone();
  let status = string_field(item, "status")?;
  let is_finished = matches!(phase, Phase::Finished);
  if (is_finished && !matches!(status.as_str(), "completed" | "failed"))
    || (!is_finished && status != "in_progress")
    || item
      .get("content_items")
      .is_some_and(|value| !value.is_null() && !value.is_array())
    || item
      .get("success")
      .is_some_and(|value| !value.is_null() && !value.is_boolean())
    || item
      .get("error")
      .is_some_and(|value| !value.is_null() && !value.is_string())
  {
    return None;
  }
  let success = item.get("success").and_then(Value::as_bool);
  Some(vec![AgentEvent::ToolCall(ToolCallEvent {
    provider: Provider::Codex,
    session_id,
    turn_id: None,
    message_id: None,
    parent_id: None,
    record_kind: tool_record_kind_for_phase(phase),
    tool_call_id: Some(call_id),
    provider_tool_name: None,
    tool_name: Some(name.clone()),
    tool_kind: tool_kind_for_name(&name),
    transport: None,
    summary: (!is_finished)
      .then(|| tool_summary_for_io(Some(&name), Some(&arguments), None))
      .flatten(),
    phase,
    input: (!is_finished).then_some(arguments),
    output: is_finished.then(|| {
      json!({
        "content_items": item.get("content_items").cloned().unwrap_or(Value::Null),
        "success": success,
        "error": item.get("error").cloned().unwrap_or(Value::Null),
      })
    }),
    is_error: is_finished.then(|| status == "failed" || success == Some(false)),
    native: None,
    timestamp,
  })])
}

fn normalize_file_change_item(
  session_id: Option<String>,
  item: &Value,
  phase: Phase,
  timestamp: Option<String>,
) -> Option<Vec<AgentEvent>> {
  let call_id = string_field(item, "id")?;
  let changes = item.get("changes")?.as_object().map(|_| item["changes"].clone())?;
  let is_finished = matches!(phase, Phase::Finished);
  let status = item.get("status").and_then(Value::as_str);
  if (is_finished && !matches!(status, None | Some("completed" | "failed" | "declined")))
    || (!is_finished && !matches!(status, None | Some("in_progress")))
  {
    return None;
  }
  Some(vec![AgentEvent::ToolCall(ToolCallEvent {
    provider: Provider::Codex,
    session_id,
    turn_id: None,
    message_id: None,
    parent_id: None,
    record_kind: tool_record_kind_for_phase(phase),
    tool_call_id: Some(call_id),
    provider_tool_name: None,
    tool_name: Some("apply_patch".to_string()),
    tool_kind: ToolKind::FileEdit,
    transport: None,
    summary: Some(patch_summary(&changes)),
    phase,
    input: (!is_finished).then_some(changes),
    output: is_finished.then(|| {
      json!({
        "stdout": item.get("stdout").cloned().unwrap_or(Value::Null),
        "stderr": item.get("stderr").cloned().unwrap_or(Value::Null),
      })
    }),
    is_error: if is_finished {
      status.map(|status| status != "completed")
    } else {
      None
    },
    native: None,
    timestamp,
  })])
}

fn normalize_mcp_item(
  session_id: Option<String>,
  item: &Value,
  phase: Phase,
  timestamp: Option<String>,
) -> Option<Vec<AgentEvent>> {
  let call_id = string_field(item, "id")?;
  let server = string_field(item, "server")?;
  let tool = string_field(item, "tool")?;
  let arguments = item.get("arguments")?.clone();
  let status = string_field(item, "status")?;
  let is_finished = matches!(phase, Phase::Finished);
  if (is_finished && !matches!(status.as_str(), "completed" | "failed"))
    || (!is_finished && !matches!(status.as_str(), "inProgress" | "in_progress"))
  {
    return None;
  }
  let name = format!("{server}.{tool}");
  let output = item
    .get("result")
    .cloned()
    .or_else(|| item.get("error").cloned())
    .unwrap_or(Value::Null);
  let result_is_error = output
    .get("isError")
    .or_else(|| output.get("is_error"))
    .and_then(Value::as_bool)
    .unwrap_or(false);
  Some(vec![AgentEvent::ToolCall(ToolCallEvent {
    provider: Provider::Codex,
    session_id,
    turn_id: None,
    message_id: None,
    parent_id: None,
    record_kind: tool_record_kind_for_phase(phase),
    tool_call_id: Some(call_id),
    provider_tool_name: None,
    tool_name: Some(name.clone()),
    tool_kind: tool_kind_for_name(&name),
    transport: None,
    summary: (!is_finished)
      .then(|| tool_summary_for_io(Some(&name), Some(&arguments), None))
      .flatten(),
    phase,
    input: (!is_finished).then_some(arguments),
    output: is_finished.then_some(output),
    is_error: is_finished.then(|| status == "failed" || result_is_error),
    native: None,
    timestamp,
  })])
}

fn normalize_collab_item(
  session_id: Option<String>,
  item: &Value,
  phase: Phase,
  timestamp: Option<String>,
) -> Option<Vec<AgentEvent>> {
  let call_id = string_field(item, "id")?;
  let tool = string_field(item, "tool")?;
  let tool_name = match tool.as_str() {
    "spawn_agent" | "send_input" | "resume_agent" | "wait" | "close_agent" => tool,
    _ => return None,
  };
  let status = string_field(item, "status")?;
  let is_finished = matches!(phase, Phase::Finished);
  if (is_finished && !matches!(status.as_str(), "completed" | "failed"))
    || (!is_finished && status != "in_progress")
    || string_field(item, "sender_thread_id").is_none()
    || item.get("receiver_thread_ids").is_some_and(|value| !value.is_array())
    || item.get("receiver_agents").is_some_and(|value| !value.is_array())
    || item.get("agents_states").is_some_and(|value| !value.is_object())
  {
    return None;
  }
  let input = json!({
    "sender_thread_id": item.get("sender_thread_id"),
    "receiver_thread_ids": item.get("receiver_thread_ids"),
    "receiver_agents": item.get("receiver_agents"),
    "prompt": item.get("prompt"),
    "model": item.get("model"),
    "reasoning_effort": item.get("reasoning_effort"),
  });
  Some(vec![AgentEvent::ToolCall(ToolCallEvent {
    provider: Provider::Codex,
    session_id,
    turn_id: None,
    message_id: None,
    parent_id: None,
    record_kind: tool_record_kind_for_phase(phase),
    tool_call_id: Some(call_id),
    provider_tool_name: None,
    tool_name: Some(tool_name.clone()),
    tool_kind: ToolKind::Task,
    transport: None,
    summary: Some(ToolSummary::Task {
      title: string_field(item, "prompt").or_else(|| Some(tool_name.clone())),
    }),
    phase,
    input: (!is_finished).then_some(input),
    output: is_finished.then(|| {
      json!({
        "status": status,
        "receiver_thread_ids": item.get("receiver_thread_ids"),
        "agents_states": item.get("agents_states"),
      })
    }),
    is_error: is_finished.then(|| status == "failed"),
    native: None,
    timestamp,
  })])
}

fn normalize_subagent_item(
  session_id: Option<String>,
  lifecycle_type: &str,
  payload: &Value,
  item: &Value,
  phase: Phase,
  timestamp: Option<String>,
) -> Option<Vec<AgentEvent>> {
  let event_id = string_field(item, "id")?;
  let kind = string_field(item, "kind")?;
  let target_session_id = string_field(item, "agent_thread_id")?;
  let target_agent_path = string_field(item, "agent_path")?;
  if matches!(phase, Phase::Started) {
    return Some(vec![item_lifecycle_metadata(
      session_id,
      lifecycle_type,
      "SubAgentActivity",
      "subagent activity started",
      payload,
      timestamp,
    )]);
  }
  let occurred_at_ms = payload.get("completed_at_ms")?.as_u64()?;
  Some(vec![AgentEvent::AgentActivity(AgentActivity {
    provider: Provider::Codex,
    session_id,
    event_id: Some(event_id),
    actor_session_id: None,
    actor_agent_path: None,
    target_session_id: Some(target_session_id),
    target_agent_path: Some(target_agent_path),
    kind,
    communication: None,
    occurred_at_ms: Some(occurred_at_ms),
    native: Some(payload.clone()),
    timestamp,
  })])
}

fn normalize_search_item(
  session_id: Option<String>,
  item: &Value,
  phase: Phase,
  timestamp: Option<String>,
) -> Option<Vec<AgentEvent>> {
  let call_id = string_field(item, "id")?;
  let query = string_field(item, "query")?;
  let action = optional_object(item, "action")?;
  if item
    .get("results")
    .is_some_and(|value| !value.is_null() && !value.is_array())
  {
    return None;
  }
  Some(search_tool_event(
    session_id,
    call_id,
    query,
    action,
    item.get("results").cloned(),
    phase,
    timestamp,
  ))
}

fn normalize_image_view_item(
  session_id: Option<String>,
  item: &Value,
  phase: Phase,
  timestamp: Option<String>,
) -> Option<Vec<AgentEvent>> {
  let call_id = string_field(item, "id")?;
  let path = path_field(item, "path")?;
  Some(vec![AgentEvent::ToolCall(ToolCallEvent {
    provider: Provider::Codex,
    session_id,
    turn_id: None,
    message_id: None,
    parent_id: None,
    record_kind: tool_record_kind_for_phase(phase),
    tool_call_id: Some(call_id),
    provider_tool_name: None,
    tool_name: Some("view_image".to_string()),
    tool_kind: ToolKind::FileRead,
    transport: None,
    summary: Some(ToolSummary::FileRead {
      path: Some(path.clone()),
    }),
    phase,
    input: Some(json!({ "path": path })),
    output: None,
    is_error: None,
    native: None,
    timestamp,
  })])
}

fn normalize_image_generation_item(
  session_id: Option<String>,
  item: &Value,
  phase: Phase,
  camel_case: bool,
  timestamp: Option<String>,
) -> Option<Vec<AgentEvent>> {
  let call_id = string_field(item, "id")?;
  let status = string_field(item, "status")?;
  let revised_prompt_field = if camel_case { "revisedPrompt" } else { "revised_prompt" };
  let saved_path_field = if camel_case { "savedPath" } else { "saved_path" };
  let revised_prompt = item
    .get(revised_prompt_field)
    .and_then(Value::as_str)
    .map(str::to_string);
  if item
    .get(revised_prompt_field)
    .is_some_and(|value| !value.is_null() && !value.is_string())
    || item
      .get(saved_path_field)
      .is_some_and(|value| !value.is_null() && !value.is_string())
  {
    return None;
  }
  let result = item.get("result")?.as_str()?.to_string();
  let is_finished = matches!(phase, Phase::Finished);
  Some(vec![AgentEvent::ToolCall(ToolCallEvent {
    provider: Provider::Codex,
    session_id,
    turn_id: None,
    message_id: None,
    parent_id: None,
    record_kind: tool_record_kind_for_phase(phase),
    tool_call_id: Some(call_id),
    provider_tool_name: None,
    tool_name: Some("image_generation".to_string()),
    tool_kind: ToolKind::Unknown,
    transport: None,
    summary: None,
    phase,
    input: revised_prompt.map(Value::String),
    output: is_finished.then(|| {
      json!({
        "status": status,
        "result": result,
        "saved_path": item.get(saved_path_field).cloned().unwrap_or(Value::Null),
      })
    }),
    is_error: is_finished.then(|| matches!(status.as_str(), "failed" | "error")),
    native: None,
    timestamp,
  })])
}

fn normalize_extension_item(
  session_id: Option<String>,
  item: &Value,
  phase: Phase,
  timestamp: Option<String>,
) -> Option<Vec<AgentEvent>> {
  match string_field(item, "kind")?.as_str() {
    "web.search" => {
      let call_id = string_field(item, "id")?;
      let query = string_field(item, "query")?;
      let action = optional_object(item, "action")?;
      if item
        .get("results")
        .is_some_and(|value| !value.is_null() && !value.is_array())
      {
        return None;
      }
      Some(search_tool_event(
        session_id,
        call_id,
        query,
        action,
        item.get("results").cloned(),
        phase,
        timestamp,
      ))
    }
    "image_gen.generation" => normalize_image_generation_item(session_id, item, phase, true, timestamp),
    "clock.sleep" => {
      let call_id = string_field(item, "id")?;
      let duration_ms = item.get("durationMs")?.as_u64()?;
      let is_finished = matches!(phase, Phase::Finished);
      Some(vec![AgentEvent::ToolCall(ToolCallEvent {
        provider: Provider::Codex,
        session_id,
        turn_id: None,
        message_id: None,
        parent_id: None,
        record_kind: tool_record_kind_for_phase(phase),
        tool_call_id: Some(call_id),
        provider_tool_name: None,
        tool_name: Some("sleep".to_string()),
        tool_kind: ToolKind::Unknown,
        transport: None,
        summary: None,
        phase,
        input: (!is_finished).then(|| json!({ "duration_ms": duration_ms })),
        output: is_finished.then(|| json!({ "duration_ms": duration_ms })),
        is_error: None,
        native: None,
        timestamp,
      })])
    }
    _ => None,
  }
}

fn valid_plan_item(item: &Value) -> bool {
  string_field(item, "id").is_some() && item.get("text").is_some_and(Value::is_string)
}

fn valid_entered_review_item(item: &Value) -> bool {
  string_field(item, "id").is_some()
    && item
      .get("target")
      .and_then(Value::as_object)
      .and_then(|target| target.get("type"))
      .is_some_and(Value::is_string)
    && item.get("user_facing_hint").is_some_and(Value::is_string)
}

fn valid_exited_review_item(item: &Value) -> bool {
  string_field(item, "id").is_some()
    && item
      .get("review_output")
      .is_some_and(|value| value.is_null() || value.is_object())
}

fn search_tool_event(
  session_id: Option<String>,
  call_id: String,
  query: String,
  action: Option<Value>,
  results: Option<Value>,
  phase: Phase,
  timestamp: Option<String>,
) -> Vec<AgentEvent> {
  let is_finished = matches!(phase, Phase::Finished);
  vec![AgentEvent::ToolCall(ToolCallEvent {
    provider: Provider::Codex,
    session_id,
    turn_id: None,
    message_id: None,
    parent_id: None,
    record_kind: tool_record_kind_for_phase(phase),
    tool_call_id: Some(call_id),
    provider_tool_name: None,
    tool_name: Some("web_search".to_string()),
    tool_kind: ToolKind::Search,
    transport: None,
    summary: Some(ToolSummary::Search {
      query: Some(query.clone()),
    }),
    phase,
    input: action.or_else(|| Some(json!({ "query": query }))),
    output: is_finished.then(|| {
      json!({
        "query": query,
        "results": results.unwrap_or(Value::Null),
      })
    }),
    is_error: None,
    native: None,
    timestamp,
  })]
}

fn item_lifecycle_metadata(
  session_id: Option<String>,
  lifecycle_type: &str,
  item_type: &str,
  summary: &str,
  payload: &Value,
  timestamp: Option<String>,
) -> AgentEvent {
  AgentEvent::Metadata(MetadataEvent {
    provider: Provider::Codex,
    session_id,
    kind: MetadataKind::Context,
    native_type: format!("event_msg.{lifecycle_type}.{item_type}"),
    summary: summary.to_string(),
    native: payload.clone(),
    timestamp,
  })
}

fn unknown_item_lifecycle(
  session_id: Option<String>,
  lifecycle_type: &str,
  item_type: Option<&str>,
  payload: &Value,
  timestamp: Option<String>,
) -> Vec<AgentEvent> {
  let native_type = item_type
    .map(|item_type| format!("event_msg.{lifecycle_type}.{item_type}"))
    .unwrap_or_else(|| format!("event_msg.{lifecycle_type}"));
  vec![unknown_event(
    session_id,
    Some(native_type),
    Some(payload.clone()),
    timestamp,
  )]
}
