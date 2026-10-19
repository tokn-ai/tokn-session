use serde_json::Value;
use tokn_session_core::{
  AgentEvent, ErrorEvent, MessageDelivery, MessageEvent, MetadataEvent, MetadataKind, Phase, Provider, ProviderChanged,
  ReasoningEvent, Role, SessionStarted, ToolCallEvent, ToolRecordKind, ToolTransport, UnknownEvent, UsageEvent,
  UsageKind, tool_kind_for_optional_name, tool_summary_for_io,
};
use tokn_workbuddy_protocol::{ContentBlock, WorkBuddySessionItem, WorkBuddySessionLine};

pub(crate) struct WorkBuddyNormalizer {
  session_id: String,
  current_model_provider: Option<String>,
  current_model_id: Option<String>,
}

impl WorkBuddyNormalizer {
  pub(crate) fn new(session_id: String) -> Self {
    Self {
      session_id,
      current_model_provider: None,
      current_model_id: None,
    }
  }

  pub(crate) fn start(
    &mut self,
    cwd: Option<String>,
    timestamp: Option<String>,
    model: Option<&str>,
  ) -> Vec<AgentEvent> {
    let mut events = vec![AgentEvent::SessionStarted(SessionStarted {
      provider: Provider::WorkBuddy,
      session_id: self.session_id.clone(),
      cwd,
      timestamp: timestamp.clone(),
    })];
    if let Some(model) = model.and_then(non_blank) {
      let (provider, model_id) = split_model(model);
      self.current_model_provider.clone_from(&provider);
      self.current_model_id = Some(model_id.clone());
      events.push(AgentEvent::ProviderChanged(ProviderChanged {
        provider: Provider::WorkBuddy,
        session_id: Some(self.session_id.clone()),
        native_id: None,
        native_parent_id: None,
        model_provider: provider,
        model_id: Some(model_id),
        thinking_level: None,
        timestamp,
      }));
    }
    events
  }

  pub(crate) fn normalize_line(&mut self, line: WorkBuddySessionLine) -> Vec<AgentEvent> {
    let message_id = line.id().map(str::to_string);
    let parent_id = line.parent_id().map(str::to_string);
    let session_id = line
      .session_id()
      .map(str::to_string)
      .unwrap_or_else(|| self.session_id.clone());
    let timestamp = timestamp(line.timestamp());
    let native_type = line.item().native_type().map(str::to_string);
    let (item, native) = line.into_parts();
    let usage = usage_event(&native, &session_id, message_id.clone(), timestamp.clone());
    let mut events = self.observe_model(
      &native,
      &session_id,
      message_id.clone(),
      parent_id.clone(),
      timestamp.clone(),
    );

    match item {
      WorkBuddySessionItem::Message(item) => {
        events.extend(self.message(item, native, session_id, message_id, parent_id, timestamp));
      }
      WorkBuddySessionItem::Reasoning(item) => {
        events.extend(self.reasoning(
          item.content,
          item.raw_content,
          native,
          session_id,
          message_id,
          parent_id,
          timestamp,
        ));
      }
      WorkBuddySessionItem::FunctionCall(item) => {
        let input = item.arguments.and_then(decode_arguments);
        let tool_kind = tool_kind_for_optional_name(item.name.as_deref());
        events.push(AgentEvent::ToolCall(ToolCallEvent {
          provider: Provider::WorkBuddy,
          session_id: Some(session_id),
          turn_id: None,
          message_id,
          parent_id,
          record_kind: ToolRecordKind::Invocation,
          tool_call_id: item.call_id,
          provider_tool_name: item.name.clone(),
          tool_name: item.name.clone(),
          tool_kind,
          transport: Some(ToolTransport::Native),
          summary: tool_summary_for_io(item.name.as_deref(), input.as_ref(), None),
          phase: Phase::Started,
          input,
          output: None,
          is_error: None,
          native: Some(native),
          timestamp,
        }));
      }
      WorkBuddySessionItem::FunctionCallResult(item) => {
        let output = result_output(item.output.as_ref(), item.provider_data.as_ref());
        let is_error = result_is_error(item.status.as_deref(), item.provider_data.as_ref());
        let tool_kind = tool_kind_for_optional_name(item.name.as_deref());
        events.push(AgentEvent::ToolCall(ToolCallEvent {
          provider: Provider::WorkBuddy,
          session_id: Some(session_id),
          turn_id: None,
          message_id,
          parent_id,
          record_kind: ToolRecordKind::Result,
          tool_call_id: item.call_id,
          provider_tool_name: item.name.clone(),
          tool_name: item.name.clone(),
          tool_kind,
          transport: Some(ToolTransport::Native),
          summary: tool_summary_for_io(item.name.as_deref(), None, output.as_ref()),
          phase: result_phase(item.status.as_deref()),
          input: None,
          output,
          is_error,
          native: Some(native),
          timestamp,
        }));
      }
      WorkBuddySessionItem::FileHistorySnapshot(item) => {
        if item.snapshot.is_some() {
          events.push(AgentEvent::Metadata(MetadataEvent {
            provider: Provider::WorkBuddy,
            session_id: Some(session_id),
            kind: MetadataKind::Context,
            native_type: "file-history-snapshot".to_string(),
            summary: if item.is_snapshot_update == Some(true) {
              "file history snapshot updated".to_string()
            } else {
              "file history snapshot".to_string()
            },
            native,
            timestamp,
          }));
        } else {
          events.push(unknown_event(Some(session_id), native_type, Some(native), timestamp));
        }
      }
      WorkBuddySessionItem::AiTitle(item) => {
        if let Some(title) = item.ai_title.as_deref().and_then(non_blank) {
          events.push(AgentEvent::Metadata(MetadataEvent {
            provider: Provider::WorkBuddy,
            session_id: Some(session_id),
            kind: MetadataKind::Session,
            native_type: "ai-title".to_string(),
            summary: format!("session title: {title}"),
            native,
            timestamp,
          }));
        } else {
          events.push(unknown_event(Some(session_id), native_type, Some(native), timestamp));
        }
      }
      WorkBuddySessionItem::Unknown(item) => events.push(unknown_event(
        Some(session_id),
        item.native_type,
        Some(item.native),
        timestamp,
      )),
    }

    if let Some(usage) = usage {
      events.push(usage);
    }

    events
  }

  fn observe_model(
    &mut self,
    native: &Value,
    session_id: &str,
    native_id: Option<String>,
    native_parent_id: Option<String>,
    timestamp: Option<String>,
  ) -> Vec<AgentEvent> {
    let Some(provider_data) = native.get("providerData") else {
      return Vec::new();
    };
    let request_model = string_field(provider_data, "requestModelId");
    let reported_model = string_field(provider_data, "model");
    let Some(model) = request_model
      .or(reported_model)
      .and_then(|value| non_blank_owned(value))
    else {
      return Vec::new();
    };
    let (reported_provider, model_id) = split_model(&model);
    let provider = reported_provider.or_else(|| self.current_model_provider.clone());
    if self.current_model_id.as_deref() == Some(model_id.as_str()) && self.current_model_provider == provider {
      return Vec::new();
    }
    self.current_model_provider.clone_from(&provider);
    self.current_model_id = Some(model_id.clone());
    vec![AgentEvent::ProviderChanged(ProviderChanged {
      provider: Provider::WorkBuddy,
      session_id: Some(session_id.to_string()),
      native_id,
      native_parent_id,
      model_provider: provider,
      model_id: Some(model_id),
      thinking_level: None,
      timestamp,
    })]
  }

  fn message(
    &self,
    item: tokn_workbuddy_protocol::MessageItem,
    native: Value,
    session_id: String,
    message_id: Option<String>,
    parent_id: Option<String>,
    timestamp: Option<String>,
  ) -> Vec<AgentEvent> {
    let role = match item.role.as_deref() {
      Some("user") => Role::User,
      Some("assistant") => Role::Assistant,
      Some("system") => Role::System,
      Some("tool") => Role::Tool,
      _ => Role::Unknown,
    };
    let delivery = if matches!(role, Role::Assistant) {
      MessageDelivery::Final
    } else {
      MessageDelivery::Unspecified
    };
    let mut events = Vec::new();
    let text = content_text(&item.content);
    let error = item
      .provider_data
      .as_ref()
      .and_then(|data| data.get("error"))
      .and_then(provider_error_message);
    if !text.is_empty() {
      events.push(AgentEvent::Message(MessageEvent {
        provenance: None,
        provider: Provider::WorkBuddy,
        session_id: Some(session_id.clone()),
        message_id: message_id.clone(),
        parent_id: parent_id.clone(),
        role,
        delivery,
        phase: message_phase(item.status.as_deref()),
        text,
        timestamp: timestamp.clone(),
      }));
    }
    for block in item.content {
      if let ContentBlock::Unknown(item) = block {
        events.push(unknown_event(
          Some(session_id.clone()),
          item.native_type.map(|kind| format!("message.content.{kind}")),
          Some(item.native),
          timestamp.clone(),
        ));
      }
    }
    if matches!(role, Role::Unknown) {
      events.push(unknown_event(
        Some(session_id.clone()),
        item.role.map(|role| format!("message.role.{role}")),
        Some(native.clone()),
        timestamp.clone(),
      ));
    } else if events.is_empty() && error.is_none() {
      events.push(unknown_event(
        Some(session_id.clone()),
        Some("message".to_string()),
        Some(native.clone()),
        timestamp.clone(),
      ));
    }
    if let Some(message) = error {
      events.push(AgentEvent::Error(ErrorEvent {
        provider: Provider::WorkBuddy,
        session_id: Some(session_id.clone()),
        message,
        native: Some(native.clone()),
        timestamp: timestamp.clone(),
      }));
      // Keep the diagnostic observation alongside the visible failure, including
      // empty assistant bodies and retry/network attribution.
      events.push(AgentEvent::Metadata(MetadataEvent {
        provider: Provider::WorkBuddy,
        session_id: Some(session_id),
        kind: MetadataKind::Diagnostic,
        native_type: "message".into(),
        summary: "WorkBuddy provider request failed".into(),
        native,
        timestamp,
      }));
    }
    events
  }

  fn reasoning(
    &self,
    content: Vec<ContentBlock>,
    raw_content: Vec<ContentBlock>,
    native: Value,
    session_id: String,
    message_id: Option<String>,
    parent_id: Option<String>,
    timestamp: Option<String>,
  ) -> Vec<AgentEvent> {
    let visible = content_text(&content);
    let raw = content_text(&raw_content);
    let text = non_blank(&visible).or_else(|| non_blank(&raw)).map(str::to_string);
    let mut events = Vec::new();
    if text.is_some() {
      events.push(AgentEvent::Reasoning(ReasoningEvent {
        provenance: None,
        provider: Provider::WorkBuddy,
        session_id: Some(session_id.clone()),
        message_id,
        parent_id,
        phase: Phase::Finished,
        text,
        summary: None,
        redacted: None,
        encrypted_content: None,
        signature: None,
        timestamp: timestamp.clone(),
      }));
    }
    for block in content.into_iter().chain(raw_content) {
      if let ContentBlock::Unknown(item) = block {
        events.push(unknown_event(
          Some(session_id.clone()),
          item.native_type.map(|kind| format!("reasoning.content.{kind}")),
          Some(item.native),
          timestamp.clone(),
        ));
      }
    }
    if events.is_empty() {
      events.push(unknown_event(
        Some(session_id),
        Some("reasoning".to_string()),
        Some(native),
        timestamp,
      ));
    }
    events
  }
}

const MAX_ERROR_MESSAGE_CHARS: usize = 4000;

fn provider_error_message(error: &Value) -> Option<String> {
  if !meaningful_error(error) {
    return None;
  }
  let text = match error {
    Value::String(text) => text.trim().to_string(),
    Value::Bool(true) => "WorkBuddy provider request failed".into(),
    Value::Object(object) => object
      .get("message")
      .and_then(Value::as_str)
      .and_then(non_blank)
      .map(str::to_string)
      .unwrap_or_else(|| error.to_string()),
    _ => error.to_string(),
  };
  let mut chars = text.chars();
  let mut bounded = chars.by_ref().take(MAX_ERROR_MESSAGE_CHARS).collect::<String>();
  if chars.next().is_some() {
    bounded.push_str("… [truncated]");
  }
  Some(bounded)
}

fn meaningful_error(value: &Value) -> bool {
  match value {
    Value::Null | Value::Bool(false) => false,
    Value::String(text) => !text.trim().is_empty(),
    Value::Array(values) => values.iter().any(meaningful_error),
    Value::Object(values) => values.values().any(meaningful_error),
    Value::Number(_) | Value::Bool(true) => true,
  }
}

fn content_text(content: &[ContentBlock]) -> String {
  content
    .iter()
    .filter_map(ContentBlock::text)
    .filter_map(non_blank)
    .collect::<Vec<_>>()
    .join("\n")
}

fn usage_event(
  record: &Value,
  session_id: &str,
  record_id: Option<String>,
  timestamp: Option<String>,
) -> Option<AgentEvent> {
  let provider_data = record.get("providerData");
  let usage = provider_data
    .and_then(|data| data.get("rawUsage"))
    .filter(|usage| !usage.is_null())
    .or_else(|| record.pointer("/message/usage").filter(|usage| !usage.is_null()))
    .or_else(|| {
      provider_data
        .and_then(|data| data.get("usage"))
        .filter(|usage| !usage.is_null())
    })?;
  let counters = parse_usage(usage);
  let Some((input_tokens, output_tokens, total_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens)) =
    counters
  else {
    return Some(unknown_event(
      Some(session_id.to_string()),
      Some("usage".to_string()),
      Some(usage.clone()),
      timestamp,
    ));
  };
  Some(AgentEvent::Usage(UsageEvent {
    kind: UsageKind::ModelCall,
    provider: Provider::WorkBuddy,
    session_id: Some(session_id.to_string()),
    turn_id: None,
    step_id: None,
    message_id: record_id.clone(),
    record_id,
    input_tokens,
    output_tokens,
    total_tokens,
    cache_read_tokens,
    cache_write_tokens,
    reasoning_tokens,
    native: usage.clone(),
    timestamp,
  }))
}

fn parse_usage(usage: &Value) -> Option<(u64, u64, Option<u64>, Option<u64>, Option<u64>, Option<u64>)> {
  let input_tokens = unsigned_field(usage, &["prompt_tokens", "input_tokens", "inputTokens"])?;
  let output_tokens = unsigned_field(usage, &["completion_tokens", "output_tokens", "outputTokens"])?;
  let total_tokens = optional_unsigned_field(usage, &["total_tokens", "totalTokens"])?;
  let cache_read_tokens = optional_unsigned_paths(
    usage,
    &[
      "/prompt_tokens_details/cached_tokens",
      "/cache_read_input_tokens",
      "/prompt_cache_hit_tokens",
      "/inputTokensDetails/0/cached_tokens",
    ],
  )?;
  let cache_write_tokens = optional_unsigned_paths(usage, &["/cache_write_input_tokens"])?;
  let reasoning_tokens = optional_unsigned_paths(
    usage,
    &[
      "/completion_tokens_details/reasoning_tokens",
      "/outputTokensDetails/0/reasoning_tokens",
    ],
  )?;
  Some((
    input_tokens,
    output_tokens,
    total_tokens,
    cache_read_tokens,
    cache_write_tokens,
    reasoning_tokens,
  ))
}

fn unsigned_field(value: &Value, names: &[&str]) -> Option<u64> {
  names.iter().find_map(|name| value.get(name).and_then(unsigned_value))
}

fn optional_unsigned_field(value: &Value, names: &[&str]) -> Option<Option<u64>> {
  for name in names {
    if let Some(counter) = value.get(name) {
      if counter.is_null() {
        return Some(None);
      }
      return unsigned_value(counter).map(Some);
    }
  }
  Some(None)
}

fn optional_unsigned_paths(value: &Value, paths: &[&str]) -> Option<Option<u64>> {
  for path in paths {
    if let Some(counter) = value.pointer(path) {
      if counter.is_null() {
        return Some(None);
      }
      return unsigned_value(counter).map(Some);
    }
  }
  Some(None)
}

fn unsigned_value(value: &Value) -> Option<u64> {
  value.as_u64().or_else(|| {
    let value = value.as_f64()?;
    (value.is_finite() && value >= 0.0 && value.fract() == 0.0 && value < u64::MAX as f64).then_some(value as u64)
  })
}

fn decode_arguments(value: Value) -> Option<Value> {
  match value {
    Value::String(raw) => serde_json::from_str(&raw).ok().or(Some(Value::String(raw))),
    Value::Null => None,
    value => Some(value),
  }
}

fn result_output(output: Option<&ContentBlock>, provider_data: Option<&Value>) -> Option<Value> {
  let mut output = output.and_then(|output| serde_json::to_value(output).ok())?;
  let exit_code = provider_data
    .and_then(|data| data.pointer("/toolResult/rawResponse/exitCode"))
    .and_then(Value::as_i64);
  if let (Some(exit_code), Some(object)) = (exit_code, output.as_object_mut()) {
    object.insert("exit_code".to_string(), Value::from(exit_code));
  }
  Some(output)
}

fn result_is_error(status: Option<&str>, provider_data: Option<&Value>) -> Option<bool> {
  if matches!(status, Some("error" | "failed" | "cancelled")) {
    return Some(true);
  }
  if status == Some("completed") {
    return Some(
      provider_data
        .and_then(|data| data.pointer("/toolResult/rawResponse/exitCode"))
        .and_then(Value::as_i64)
        .is_some_and(|code| code != 0),
    );
  }
  None
}

fn result_phase(status: Option<&str>) -> Phase {
  match status {
    Some("completed" | "error" | "failed" | "cancelled") => Phase::Finished,
    Some("running" | "in_progress") => Phase::Updated,
    _ => Phase::Updated,
  }
}

fn message_phase(status: Option<&str>) -> Phase {
  match status {
    Some("running" | "in_progress" | "streaming") => Phase::Updated,
    _ => Phase::Finished,
  }
}

fn split_model(model: &str) -> (Option<String>, String) {
  match model.split_once(':') {
    Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
      (Some(provider.to_string()), model.to_string())
    }
    _ => (None, model.to_string()),
  }
}

fn string_field(value: &Value, field: &str) -> Option<String> {
  value.get(field).and_then(Value::as_str).map(str::to_string)
}

fn non_blank(value: &str) -> Option<&str> {
  let value = value.trim();
  (!value.is_empty()).then_some(value)
}

fn non_blank_owned(value: String) -> Option<String> {
  non_blank(&value).map(str::to_string)
}

pub(crate) fn timestamp(value: Option<u64>) -> Option<String> {
  value.map(|value| value.to_string())
}

fn unknown_event(
  session_id: Option<String>,
  native_type: Option<String>,
  native: Option<Value>,
  timestamp: Option<String>,
) -> AgentEvent {
  AgentEvent::Unknown(UnknownEvent {
    provider: Provider::WorkBuddy,
    session_id,
    native_type,
    native,
    timestamp,
  })
}

#[cfg(test)]
mod tests {
  use tokn_session_core::{AgentEvent, MessageDelivery, Phase, ToolKind, ToolRecordKind, UsageKind};

  use super::WorkBuddyNormalizer;

  #[test]
  fn normalizes_message_reasoning_and_tool_fixtures() {
    let input = include_str!("../fixtures/projects/fixture-workspace/wb-file-read-local.jsonl");
    let lines = input
      .lines()
      .map(|line| serde_json::from_str(line).expect("fixture line should decode"))
      .collect::<Vec<_>>();
    let mut normalizer = WorkBuddyNormalizer::new("wb-file-read-local".to_string());
    let mut events = normalizer.start(
      Some("/fixture/workspace".to_string()),
      Some("1788265925769".to_string()),
      Some("custom-local:deepseek-v4-flash"),
    );
    for line in lines {
      events.extend(normalizer.normalize_line(line));
    }

    assert!(matches!(&events[0], AgentEvent::SessionStarted(event)
      if event.session_id == "wb-file-read-local"));
    assert!(matches!(&events[1], AgentEvent::ProviderChanged(event)
      if event.model_provider.as_deref() == Some("custom-local")
        && event.model_id.as_deref() == Some("deepseek-v4-flash")));
    assert!(events.iter().any(|event| matches!(event, AgentEvent::Message(message)
      if matches!(message.delivery, MessageDelivery::Unspecified)
        && message.text.contains("Use the Read tool"))));
    assert!(
      events
        .iter()
        .any(|event| matches!(event, AgentEvent::Reasoning(reasoning)
      if reasoning.text.as_deref().is_some_and(|text| text.contains("Total item count"))))
    );
    assert!(events.iter().any(|event| matches!(event, AgentEvent::ToolCall(tool)
      if matches!(tool.record_kind, ToolRecordKind::Invocation)
        && matches!(tool.phase, Phase::Started)
        && matches!(tool.tool_kind, ToolKind::FileRead)
        && tool.input.as_ref().and_then(|input| input["file_path"].as_str()) == Some("/fixture/workspace/inventory.txt"))));
    assert!(events.iter().any(|event| matches!(event, AgentEvent::ToolCall(tool)
      if matches!(tool.record_kind, ToolRecordKind::Result)
        && matches!(tool.phase, Phase::Finished)
        && tool.tool_call_id.as_deref() == Some("call_00_ET_QRXue3bsxdLblbRzA9Fp3295"))));
    assert!(events.iter().any(|event| matches!(event, AgentEvent::Message(message)
      if matches!(message.delivery, MessageDelivery::Final)
        && message.text == "Total count: **10**. Largest: **banana (5)**.")));
    let usage = events
      .iter()
      .filter_map(|event| match event {
        AgentEvent::Usage(usage) => Some(usage),
        _ => None,
      })
      .collect::<Vec<_>>();
    assert_eq!(usage.len(), 2);
    assert!(usage.iter().all(|usage| matches!(usage.kind, UsageKind::ModelCall)));
    assert!(usage.iter().all(|usage| usage.native.get("prompt_tokens").is_some()));
    assert!(usage.iter().any(|usage| {
      usage.record_id.as_deref() == Some("b0c388a7-06bd-4567-99b3-da0099586794")
        && usage.input_tokens == 4335
        && usage.output_tokens == 63
        && usage.total_tokens == Some(4398)
    }));
    assert!(usage.iter().any(|usage| {
      usage.record_id.as_deref() == Some("3b959bf2-fd61-457f-8010-fecbea15ef66")
        && usage.cache_read_tokens == Some(4352)
        && usage.reasoning_tokens == Some(39)
        && usage.cache_write_tokens.is_none()
    }));
  }

  #[test]
  fn preserves_failed_messages_and_future_records() {
    let input = include_str!("../fixtures/projects/fixture-workspace/wb-file-read.jsonl");
    let mut normalizer = WorkBuddyNormalizer::new("wb-file-read".to_string());
    let mut events = normalizer.start(None, None, None);
    for line in input.lines() {
      events.extend(normalizer.normalize_line(serde_json::from_str(line).expect("fixture line should decode")));
    }
    assert!(events.iter().any(|event| matches!(event, AgentEvent::Message(message)
      if message.text.starts_with("Authentication required"))));
    assert!(events.iter().any(|event| matches!(event, AgentEvent::Error(error)
      if error.message.starts_with("Authentication required"))));
    assert!(events.iter().any(|event| matches!(event, AgentEvent::Metadata(metadata)
      if matches!(metadata.kind, tokn_session_core::MetadataKind::Diagnostic)
        && metadata.native["id"] == "a50df043-e1c0-4691-b3d0-8f5ca503c7e8"
        && metadata.native["parentId"] == "0484b091-c6d5-4621-b623-3777eb42e1b0"
        && metadata.native["providerData"]["error"]["isRetryable"] == false)));

    let future = serde_json::from_value(serde_json::json!({
      "type": "future_record",
      "sessionId": "wb-file-read",
      "payload": {"answer": 42}
    }))
    .expect("future record should decode");
    let events = normalizer.normalize_line(future);
    assert!(matches!(&events[..], [AgentEvent::Unknown(event)]
      if event.native_type.as_deref() == Some("future_record")
        && event.native.as_ref().and_then(|native| native["payload"]["answer"].as_i64()) == Some(42)));
  }

  #[test]
  fn empty_assistant_failures_keep_structured_diagnostics_and_identity() {
    let native = serde_json::json!({
      "id":"failed-message", "parentId":"parent-message", "timestamp":42,
      "sessionId":"errors", "type":"message", "role":"assistant", "status":"incomplete", "content":[],
      "providerData":{"error":{"message":"Connection failed", "isNetworkError":true,"isRetryable":true,
        "details":{"status":503,"requestId":"request"}}, "requestModelId":"provider:model"}
    });
    let mut normalizer = WorkBuddyNormalizer::new("errors".into());
    let events = normalizer.normalize_line(serde_json::from_value(native.clone()).unwrap());
    assert_eq!(
      events
        .iter()
        .filter(|event| matches!(event, AgentEvent::Error(_)))
        .count(),
      1
    );
    assert!(events.iter().any(|event| matches!(event, AgentEvent::Error(error)
      if error.message == "Connection failed" && error.timestamp.as_deref() == Some("42")
        && error.native.as_ref() == Some(&native))));
    assert!(events.iter().any(|event| matches!(event, AgentEvent::Metadata(metadata)
      if metadata.native == native && metadata.timestamp.as_deref() == Some("42"))));
    assert!(
      !events
        .iter()
        .any(|event| matches!(event, AgentEvent::Message(_) | AgentEvent::Unknown(_)))
    );
  }

  #[test]
  fn repeated_failures_preserve_their_own_exact_native_envelope() {
    let mut normalizer = WorkBuddyNormalizer::new("errors".into());
    for id in ["first", "second"] {
      let native = serde_json::json!({
        "type":"message", "id":id, "parentId":format!("parent-{id}"),
        "sessionId":"errors", "role":"assistant", "status":"incomplete", "content":[],
        "providerData":{"error":{"message":"Connection failed", "details":{
          "code":"CONNECTION_RESET", "requestId":format!("request-{id}")
        }}}
      });
      let events = normalizer.normalize_line(serde_json::from_value(native.clone()).unwrap());
      assert!(
        matches!(&events[..], [AgentEvent::Error(error), AgentEvent::Metadata(diagnostic)]
        if error.message == "Connection failed" && error.native.as_ref() == Some(&native)
          && diagnostic.native == native)
      );
    }
  }

  #[test]
  fn pseudoerrors_do_not_turn_successful_messages_into_failures() {
    for error in [
      serde_json::Value::Null,
      serde_json::json!(false),
      serde_json::json!(" \n "),
      serde_json::json!({}),
      serde_json::json!([]),
      serde_json::json!({"message":"", "isNetworkError":false, "details":[]}),
      serde_json::json!([false, null, {}]),
    ] {
      let mut normalizer = WorkBuddyNormalizer::new("errors".into());
      let events = normalizer.normalize_line(
        serde_json::from_value(serde_json::json!({
          "type":"message", "role":"assistant", "status":"completed",
          "content":[{"type":"output_text", "text":"This reply discusses an error without reporting one."}],
          "providerData":{"error":error}
        }))
        .unwrap(),
      );
      assert!(
        matches!(&events[..], [AgentEvent::Message(message)]
        if message.text == "This reply discusses an error without reporting one."),
        "{events:?}"
      );
    }
  }

  #[test]
  fn partial_replies_usage_and_non_message_error_details_survive() {
    let native = serde_json::json!({
      "type":"message", "id":"partial", "parentId":"user", "role":"assistant", "status":"incomplete",
      "content":[{"type":"output_text","text":"A partial reply"}],
      "providerData":{"error":{"cause":{"code":"CONNECTION_RESET","message":"stream failed"}},
        "rawUsage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}
    });
    let mut normalizer = WorkBuddyNormalizer::new("errors".into());
    let events = normalizer.normalize_line(serde_json::from_value(native.clone()).unwrap());
    assert!(events.iter().any(|event| matches!(event, AgentEvent::Message(message)
      if message.text == "A partial reply" && message.message_id.as_deref() == Some("partial"))));
    assert!(events.iter().any(|event| matches!(event, AgentEvent::Error(error)
      if error.message.contains("CONNECTION_RESET") && error.message.contains("stream failed"))));
    assert!(
      events
        .iter()
        .any(|event| matches!(event, AgentEvent::Metadata(metadata) if metadata.native == native))
    );
    assert!(events.iter().any(|event| matches!(event, AgentEvent::Usage(usage)
      if usage.input_tokens == 10 && usage.output_tokens == 2 && usage.message_id.as_deref() == Some("partial"))));
  }

  #[test]
  fn recursively_empty_errors_stay_non_errors_across_nesting_depths() {
    for depth in [0, 7, 8, 9, 16, 64] {
      let mut error = serde_json::json!({"message":" ","network":false,"details":[null,{}]});
      for _ in 0..depth {
        error = serde_json::json!({"cause":error});
      }
      assert_eq!(super::provider_error_message(&error), None, "depth {depth}");
    }
  }

  #[test]
  fn tool_failures_and_cancellations_remain_tool_results_without_provider_errors() {
    for (status, exit_code, expected_error) in [("completed", 0, false), ("completed", 9, true), ("cancelled", 0, true)]
    {
      let mut normalizer = WorkBuddyNormalizer::new("tools".into());
      let events = normalizer.normalize_line(
        serde_json::from_value(serde_json::json!({
          "type":"function_call_result", "callId":"call", "name":"Bash", "status":status,
          "output":{"type":"text","text":"tool output"},
          "providerData":{"toolResult":{"rawResponse":{"exitCode":exit_code}}}
        }))
        .unwrap(),
      );
      assert!(matches!(&events[..], [AgentEvent::ToolCall(tool)]
        if tool.is_error == Some(expected_error) && matches!(tool.phase, Phase::Finished)));
    }
  }
}
