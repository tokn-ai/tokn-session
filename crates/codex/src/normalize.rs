mod async_replies;
mod code_mode;
mod communication;
mod item_lifecycle;
mod questions;
mod turn_lifecycle;

use std::collections::{HashMap, VecDeque};

use serde_json::{Value, json};
use tokn_codex_protocol::{
  ContentItem, EventMessage, MessageItem, ReasoningItem, ResponseItem, RolloutItem, SessionMetaItem, UnknownItem,
};
use tokn_session_core::{
  AgentActivity, AgentEvent, ErrorEvent, GoalUpdated, MessageDelivery, MessageEvent, Phase, Provider, ProviderChanged,
  ReasoningEvent, Role, SessionHistoryStatus, SessionSettingsApplied, SessionStarted, ToolCallEvent, ToolKind,
  ToolRecordKind, ToolSummary, ToolTransport, UnknownEvent, patch_summary, tool_kind_for_name,
  tool_kind_for_optional_name, tool_summary_for_input, tool_summary_for_io,
};

use crate::event::CodexLine;
use code_mode::{DecodedCodeModeCall, decode_call, decode_output};
use communication::{normalize_agent_message, normalize_inter_agent_communication};
use item_lifecycle::{normalize_item_lifecycle, normalize_legacy_item_completed};

const MAX_PENDING_CODE_MODE_CALLS: usize = 256;

pub struct CodexNormalizer {
  session_id: Option<String>,
  history_mode: CodexRolloutHistoryMode,
  history_boundary: Option<CodexHistoryBoundary>,
  records: crate::records::RecordsNormalizer,
  pending_communication_trigger: Option<bool>,
  pending_code_mode_calls: HashMap<String, VecDeque<PendingCodeModeCall>>,
  pending_code_mode_order: VecDeque<(String, u64)>,
  pending_code_mode_call_count: usize,
  next_pending_code_mode_token: u64,
  question_replies: questions::RepliesNormalizer,
}

#[derive(Clone, Debug)]
struct PendingCodeModeCall {
  token: u64,
  call: Option<DecodedCodeModeCall>,
  provider_tool_name: String,
  turn_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum CodexRolloutHistoryMode {
  #[default]
  Legacy,
  Paginated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodexHistoryState {
  BeforeOwner,
  Root,
  AwaitingSubagentBody,
  SubagentBody,
}

pub(crate) struct CodexHistoryBoundary {
  state: CodexHistoryState,
}

impl CodexHistoryBoundary {
  pub(crate) fn new() -> Self {
    Self {
      state: CodexHistoryState::BeforeOwner,
    }
  }

  pub(crate) fn accepts(&mut self, item: &RolloutItem) -> bool {
    match self.state {
      CodexHistoryState::BeforeOwner => {
        if let RolloutItem::SessionMeta(item) = item {
          self.state = if requires_thread_spawn_boundary(item) {
            CodexHistoryState::AwaitingSubagentBody
          } else {
            CodexHistoryState::Root
          };
        }
        true
      }
      CodexHistoryState::Root | CodexHistoryState::SubagentBody => !matches!(item, RolloutItem::SessionMeta(_)),
      CodexHistoryState::AwaitingSubagentBody => match item {
        RolloutItem::InterAgentCommunicationMetadata(item) if item.trigger_turn == Some(true) => {
          self.state = CodexHistoryState::SubagentBody;
          false
        }
        RolloutItem::InterAgentCommunication(item) if item.trigger_turn == Some(true) => {
          self.state = CodexHistoryState::SubagentBody;
          true
        }
        _ => false,
      },
    }
  }

  pub(crate) fn status(&self) -> SessionHistoryStatus {
    match self.state {
      CodexHistoryState::AwaitingSubagentBody => SessionHistoryStatus::SubagentBodyUnavailable,
      CodexHistoryState::SubagentBody => SessionHistoryStatus::FilteredSubagent,
      CodexHistoryState::BeforeOwner | CodexHistoryState::Root => SessionHistoryStatus::Complete,
    }
  }
}

impl CodexNormalizer {
  pub fn new() -> Self {
    Self {
      session_id: None,
      history_mode: CodexRolloutHistoryMode::Legacy,
      history_boundary: None,
      records: Default::default(),
      pending_communication_trigger: None,
      pending_code_mode_calls: Default::default(),
      pending_code_mode_order: Default::default(),
      pending_code_mode_call_count: 0,
      next_pending_code_mode_token: 0,
      question_replies: Default::default(),
    }
  }

  pub fn new_historical() -> Self {
    Self {
      session_id: None,
      history_mode: CodexRolloutHistoryMode::Legacy,
      history_boundary: Some(CodexHistoryBoundary::new()),
      records: Default::default(),
      pending_communication_trigger: None,
      pending_code_mode_calls: Default::default(),
      pending_code_mode_order: Default::default(),
      pending_code_mode_call_count: 0,
      next_pending_code_mode_token: 0,
      question_replies: Default::default(),
    }
  }

  pub fn normalize(&mut self, line: CodexLine) -> Vec<AgentEvent> {
    let timestamp = line.timestamp().map(str::to_string);
    // Delivery metadata belongs only to the immediately following raw record.
    // Track it before filtering: the first child-task marker is a consumed
    // history boundary, but its following agent message still needs the flag.
    let communication_trigger = self.pending_communication_trigger.take();
    if let RolloutItem::InterAgentCommunicationMetadata(item) = line.item() {
      self.pending_communication_trigger = item.trigger_turn;
    }
    if self
      .history_boundary
      .as_mut()
      .is_some_and(|boundary| !boundary.accepts(line.item()))
    {
      return Vec::new();
    }

    // Observe every source record before reply adapters can consume it. A
    // substantive reply must break pending checkpoint/notice correlation too.
    if let Some(events) = self.records.normalize(
      &line,
      self.session_id.clone(),
      matches!(self.history_mode, CodexRolloutHistoryMode::Paginated),
    ) {
      return events;
    }
    if let Some(events) = async_replies::normalize(&line, self.session_id.clone()) {
      return events;
    }
    if let Some(events) = self.question_replies.output(&line, self.session_id.clone()) {
      return events;
    }
    let events = self.normalize_item(line.into_item(), timestamp, communication_trigger);
    self.question_replies.observe(&events);
    events
  }

  pub fn history_status(&self) -> SessionHistoryStatus {
    self
      .history_boundary
      .as_ref()
      .map(CodexHistoryBoundary::status)
      .unwrap_or(SessionHistoryStatus::Complete)
  }

  fn normalize_item(
    &mut self,
    item: RolloutItem,
    timestamp: Option<String>,
    communication_trigger: Option<bool>,
  ) -> Vec<AgentEvent> {
    match item {
      RolloutItem::SessionMeta(item) => self.normalize_session_meta(item, timestamp),
      // Incoming agent messages have no canonical ItemCompleted equivalent.
      // Preserve them even when ordinary response items are deduplicated.
      RolloutItem::ResponseItem(ResponseItem::AgentMessage(item)) => {
        normalize_agent_message(self.session_id.clone(), item, timestamp, communication_trigger)
      }
      RolloutItem::ResponseItem(item) => {
        if matches!(self.history_mode, CodexRolloutHistoryMode::Paginated) {
          match item {
            // Structured requests have no canonical TurnItem equivalent. Their
            // raw invocation is the historical source of the question text.
            ResponseItem::FunctionCall(item)
              if item
                .name
                .as_deref()
                .is_some_and(|name| name.rsplit('.').next() == Some("request_user_input")) =>
            {
              vec![
                questions::function_call(self.session_id.clone(), &item, timestamp.clone()).unwrap_or_else(|| {
                  unknown_event(
                    self.session_id.clone(),
                    Some("response_item.function_call".into()),
                    Some(json_value(item)),
                    timestamp,
                  )
                }),
              ]
            }
            ResponseItem::Reasoning(item) => normalize_reasoning(self.session_id.clone(), item, timestamp),
            ResponseItem::Unknown(item) => {
              vec![unknown_response_event(self.session_id.clone(), item, timestamp)]
            }
            _ => Vec::new(),
          }
        } else {
          self.normalize_response_item(item, timestamp)
        }
      }
      RolloutItem::InterAgentCommunication(item) => {
        normalize_inter_agent_communication(self.session_id.clone(), item, timestamp)
      }
      RolloutItem::InterAgentCommunicationMetadata(_)
      | RolloutItem::TurnContext(_)
      | RolloutItem::WorldState(_)
      | RolloutItem::Compacted(_) => unreachable!("context records handled before consuming native envelope"),
      RolloutItem::EventMessage(item) => normalize_event_message(
        self.session_id.clone(),
        item,
        matches!(self.history_mode, CodexRolloutHistoryMode::Paginated),
        timestamp,
      ),
      RolloutItem::Unknown(item) => vec![unknown_rollout_event(self.session_id.clone(), item, timestamp)],
    }
  }

  fn normalize_session_meta(&mut self, item: SessionMetaItem, line_timestamp: Option<String>) -> Vec<AgentEvent> {
    if self.session_id.is_some() {
      return Vec::new();
    }

    let Some(session_id) = item.id.clone() else {
      return vec![unknown_event(
        None,
        Some("session_meta".to_string()),
        Some(json_value(item)),
        line_timestamp,
      )];
    };

    let (Ok(history_mode), Ok(_)) = (item.history_mode(), item.history_base()) else {
      return vec![unknown_event(
        None,
        Some("session_meta".to_string()),
        Some(json_value(item)),
        line_timestamp,
      )];
    };
    self.history_mode = if history_mode == Some("paginated") {
      CodexRolloutHistoryMode::Paginated
    } else {
      CodexRolloutHistoryMode::Legacy
    };
    self.session_id = Some(session_id.clone());
    let timestamp = item.timestamp.clone().or(line_timestamp);
    let mut events = vec![AgentEvent::SessionStarted(SessionStarted {
      provider: Provider::Codex,
      session_id,
      cwd: item.cwd.clone(),
      timestamp: timestamp.clone(),
    })];

    if item.model_provider.is_some() {
      events.push(AgentEvent::ProviderChanged(ProviderChanged {
        provider: Provider::Codex,
        session_id: self.session_id.clone(),
        native_id: item.id,
        native_parent_id: item.parent_thread_id,
        model_provider: item.model_provider,
        model_id: None,
        thinking_level: None,
        timestamp,
      }));
    }

    events
  }

  fn normalize_response_item(&mut self, item: ResponseItem, timestamp: Option<String>) -> Vec<AgentEvent> {
    match item {
      ResponseItem::CustomToolCall(item) => self.normalize_custom_tool_call(item, timestamp),
      ResponseItem::CustomToolCallOutput(item) => self.normalize_custom_tool_call_output(item, timestamp),
      item => normalize_response_item(self.session_id.clone(), item, timestamp),
    }
  }

  fn normalize_custom_tool_call(
    &mut self,
    item: tokn_codex_protocol::CustomToolCallItem,
    timestamp: Option<String>,
  ) -> Vec<AgentEvent> {
    let native = json_value(&item);
    let name = item
      .name
      .clone()
      .or_else(|| item.namespace.clone())
      .unwrap_or_else(|| "custom_tool".to_string());
    let raw_input = item.input.clone();
    let input = parse_json_string_or_value(item.input);
    let turn_id = response_turn_id(item.internal_chat_message_metadata_passthrough.as_ref());
    let code_mode = name == "exec"
      && raw_input.as_str().is_some_and(|source| {
        let source = source.trim_start();
        source.contains("tools.")
          || source.starts_with("const ")
          || source.starts_with("let ")
          || source.starts_with("await ")
      });
    let decoded = code_mode.then(|| decode_call(&raw_input)).flatten();

    if code_mode {
      self.remember_code_mode_call(
        item.call_id.clone(),
        PendingCodeModeCall {
          token: 0,
          call: decoded.clone(),
          provider_tool_name: name.clone(),
          turn_id: turn_id.clone(),
        },
      );
    }

    let (tool_name, tool_kind, summary, semantic_input) = if let Some(decoded) = decoded {
      let tool_name = decoded.tool.name().to_string();
      let tool_kind = tool_kind_for_name(&tool_name);
      let summary = tool_summary_for_input(&tool_name, &decoded.input);
      (tool_name, tool_kind, summary, decoded.input)
    } else if code_mode {
      (
        name.clone(),
        ToolKind::CodeExecution,
        Some(ToolSummary::CodeExecution {
          language: Some("javascript".to_string()),
        }),
        input,
      )
    } else {
      let tool_kind = tool_kind_for_name(&name);
      let summary = tool_summary_for_input(&name, &input);
      (name.clone(), tool_kind, summary, input)
    };
    let provider_tool_name = code_mode.then(|| name.clone());

    vec![AgentEvent::ToolCall(ToolCallEvent {
      provider: Provider::Codex,
      session_id: self.session_id.clone(),
      turn_id,
      message_id: item.id,
      parent_id: None,
      record_kind: ToolRecordKind::Invocation,
      tool_call_id: item.call_id,
      provider_tool_name,
      tool_name: Some(tool_name),
      tool_kind,
      transport: code_mode.then_some(ToolTransport::CodeExecution),
      summary,
      phase: Phase::Started,
      input: Some(semantic_input),
      output: None,
      is_error: None,
      native: Some(native),
      timestamp,
    })]
  }

  fn normalize_custom_tool_call_output(
    &mut self,
    item: tokn_codex_protocol::CustomToolCallOutputItem,
    timestamp: Option<String>,
  ) -> Vec<AgentEvent> {
    let native = json_value(&item);
    let turn_id = response_turn_id(item.internal_chat_message_metadata_passthrough.as_ref());
    let pending = self.take_code_mode_call(item.call_id.as_deref(), turn_id.as_deref());
    let had_pending_code_mode_call = pending.is_some();
    let fallback_provider_tool_name = pending.as_ref().map(|call| call.provider_tool_name.clone());
    let fallback_turn_id = pending.as_ref().and_then(|call| call.turn_id.clone());

    if let Some(pending) = pending {
      if let Some(call) = pending.call {
        if let Some(output) = decode_output(call.tool, &item.output) {
          let tool_name = call.tool.name().to_string();
          let is_error = output.get("exit_code").and_then(Value::as_i64).map(|code| code != 0);
          return vec![AgentEvent::ToolCall(ToolCallEvent {
            provider: Provider::Codex,
            session_id: self.session_id.clone(),
            turn_id: turn_id.or(pending.turn_id),
            message_id: item.id,
            parent_id: None,
            record_kind: ToolRecordKind::Result,
            tool_call_id: item.call_id,
            provider_tool_name: Some(pending.provider_tool_name),
            tool_name: Some(tool_name.clone()),
            tool_kind: tool_kind_for_name(&tool_name),
            transport: Some(ToolTransport::CodeExecution),
            summary: tool_summary_for_io(Some(&tool_name), Some(&call.input), Some(&output)),
            phase: Phase::Finished,
            input: Some(call.input),
            output: Some(output),
            is_error,
            native: Some(native),
            timestamp,
          })];
        }
      }
    }

    let name = item
      .name
      .or(fallback_provider_tool_name)
      .unwrap_or_else(|| "custom_tool".to_string());
    let code_mode = had_pending_code_mode_call;
    let (tool_kind, summary, transport) = if code_mode {
      (
        ToolKind::CodeExecution,
        Some(ToolSummary::CodeExecution {
          language: Some("javascript".to_string()),
        }),
        Some(ToolTransport::CodeExecution),
      )
    } else {
      (tool_kind_for_name(&name), None, None)
    };

    vec![AgentEvent::ToolCall(ToolCallEvent {
      provider: Provider::Codex,
      session_id: self.session_id.clone(),
      turn_id: turn_id.or(fallback_turn_id),
      message_id: item.id,
      parent_id: None,
      record_kind: ToolRecordKind::Result,
      tool_call_id: item.call_id,
      provider_tool_name: code_mode.then(|| name.clone()),
      tool_name: Some(name),
      tool_kind,
      transport,
      summary,
      phase: Phase::Finished,
      input: None,
      output: Some(item.output),
      is_error: None,
      native: Some(native),
      timestamp,
    })]
  }

  fn remember_code_mode_call(&mut self, call_id: Option<String>, call: PendingCodeModeCall) {
    let Some(call_id) = call_id else {
      return;
    };
    let mut call = call;
    call.token = self.next_pending_code_mode_token;
    self.next_pending_code_mode_token = self.next_pending_code_mode_token.wrapping_add(1);
    self.pending_code_mode_order.push_back((call_id.clone(), call.token));
    self.pending_code_mode_calls.entry(call_id).or_default().push_back(call);
    self.pending_code_mode_call_count += 1;
    self.trim_pending_code_mode_calls();
  }

  fn take_code_mode_call(
    &mut self,
    call_id: Option<&str>,
    result_turn_id: Option<&str>,
  ) -> Option<PendingCodeModeCall> {
    let call_id = call_id?;
    let pending = self.pending_code_mode_calls.get_mut(call_id)?;
    let index = pending
      .iter()
      .position(|call| match (call.turn_id.as_deref(), result_turn_id) {
        (Some(invocation_turn_id), Some(result_turn_id)) => invocation_turn_id == result_turn_id,
        _ => true,
      })?;
    let call = pending.remove(index);
    if pending.is_empty() {
      self.pending_code_mode_calls.remove(call_id);
    }
    if let Some(call) = &call {
      self.pending_code_mode_call_count = self.pending_code_mode_call_count.saturating_sub(1);
      self
        .pending_code_mode_order
        .retain(|(queued_call_id, token)| queued_call_id != call_id || *token != call.token);
    }
    call
  }

  fn trim_pending_code_mode_calls(&mut self) {
    while self.pending_code_mode_call_count > MAX_PENDING_CODE_MODE_CALLS {
      let Some((call_id, token)) = self.pending_code_mode_order.pop_front() else {
        self.pending_code_mode_call_count = 0;
        self.pending_code_mode_calls.clear();
        break;
      };
      let removed = self.pending_code_mode_calls.get_mut(&call_id).and_then(|calls| {
        calls
          .iter()
          .position(|call| call.token == token)
          .and_then(|index| calls.remove(index))
      });
      if self
        .pending_code_mode_calls
        .get(&call_id)
        .is_some_and(VecDeque::is_empty)
      {
        self.pending_code_mode_calls.remove(&call_id);
      }
      if removed.is_some() {
        self.pending_code_mode_call_count = self.pending_code_mode_call_count.saturating_sub(1);
      }
    }
  }
}

pub(crate) fn requires_thread_spawn_boundary(item: &SessionMetaItem) -> bool {
  item.source.as_ref().is_some_and(|source| match source {
    Value::Object(source) => source
      .get("subagent")
      .and_then(|subagent| subagent.get("thread_spawn"))
      .is_some(),
    Value::String(source) => source.starts_with("subagent_thread_spawn"),
    _ => false,
  }) || item.extra.get("subagent_source").and_then(Value::as_str) == Some("thread_spawn")
}

fn normalize_response_item(
  session_id: Option<String>,
  item: ResponseItem,
  timestamp: Option<String>,
) -> Vec<AgentEvent> {
  match item {
    ResponseItem::Message(item) => normalize_message(session_id, item, timestamp),
    ResponseItem::AgentMessage(item) => normalize_agent_message(session_id, item, timestamp, None),
    ResponseItem::Reasoning(item) => normalize_reasoning(session_id, item, timestamp),
    ResponseItem::FunctionCall(item) => {
      if let Some(event) = questions::function_call(session_id.clone(), &item, timestamp.clone()) {
        return vec![event];
      }
      let name = item
        .name
        .or(item.namespace)
        .unwrap_or_else(|| "function_call".to_string());
      let input = parse_json_string_or_value(item.arguments);
      vec![AgentEvent::ToolCall(ToolCallEvent {
        provider: Provider::Codex,
        session_id,
        turn_id: None,
        message_id: item.id,
        parent_id: None,
        record_kind: ToolRecordKind::Invocation,
        tool_call_id: item.call_id,
        provider_tool_name: None,
        tool_name: Some(name.clone()),
        tool_kind: tool_kind_for_name(&name),
        transport: None,
        summary: tool_summary_for_input(&name, &input),
        phase: Phase::Finished,
        input: Some(input),
        output: None,
        is_error: None,
        native: None,
        timestamp,
      })]
    }
    ResponseItem::FunctionCallOutput(item) => vec![tool_output_event(
      session_id,
      item.id,
      item.call_id,
      None,
      item.output,
      timestamp,
    )],
    ResponseItem::LocalShellCall(item) => {
      let (record_kind, phase, is_error) = standalone_tool_call_lifecycle(item.status.as_deref());
      let input = item.action.unwrap_or(Value::Null);
      vec![AgentEvent::ToolCall(ToolCallEvent {
        provider: Provider::Codex,
        session_id,
        turn_id: None,
        message_id: item.id,
        parent_id: None,
        record_kind,
        tool_call_id: item.call_id,
        provider_tool_name: None,
        tool_name: Some("local_shell".to_string()),
        tool_kind: ToolKind::Shell,
        transport: None,
        summary: tool_summary_for_input("local_shell", &input),
        phase,
        input: Some(input),
        // Local shell calls are standalone response items. They can be
        // complete without an additional output envelope.
        output: None,
        is_error,
        native: None,
        timestamp,
      })]
    }
    ResponseItem::CustomToolCall(_) | ResponseItem::CustomToolCallOutput(_) => {
      unreachable!("custom Code Mode calls are normalized with correlation state")
    }
    ResponseItem::ToolSearchCall(item) => {
      let input = parse_json_string_or_value(item.arguments);
      vec![AgentEvent::ToolCall(ToolCallEvent {
        provider: Provider::Codex,
        session_id,
        turn_id: None,
        message_id: item.id,
        parent_id: None,
        record_kind: ToolRecordKind::Invocation,
        tool_call_id: item.call_id,
        provider_tool_name: None,
        tool_name: Some("tool_search".to_string()),
        tool_kind: ToolKind::Search,
        transport: None,
        summary: tool_summary_for_input("tool_search", &input),
        phase: Phase::Finished,
        input: Some(input),
        output: None,
        is_error: None,
        native: None,
        timestamp,
      })]
    }
    ResponseItem::ToolSearchOutput(item) => vec![tool_output_event(
      session_id,
      item.id,
      item.call_id,
      Some("tool_search".to_string()),
      json!({
        "status": item.status,
        "execution": item.execution,
        "tools": item.tools,
      }),
      timestamp,
    )],
    ResponseItem::WebSearchCall(item) => {
      let (record_kind, phase, is_error) = standalone_tool_call_lifecycle(item.status.as_deref());
      let input = item.action.unwrap_or(Value::Null);
      vec![AgentEvent::ToolCall(ToolCallEvent {
        provider: Provider::Codex,
        session_id,
        turn_id: None,
        message_id: item.id,
        parent_id: None,
        record_kind,
        tool_call_id: None,
        provider_tool_name: None,
        tool_name: Some("web_search".to_string()),
        tool_kind: ToolKind::Search,
        transport: None,
        summary: tool_summary_for_input("web_search", &input),
        phase,
        input: Some(input),
        output: None,
        is_error,
        native: None,
        timestamp,
      })]
    }
    ResponseItem::ImageGenerationCall(item) => vec![AgentEvent::ToolCall(ToolCallEvent {
      provider: Provider::Codex,
      session_id,
      turn_id: None,
      message_id: item.id,
      parent_id: None,
      record_kind: ToolRecordKind::Snapshot,
      tool_call_id: None,
      provider_tool_name: None,
      tool_name: Some("image_generation".to_string()),
      tool_kind: ToolKind::Unknown,
      transport: None,
      summary: None,
      phase: Phase::Finished,
      input: item.revised_prompt.map(Value::String),
      output: item.result.map(Value::String),
      is_error: None,
      native: None,
      timestamp,
    })],
    ResponseItem::AdditionalTools(_)
    | ResponseItem::Compaction(_)
    | ResponseItem::CompactionTrigger(_)
    | ResponseItem::ContextCompaction(_) => Vec::new(),
    ResponseItem::Unknown(item) => vec![unknown_response_event(session_id, item, timestamp)],
  }
}

fn normalize_message(session_id: Option<String>, item: MessageItem, timestamp: Option<String>) -> Vec<AgentEvent> {
  let role = item.role.as_deref().unwrap_or("unknown");
  if role != "assistant" {
    return Vec::new();
  }
  let delivery = codex_message_delivery(item.phase.as_deref());

  let text = content_text(&item.content);
  if text.is_empty() {
    return vec![unknown_event(
      session_id,
      Some("response_item.message".to_string()),
      Some(json_value(item)),
      timestamp,
    )];
  }

  vec![AgentEvent::Message(MessageEvent {
    provenance: None,
    provider: Provider::Codex,
    session_id,
    message_id: item.id,
    parent_id: None,
    role: codex_role(role),
    delivery,
    phase: Phase::Finished,
    text,
    timestamp,
  })]
}

fn normalize_reasoning(session_id: Option<String>, item: ReasoningItem, timestamp: Option<String>) -> Vec<AgentEvent> {
  let text = content_text(item.content.as_deref().unwrap_or_default());
  let summary = content_text(&item.summary);
  if text.is_empty() && summary.is_empty() && item.encrypted_content.is_none() {
    return Vec::new();
  }

  vec![AgentEvent::Reasoning(ReasoningEvent {
    provenance: None,
    provider: Provider::Codex,
    session_id,
    message_id: item.id,
    parent_id: None,
    phase: Phase::Finished,
    text: present_text(text),
    summary: present_text(summary),
    redacted: None,
    encrypted_content: item.encrypted_content,
    signature: None,
    timestamp,
  })]
}

fn normalize_event_message(
  session_id: Option<String>,
  item: EventMessage,
  canonical_items: bool,
  timestamp: Option<String>,
) -> Vec<AgentEvent> {
  let payload = item.native;
  match item.event_type.as_deref() {
    Some(kind @ ("task_started" | "turn_started" | "task_complete" | "turn_complete")) => {
      vec![turn_lifecycle::normalize(session_id, &payload, kind, timestamp)]
    }
    Some("user_message") => string_field(&payload, "message")
      .map(|text| {
        vec![message_event(
          session_id,
          None,
          Role::User,
          Phase::Finished,
          text,
          timestamp,
        )]
      })
      .unwrap_or_default(),
    Some("agent_message") => Vec::new(),
    Some("request_user_input") => vec![
      questions::request(session_id.clone(), &payload, timestamp.clone()).unwrap_or_else(|| {
        unknown_event(
          session_id,
          Some("event_msg.request_user_input".into()),
          Some(payload),
          timestamp,
        )
      }),
    ],
    Some("agent_message_delta" | "agent_message_content_delta") => {
      normalize_message_delta(session_id, &payload, timestamp)
    }
    Some("agent_reasoning") => normalize_reasoning_event(session_id, &payload, "text", Phase::Finished, timestamp),
    Some("agent_reasoning_delta") => normalize_reasoning_event(session_id, &payload, "delta", Phase::Delta, timestamp),
    Some("agent_reasoning_raw_content") => {
      normalize_reasoning_event(session_id, &payload, "text", Phase::Finished, timestamp)
    }
    Some("agent_reasoning_raw_content_delta" | "reasoning_raw_content_delta") => {
      normalize_reasoning_event(session_id, &payload, "delta", Phase::Delta, timestamp)
    }
    Some("reasoning_content_delta") => {
      normalize_reasoning_event(session_id, &payload, "delta", Phase::Delta, timestamp)
    }
    Some("item_started") if canonical_items => {
      normalize_item_lifecycle(session_id, &payload, Phase::Started, timestamp)
    }
    Some("item_completed") if canonical_items => {
      normalize_item_lifecycle(session_id, &payload, Phase::Finished, timestamp)
    }
    Some("item_completed") => normalize_legacy_item_completed(session_id, &payload, timestamp),
    Some("exec_command_begin") => normalize_exec_begin(session_id, &payload, timestamp),
    Some("exec_command_output_delta") => normalize_exec_delta(session_id, payload, timestamp),
    Some("exec_command_end") => normalize_exec_end(session_id, &payload, timestamp),
    Some("mcp_tool_call_begin") => normalize_mcp_begin(session_id, &payload, timestamp),
    Some("mcp_tool_call_end") => normalize_mcp_end(session_id, &payload, timestamp),
    Some("web_search_begin") => normalize_web_search_begin(session_id, &payload, timestamp),
    Some("web_search_end") => normalize_web_search_end(session_id, &payload, timestamp),
    Some("patch_apply_begin") => normalize_patch_begin(session_id, &payload, timestamp),
    Some("patch_apply_end") => normalize_patch_end(session_id, &payload, timestamp),
    Some("view_image_tool_call") => normalize_view_image(session_id, &payload, timestamp),
    Some("error" | "warning" | "guardian_warning" | "stream_error") => normalize_error(session_id, &payload, timestamp),
    Some("turn_aborted") => {
      let mut events = normalize_turn_aborted(session_id.clone(), &payload, timestamp.clone());
      events.push(turn_lifecycle::normalize(
        session_id,
        &payload,
        "turn_aborted",
        timestamp,
      ));
      events
    }
    Some("thread_settings_applied") => {
      vec![session_settings_applied_event(session_id, &payload, timestamp)]
    }
    Some("thread_goal_updated") => vec![goal_updated_event(session_id, &payload, timestamp)],
    Some("sub_agent_activity") => normalize_sub_agent_activity(session_id, &payload, timestamp),
    _ => vec![unknown_event(
      session_id,
      item
        .event_type
        .map(|event_type| format!("event_msg.{event_type}"))
        .or_else(|| Some("event_msg".to_string())),
      Some(payload),
      timestamp,
    )],
  }
}

fn normalize_message_delta(session_id: Option<String>, payload: &Value, timestamp: Option<String>) -> Vec<AgentEvent> {
  let Some(text) = string_field(payload, "delta") else {
    return Vec::new();
  };
  vec![message_event(
    session_id,
    string_field(payload, "item_id"),
    Role::Assistant,
    Phase::Delta,
    text,
    timestamp,
  )]
}

fn normalize_reasoning_event(
  session_id: Option<String>,
  payload: &Value,
  field: &str,
  phase: Phase,
  timestamp: Option<String>,
) -> Vec<AgentEvent> {
  let Some(text) = string_field(payload, field).filter(|text| !text.is_empty()) else {
    return Vec::new();
  };
  vec![AgentEvent::Reasoning(ReasoningEvent {
    provenance: None,
    provider: Provider::Codex,
    session_id,
    message_id: string_field(payload, "item_id"),
    parent_id: None,
    phase,
    text: Some(text),
    summary: None,
    redacted: None,
    encrypted_content: None,
    signature: None,
    timestamp,
  })]
}

fn normalize_exec_begin(session_id: Option<String>, payload: &Value, timestamp: Option<String>) -> Vec<AgentEvent> {
  let command = command_value(payload);
  vec![AgentEvent::ToolCall(ToolCallEvent {
    provider: Provider::Codex,
    session_id,
    turn_id: None,
    message_id: None,
    parent_id: None,
    record_kind: ToolRecordKind::Invocation,
    tool_call_id: string_field(payload, "call_id"),
    provider_tool_name: None,
    tool_name: Some("exec_command".to_string()),
    tool_kind: ToolKind::Shell,
    transport: None,
    summary: Some(ToolSummary::Shell {
      command: command_text(command.as_ref()),
      cwd: path_field(payload, "cwd"),
      exit_code: None,
    }),
    phase: Phase::Started,
    input: command,
    output: None,
    is_error: None,
    native: None,
    timestamp,
  })]
}

fn normalize_exec_delta(session_id: Option<String>, payload: Value, timestamp: Option<String>) -> Vec<AgentEvent> {
  vec![AgentEvent::ToolCall(ToolCallEvent {
    provider: Provider::Codex,
    session_id,
    turn_id: None,
    message_id: None,
    parent_id: None,
    record_kind: ToolRecordKind::Progress,
    tool_call_id: string_field(&payload, "call_id"),
    provider_tool_name: None,
    tool_name: Some("exec_command".to_string()),
    tool_kind: ToolKind::Shell,
    transport: None,
    summary: None,
    phase: Phase::Delta,
    input: None,
    output: Some(payload),
    is_error: None,
    native: None,
    timestamp,
  })]
}

fn normalize_exec_end(session_id: Option<String>, payload: &Value, timestamp: Option<String>) -> Vec<AgentEvent> {
  let exit_code = payload.get("exit_code").and_then(Value::as_i64);
  vec![AgentEvent::ToolCall(ToolCallEvent {
    provider: Provider::Codex,
    session_id,
    turn_id: None,
    message_id: None,
    parent_id: None,
    record_kind: ToolRecordKind::Result,
    tool_call_id: string_field(payload, "call_id"),
    provider_tool_name: None,
    tool_name: Some("exec_command".to_string()),
    tool_kind: ToolKind::Shell,
    transport: None,
    summary: Some(ToolSummary::Shell {
      command: command_text(command_value(payload).as_ref()),
      cwd: path_field(payload, "cwd"),
      exit_code,
    }),
    phase: Phase::Finished,
    input: None,
    output: Some(json!({
      "stdout": payload.get("stdout").cloned().unwrap_or(Value::Null),
      "stderr": payload.get("stderr").cloned().unwrap_or(Value::Null),
      "aggregated_output": payload.get("aggregated_output").cloned().unwrap_or(Value::Null),
      "formatted_output": payload.get("formatted_output").cloned().unwrap_or(Value::Null),
    })),
    is_error: exit_code.map(|code| code != 0),
    native: None,
    timestamp,
  })]
}

fn normalize_mcp_begin(session_id: Option<String>, payload: &Value, timestamp: Option<String>) -> Vec<AgentEvent> {
  let invocation = payload.get("invocation");
  let name = invocation.and_then(mcp_name);
  let input = invocation.and_then(|value| value.get("arguments")).cloned();
  vec![AgentEvent::ToolCall(ToolCallEvent {
    provider: Provider::Codex,
    session_id,
    turn_id: None,
    message_id: None,
    parent_id: None,
    record_kind: ToolRecordKind::Invocation,
    tool_call_id: string_field(payload, "call_id"),
    provider_tool_name: None,
    tool_name: name.clone(),
    tool_kind: tool_kind_for_optional_name(name.as_deref()),
    transport: None,
    summary: tool_summary_for_io(name.as_deref(), input.as_ref(), None),
    phase: Phase::Started,
    input,
    output: None,
    is_error: None,
    native: None,
    timestamp,
  })]
}

fn normalize_mcp_end(session_id: Option<String>, payload: &Value, timestamp: Option<String>) -> Vec<AgentEvent> {
  let name = payload.get("invocation").and_then(mcp_name);
  let output = payload.get("result").cloned().unwrap_or(Value::Null);
  vec![AgentEvent::ToolCall(ToolCallEvent {
    provider: Provider::Codex,
    session_id,
    turn_id: None,
    message_id: None,
    parent_id: None,
    record_kind: ToolRecordKind::Result,
    tool_call_id: string_field(payload, "call_id"),
    provider_tool_name: None,
    tool_name: name.clone(),
    tool_kind: tool_kind_for_optional_name(name.as_deref()),
    transport: None,
    summary: None,
    phase: Phase::Finished,
    input: None,
    output: Some(output.clone()),
    is_error: Some(mcp_result_is_error(&output)),
    native: None,
    timestamp,
  })]
}

fn normalize_web_search_begin(
  session_id: Option<String>,
  payload: &Value,
  timestamp: Option<String>,
) -> Vec<AgentEvent> {
  vec![tool_lifecycle_event(
    session_id,
    string_field(payload, "call_id"),
    "web_search",
    ToolKind::Search,
    Phase::Started,
    None,
    None,
    timestamp,
  )]
}

fn normalize_web_search_end(session_id: Option<String>, payload: &Value, timestamp: Option<String>) -> Vec<AgentEvent> {
  let query = string_field(payload, "query");
  vec![AgentEvent::ToolCall(ToolCallEvent {
    provider: Provider::Codex,
    session_id,
    turn_id: None,
    message_id: None,
    parent_id: None,
    record_kind: ToolRecordKind::Result,
    tool_call_id: string_field(payload, "call_id"),
    provider_tool_name: None,
    tool_name: Some("web_search".to_string()),
    tool_kind: ToolKind::Search,
    transport: None,
    summary: Some(ToolSummary::Search { query: query.clone() }),
    phase: Phase::Finished,
    input: payload.get("action").cloned(),
    output: Some(json!({
      "query": query,
      "results": payload.get("results").cloned().unwrap_or(Value::Null),
    })),
    is_error: None,
    native: None,
    timestamp,
  })]
}

fn normalize_patch_begin(session_id: Option<String>, payload: &Value, timestamp: Option<String>) -> Vec<AgentEvent> {
  let changes = payload.get("changes").cloned().unwrap_or(Value::Null);
  vec![tool_lifecycle_event(
    session_id,
    string_field(payload, "call_id"),
    "apply_patch",
    ToolKind::FileEdit,
    Phase::Started,
    Some(changes.clone()),
    Some(patch_summary(&changes)),
    timestamp,
  )]
}

fn normalize_patch_end(session_id: Option<String>, payload: &Value, timestamp: Option<String>) -> Vec<AgentEvent> {
  let changes = payload.get("changes").cloned().unwrap_or(Value::Null);
  vec![AgentEvent::ToolCall(ToolCallEvent {
    provider: Provider::Codex,
    session_id,
    turn_id: None,
    message_id: None,
    parent_id: None,
    record_kind: ToolRecordKind::Result,
    tool_call_id: string_field(payload, "call_id"),
    provider_tool_name: None,
    tool_name: Some("apply_patch".to_string()),
    tool_kind: ToolKind::FileEdit,
    transport: None,
    summary: Some(patch_summary(&changes)),
    phase: Phase::Finished,
    input: None,
    output: Some(json!({
      "stdout": payload.get("stdout").cloned().unwrap_or(Value::Null),
      "stderr": payload.get("stderr").cloned().unwrap_or(Value::Null),
    })),
    is_error: payload.get("success").and_then(Value::as_bool).map(|success| !success),
    native: None,
    timestamp,
  })]
}

fn normalize_view_image(session_id: Option<String>, payload: &Value, timestamp: Option<String>) -> Vec<AgentEvent> {
  let path = path_field(payload, "path");
  vec![AgentEvent::ToolCall(ToolCallEvent {
    provider: Provider::Codex,
    session_id,
    turn_id: None,
    message_id: None,
    parent_id: None,
    record_kind: ToolRecordKind::Snapshot,
    tool_call_id: string_field(payload, "call_id"),
    provider_tool_name: None,
    tool_name: Some("view_image".to_string()),
    tool_kind: ToolKind::FileRead,
    transport: None,
    summary: Some(ToolSummary::FileRead { path: path.clone() }),
    phase: Phase::Finished,
    input: path.map(|path| json!({ "path": path })),
    output: None,
    is_error: None,
    native: None,
    timestamp,
  })]
}

fn normalize_error(session_id: Option<String>, payload: &Value, timestamp: Option<String>) -> Vec<AgentEvent> {
  string_field(payload, "message")
    .map(|message| {
      vec![AgentEvent::Error(ErrorEvent {
        provider: Provider::Codex,
        session_id,
        message,
        timestamp,
      })]
    })
    .unwrap_or_default()
}

fn normalize_turn_aborted(session_id: Option<String>, payload: &Value, timestamp: Option<String>) -> Vec<AgentEvent> {
  let message = payload
    .get("reason")
    .map(display_json_value)
    .unwrap_or_else(|| "turn aborted".to_string());
  vec![AgentEvent::Error(ErrorEvent {
    provider: Provider::Codex,
    session_id,
    message,
    timestamp,
  })]
}

fn goal_updated_event(session_id: Option<String>, payload: &Value, timestamp: Option<String>) -> AgentEvent {
  AgentEvent::GoalUpdated(GoalUpdated {
    provider: Provider::Codex,
    session_id: string_field_any(payload, &["thread_id", "threadId"]).or(session_id),
    turn_id: string_field_any(payload, &["turn_id", "turnId"]),
    goal: payload.get("goal").cloned(),
    timestamp,
  })
}

fn normalize_sub_agent_activity(
  session_id: Option<String>,
  payload: &Value,
  timestamp: Option<String>,
) -> Vec<AgentEvent> {
  let Some(kind) = string_field(payload, "kind") else {
    return vec![unknown_event(
      session_id,
      Some("event_msg.sub_agent_activity".to_string()),
      Some(payload.clone()),
      timestamp,
    )];
  };
  vec![AgentEvent::AgentActivity(AgentActivity {
    provider: Provider::Codex,
    session_id,
    event_id: string_field(payload, "event_id"),
    actor_session_id: None,
    actor_agent_path: None,
    target_session_id: string_field(payload, "agent_thread_id"),
    target_agent_path: string_field(payload, "agent_path"),
    kind,
    communication: None,
    occurred_at_ms: payload.get("occurred_at_ms").and_then(Value::as_u64),
    native: Some(payload.clone()),
    timestamp,
  })]
}

fn session_settings_applied_event(
  session_id: Option<String>,
  payload: &Value,
  timestamp: Option<String>,
) -> AgentEvent {
  let settings = payload.get("thread_settings");
  AgentEvent::SessionSettingsApplied(SessionSettingsApplied {
    provider: Provider::Codex,
    session_id,
    model_provider: settings.and_then(|settings| string_field(settings, "model_provider_id")),
    model_id: settings.and_then(|settings| string_field(settings, "model")),
    service_tier: settings.and_then(|settings| string_field(settings, "service_tier")),
    cwd: settings.and_then(|settings| string_field(settings, "cwd")),
    reasoning_effort: settings.and_then(|settings| string_field(settings, "reasoning_effort")),
    reasoning_summary: settings.and_then(|settings| string_field(settings, "reasoning_summary")),
    personality: settings.and_then(|settings| string_field(settings, "personality")),
    collaboration_mode: settings
      .and_then(|settings| settings.get("collaboration_mode"))
      .and_then(|mode| string_field(mode, "mode")),
    approval_policy: settings.and_then(|settings| string_field(settings, "approval_policy")),
    approvals_reviewer: settings.and_then(|settings| string_field(settings, "approvals_reviewer")),
    active_permission_profile_id: settings
      .and_then(|settings| settings.get("active_permission_profile"))
      .and_then(|profile| string_field(profile, "id")),
    native: settings.cloned(),
    timestamp,
  })
}

fn tool_lifecycle_event(
  session_id: Option<String>,
  call_id: Option<String>,
  name: &str,
  kind: ToolKind,
  phase: Phase,
  input: Option<Value>,
  summary: Option<ToolSummary>,
  timestamp: Option<String>,
) -> AgentEvent {
  AgentEvent::ToolCall(ToolCallEvent {
    provider: Provider::Codex,
    session_id,
    turn_id: None,
    message_id: None,
    parent_id: None,
    record_kind: tool_record_kind_for_phase(phase),
    tool_call_id: call_id,
    provider_tool_name: None,
    tool_name: Some(name.to_string()),
    tool_kind: kind,
    transport: None,
    summary,
    phase,
    input,
    output: None,
    is_error: None,
    native: None,
    timestamp,
  })
}

fn tool_output_event(
  session_id: Option<String>,
  message_id: Option<String>,
  call_id: Option<String>,
  name: Option<String>,
  output: Value,
  timestamp: Option<String>,
) -> AgentEvent {
  AgentEvent::ToolCall(ToolCallEvent {
    provider: Provider::Codex,
    session_id,
    turn_id: None,
    message_id,
    parent_id: None,
    record_kind: ToolRecordKind::Result,
    tool_call_id: call_id,
    provider_tool_name: None,
    tool_name: name.clone(),
    tool_kind: tool_kind_for_optional_name(name.as_deref()),
    transport: None,
    summary: None,
    phase: Phase::Finished,
    input: None,
    output: Some(output),
    is_error: None,
    native: None,
    timestamp,
  })
}

pub(super) fn tool_record_kind_for_phase(phase: Phase) -> ToolRecordKind {
  match phase {
    Phase::Started => ToolRecordKind::Invocation,
    Phase::Delta => ToolRecordKind::Progress,
    Phase::Updated => ToolRecordKind::Snapshot,
    Phase::Finished => ToolRecordKind::Result,
  }
}

/// Response items such as `local_shell_call` and `web_search_call` are
/// standalone snapshots: Codex does not emit a matching result item for them.
/// Preserve an explicitly in-progress snapshot as pending, but let a
/// historical completed/failed item finish its logical operation without
/// inventing an output record.
fn standalone_tool_call_lifecycle(status: Option<&str>) -> (ToolRecordKind, Phase, Option<bool>) {
  let status = status
    .map(str::trim)
    .filter(|status| !status.is_empty())
    .map(str::to_ascii_lowercase);
  match status.as_deref() {
    Some("in_progress" | "pending" | "running") => (ToolRecordKind::Invocation, Phase::Started, None),
    Some("failed" | "error" | "cancelled" | "canceled") => (ToolRecordKind::Snapshot, Phase::Finished, Some(true)),
    _ => (ToolRecordKind::Snapshot, Phase::Finished, None),
  }
}

fn message_event(
  session_id: Option<String>,
  message_id: Option<String>,
  role: Role,
  phase: Phase,
  text: String,
  timestamp: Option<String>,
) -> AgentEvent {
  AgentEvent::Message(MessageEvent {
    provenance: None,
    provider: Provider::Codex,
    session_id,
    message_id,
    parent_id: None,
    role,
    delivery: MessageDelivery::Unspecified,
    phase,
    text,
    timestamp,
  })
}

fn unknown_rollout_event(session_id: Option<String>, item: UnknownItem, timestamp: Option<String>) -> AgentEvent {
  unknown_event(session_id, item.native_type, Some(item.payload), timestamp)
}

fn unknown_response_event(session_id: Option<String>, item: UnknownItem, timestamp: Option<String>) -> AgentEvent {
  let native_type = item
    .native_type
    .map(|native_type| format!("response_item.{native_type}"))
    .or_else(|| Some("response_item".to_string()));
  unknown_event(session_id, native_type, Some(item.payload), timestamp)
}

fn unknown_event(
  session_id: Option<String>,
  native_type: Option<String>,
  native: Option<Value>,
  timestamp: Option<String>,
) -> AgentEvent {
  AgentEvent::Unknown(UnknownEvent {
    provider: Provider::Codex,
    session_id,
    native_type,
    native,
    timestamp,
  })
}

fn content_text(content: &[ContentItem]) -> String {
  content
    .iter()
    .filter_map(|item| item.text.as_deref())
    .filter(|text| !text.is_empty())
    .collect::<Vec<_>>()
    .join("\n")
}

fn codex_role(role: &str) -> Role {
  match role {
    "user" => Role::User,
    "assistant" => Role::Assistant,
    "system" | "developer" => Role::System,
    "tool" => Role::Tool,
    _ => Role::Unknown,
  }
}

fn codex_message_delivery(phase: Option<&str>) -> MessageDelivery {
  match phase {
    Some("commentary") => MessageDelivery::Commentary,
    Some("completed") | Some("final") | Some("final_answer") => MessageDelivery::Final,
    _ => MessageDelivery::Unspecified,
  }
}

fn parse_json_string_or_value(value: Value) -> Value {
  match value {
    Value::String(text) => serde_json::from_str(&text).unwrap_or(Value::String(text)),
    value => value,
  }
}

fn present_text(text: String) -> Option<String> {
  (!text.is_empty()).then_some(text)
}

fn string_field(value: &Value, field: &str) -> Option<String> {
  value
    .get(field)
    .and_then(Value::as_str)
    .filter(|value| !value.is_empty())
    .map(str::to_string)
}

fn response_turn_id(metadata: Option<&Value>) -> Option<String> {
  metadata.and_then(|metadata| string_field(metadata, "turn_id"))
}

fn string_field_any(value: &Value, fields: &[&str]) -> Option<String> {
  fields.iter().find_map(|field| string_field(value, field))
}

fn path_field(value: &Value, field: &str) -> Option<String> {
  let path = value.get(field)?;
  path
    .as_str()
    .map(str::to_string)
    .or_else(|| string_field_any(path, &["path", "uri"]))
}

fn command_value(payload: &Value) -> Option<Value> {
  payload.get("command").cloned().filter(|command| !command.is_null())
}

fn command_text(command: Option<&Value>) -> Option<String> {
  match command {
    Some(Value::String(command)) => Some(command.clone()),
    Some(Value::Array(parts)) => {
      let command = parts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ");
      (!command.is_empty()).then_some(command)
    }
    _ => None,
  }
}

fn mcp_name(invocation: &Value) -> Option<String> {
  let server = string_field(invocation, "server")?;
  let tool = string_field(invocation, "tool")?;
  Some(format!("{server}.{tool}"))
}

fn mcp_result_is_error(result: &Value) -> bool {
  if result.get("Err").is_some() || result.get("err").is_some() {
    return true;
  }
  result
    .get("Ok")
    .or_else(|| result.get("ok"))
    .unwrap_or(result)
    .get("is_error")
    .or_else(|| {
      result
        .get("Ok")
        .or_else(|| result.get("ok"))
        .unwrap_or(result)
        .get("isError")
    })
    .and_then(Value::as_bool)
    .unwrap_or(false)
}

fn display_json_value(value: &Value) -> String {
  value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string())
}

fn json_value(value: impl serde::Serialize) -> Value {
  serde_json::to_value(value).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn normalize_fixture(input: &str) -> Vec<AgentEvent> {
    let mut normalizer = CodexNormalizer::new();
    input
      .lines()
      .filter(|line| !line.trim().is_empty())
      .flat_map(|line| {
        let line: CodexLine = serde_json::from_str(line).expect("fixture line should parse");
        normalizer.normalize(line)
      })
      .collect()
  }

  fn normalize_historical_fixture(input: &str) -> (Vec<AgentEvent>, SessionHistoryStatus) {
    let mut normalizer = CodexNormalizer::new_historical();
    let events = input
      .lines()
      .filter(|line| !line.trim().is_empty())
      .flat_map(|line| {
        let line: CodexLine = serde_json::from_str(line).expect("fixture line should parse");
        normalizer.normalize(line)
      })
      .collect();
    (events, normalizer.history_status())
  }

  fn normalize_value(normalizer: &mut CodexNormalizer, value: Value) -> Vec<AgentEvent> {
    normalizer.normalize(serde_json::from_value(value).expect("record should decode"))
  }

  #[test]
  fn normalizes_basic_fixture_events() {
    let events = normalize_fixture(include_str!("../fixtures/basic_session.jsonl"));

    assert_eq!(events.len(), 11);
    assert!(matches!(&events[0], AgentEvent::SessionStarted(event) if event.session_id == "session-fixture"));
    assert!(
      matches!(&events[1], AgentEvent::ProviderChanged(event) if event.model_provider.as_deref() == Some("openai"))
    );
    assert!(
      matches!(&events[2], AgentEvent::Message(event) if matches!(event.role, Role::User) && event.text == "build a tiny test")
    );
    assert!(matches!(&events[3], AgentEvent::Message(event)
        if event.message_id.as_deref() == Some("msg-assistant")
          && matches!(event.delivery, MessageDelivery::Final)
          && event.text == "done"));
    assert!(matches!(&events[4], AgentEvent::Reasoning(event) if event.summary.as_deref() == Some("checked files")));
    assert!(
      matches!(&events[5], AgentEvent::ToolCall(event) if matches!(event.phase, Phase::Started) && matches!(event.tool_kind, ToolKind::Shell))
    );
    assert!(
      matches!(&events[6], AgentEvent::ToolCall(event) if matches!(event.phase, Phase::Started) && matches!(event.tool_kind, ToolKind::FileEdit))
    );
    assert!(matches!(&events[7], AgentEvent::GoalUpdated(event) if event.turn_id.as_deref() == Some("turn-1")));
    assert!(
      matches!(&events[8], AgentEvent::SessionSettingsApplied(event) if event.model_id.as_deref() == Some("gpt-5"))
    );
    assert!(
      matches!(&events[9], AgentEvent::AgentActivity(event) if event.target_agent_path.as_deref() == Some("/root"))
    );
    assert!(
      matches!(&events[10], AgentEvent::Unknown(event) if event.native_type.as_deref() == Some("event_msg.new_native_event"))
    );
  }

  #[test]
  fn response_item_call_status_is_not_exposed_as_tool_output() {
    let events = normalize_fixture(
      r#"{"type":"response_item","payload":{"type":"local_shell_call","id":"local-1","call_id":"local-call","status":"completed","action":{"command":["pwd"]}}}
{"type":"response_item","payload":{"type":"custom_tool_call","id":"custom-1","call_id":"custom-call","name":"exec","status":"completed","input":{"cmd":"cargo test"}}}
{"type":"response_item","payload":{"type":"custom_tool_call_output","id":"custom-output","call_id":"custom-call","name":"exec","output":"custom result"}}
{"type":"response_item","payload":{"type":"tool_search_call","id":"search-1","call_id":"search-call","status":"completed","arguments":{"query":"browser"}}}
{"type":"response_item","payload":{"type":"tool_search_output","id":"search-output","call_id":"search-call","status":"completed","execution":"local","tools":[]}}
{"type":"response_item","payload":{"type":"web_search_call","id":"web-1","status":"completed","action":{"query":"rust"}}}"#,
    );

    assert_eq!(events.len(), 6);
    for index in [0, 1, 3, 5] {
      let AgentEvent::ToolCall(call) = &events[index] else {
        panic!("expected invocation at index {index}");
      };
      assert!(call.input.is_some());
      assert!(call.output.is_none());
    }
    assert!(matches!(&events[0], AgentEvent::ToolCall(call)
      if matches!(call.record_kind, ToolRecordKind::Snapshot)
        && matches!(call.phase, Phase::Finished)));
    assert!(matches!(&events[5], AgentEvent::ToolCall(call)
      if matches!(call.record_kind, ToolRecordKind::Snapshot)
        && matches!(call.phase, Phase::Finished)));
    assert!(matches!(&events[2], AgentEvent::ToolCall(call)
      if call.output.as_ref().and_then(Value::as_str) == Some("custom result")));
    assert!(matches!(&events[4], AgentEvent::ToolCall(call)
      if call.output.as_ref().is_some_and(Value::is_object)));

    let operations = tokn_session_core::assemble_tool_operations(&events);
    for tool_name in ["local_shell", "web_search"] {
      assert!(operations.iter().any(|operation| {
        operation.tool_name.as_deref() == Some(tool_name)
          && matches!(operation.status, tokn_session_core::ToolOperationStatus::Completed)
      }));
    }
  }

  #[test]
  fn projects_strict_code_mode_wrappers_into_semantic_tool_operations() {
    let events = normalize_fixture(include_str!("../fixtures/code_mode_wrappers.jsonl"));
    assert_eq!(events.len(), 5);

    let AgentEvent::ToolCall(write_invocation) = &events[1] else {
      panic!("expected write_stdin invocation");
    };
    assert_eq!(write_invocation.turn_id.as_deref(), Some("turn-write"));
    assert!(matches!(write_invocation.record_kind, ToolRecordKind::Invocation));
    assert!(matches!(write_invocation.phase, Phase::Started));
    assert_eq!(write_invocation.provider_tool_name.as_deref(), Some("exec"));
    assert_eq!(write_invocation.tool_name.as_deref(), Some("write_stdin"));
    assert!(matches!(write_invocation.tool_kind, ToolKind::Terminal));
    assert!(matches!(write_invocation.transport, Some(ToolTransport::CodeExecution)));
    assert!(matches!(
      &write_invocation.summary,
      Some(ToolSummary::Terminal {
        session_id: Some(session_id),
        action: Some(tokn_session_core::TerminalAction::Wait),
        chars_len: Some(0),
        wait_ms: Some(30_000),
      }) if session_id == "90855"
    ));
    assert_eq!(
      write_invocation.input,
      Some(json!({
        "session_id": 90855,
        "chars": "",
        "yield_time_ms": 30000,
        "max_output_tokens": 4000,
      }))
    );
    assert!(write_invocation.output.is_none());
    assert_eq!(
      write_invocation
        .native
        .as_ref()
        .and_then(|native| native.get("type"))
        .and_then(Value::as_str),
      Some("custom_tool_call")
    );
    assert_eq!(
      write_invocation
        .native
        .as_ref()
        .and_then(|native| native.get("input"))
        .and_then(Value::as_str),
      Some(
        "const r = await tools.write_stdin({session_id: 90855, chars: \"\", yield_time_ms: 30000, max_output_tokens: 4000});\ntext(JSON.stringify(r));\n"
      )
    );

    let AgentEvent::ToolCall(write_result) = &events[2] else {
      panic!("expected write_stdin result");
    };
    assert_eq!(write_result.turn_id.as_deref(), Some("turn-write"));
    assert!(matches!(write_result.record_kind, ToolRecordKind::Result));
    assert!(matches!(write_result.phase, Phase::Finished));
    assert_eq!(write_result.provider_tool_name.as_deref(), Some("exec"));
    assert_eq!(write_result.tool_name.as_deref(), Some("write_stdin"));
    assert!(matches!(write_result.tool_kind, ToolKind::Terminal));
    assert_eq!(write_result.input, write_invocation.input);
    assert_eq!(
      write_result.output,
      Some(json!({
        "session_id": 90855,
        "chunk_id": "842651",
        "wall_time_seconds": 30.001430708,
        "original_token_count": 179,
        "text": "Refreshing checks status",
      }))
    );
    assert!(
      write_result
        .native
        .as_ref()
        .and_then(|native| native.get("output"))
        .is_some_and(Value::is_array)
    );
    assert_eq!(
      write_result
        .native
        .as_ref()
        .and_then(|native| native.get("type"))
        .and_then(Value::as_str),
      Some("custom_tool_call_output")
    );

    let AgentEvent::ToolCall(command_invocation) = &events[3] else {
      panic!("expected exec_command invocation");
    };
    assert!(matches!(command_invocation.record_kind, ToolRecordKind::Invocation));
    assert_eq!(command_invocation.provider_tool_name.as_deref(), Some("exec"));
    assert_eq!(command_invocation.tool_name.as_deref(), Some("exec_command"));
    assert!(matches!(command_invocation.tool_kind, ToolKind::Shell));
    assert_eq!(
      command_invocation.input,
      Some(json!({"cmd": "pwd", "yield_time_ms": 1000}))
    );

    let AgentEvent::ToolCall(command_result) = &events[4] else {
      panic!("expected exec_command result");
    };
    assert!(matches!(command_result.record_kind, ToolRecordKind::Result));
    assert_eq!(command_result.tool_name.as_deref(), Some("exec_command"));
    assert!(matches!(
      &command_result.summary,
      Some(ToolSummary::Shell {
        command: Some(command),
        exit_code: Some(0),
        ..
      }) if command == "pwd"
    ));
    assert_eq!(
      command_result.output,
      Some(json!({
        "session_id": 34,
        "exit_code": 0,
        "wall_time_seconds": 0.01,
        "text": "/tmp/project\n",
      }))
    );

    let operations = tokn_session_core::assemble_tool_operations(&events);
    assert_eq!(operations.len(), 2);
    assert!(matches!(
      operations[0].status,
      tokn_session_core::ToolOperationStatus::Completed
    ));
    assert_eq!(operations[0].tool_name.as_deref(), Some("write_stdin"));
    assert_eq!(operations[0].output, write_result.output);
    assert_eq!(operations[0].native.len(), 2);
    assert_eq!(operations[1].tool_name.as_deref(), Some("exec_command"));
    assert_eq!(operations[1].output, command_result.output);
  }

  #[test]
  fn preserves_non_generated_code_mode_programs_and_results_as_raw_code_execution() {
    let events = normalize_fixture(
      r#"{"type":"session_meta","payload":{"id":"code-mode-session"}}
{"type":"response_item","payload":{"type":"custom_tool_call","id":"dynamic-call","call_id":"dynamic","name":"exec","input":"const r = await tools.write_stdin({session_id: process.pid, chars: \"x\"});\ntext(JSON.stringify(r));\n"}}
{"type":"response_item","payload":{"type":"custom_tool_call_output","id":"dynamic-result","call_id":"dynamic","output":[{"type":"input_text","text":"Script completed\nWall time 0.0 seconds\nOutput:\n"},{"type":"input_text","text":"{\"session_id\":1,\"output\":\"would be unsafe to infer\"}"}]}}"#,
    );

    let AgentEvent::ToolCall(invocation) = &events[1] else {
      panic!("expected code execution invocation");
    };
    assert!(matches!(invocation.record_kind, ToolRecordKind::Invocation));
    assert_eq!(invocation.provider_tool_name.as_deref(), Some("exec"));
    assert_eq!(invocation.tool_name.as_deref(), Some("exec"));
    assert!(matches!(invocation.tool_kind, ToolKind::CodeExecution));
    assert!(matches!(invocation.transport, Some(ToolTransport::CodeExecution)));
    assert!(invocation.input.as_ref().is_some_and(Value::is_string));

    let AgentEvent::ToolCall(result) = &events[2] else {
      panic!("expected raw code execution result");
    };
    assert!(matches!(result.record_kind, ToolRecordKind::Result));
    assert_eq!(result.tool_name.as_deref(), Some("exec"));
    assert!(matches!(result.tool_kind, ToolKind::CodeExecution));
    assert!(result.output.as_ref().is_some_and(Value::is_array));
  }

  #[test]
  fn correlates_reused_code_mode_call_ids_by_turn_before_falling_back_to_order() {
    let wrapper = |session_id: u64| {
      format!(
        "const r = await tools.write_stdin({{session_id: {session_id}, chars: \"x\"}});\ntext(JSON.stringify(r));\n"
      )
    };
    let result = |session_id: u64, text: &str| {
      json!([
        {"type": "input_text", "text": "Script completed\nWall time 0.0 seconds\nOutput:\n"},
        {"type": "input_text", "text": format!("{{\"session_id\":{session_id},\"output\":\"{text}\"}}")},
      ])
    };
    let records = [
      json!({"type": "session_meta", "payload": {"id": "code-mode-session"}}),
      json!({"type": "response_item", "payload": {
        "type": "custom_tool_call", "id": "call-a", "call_id": "reused", "name": "exec",
        "input": wrapper(1), "internal_chat_message_metadata_passthrough": {"turn_id": "turn-a"},
      }}),
      json!({"type": "response_item", "payload": {
        "type": "custom_tool_call", "id": "call-b", "call_id": "reused", "name": "exec",
        "input": wrapper(2), "internal_chat_message_metadata_passthrough": {"turn_id": "turn-b"},
      }}),
      json!({"type": "response_item", "payload": {
        "type": "custom_tool_call_output", "id": "result-b", "call_id": "reused", "output": result(2, "second"),
        "internal_chat_message_metadata_passthrough": {"turn_id": "turn-b"},
      }}),
      json!({"type": "response_item", "payload": {
        "type": "custom_tool_call_output", "id": "result-a", "call_id": "reused", "output": result(1, "first"),
        "internal_chat_message_metadata_passthrough": {"turn_id": "turn-a"},
      }}),
    ];
    let mut normalizer = CodexNormalizer::new();
    let events = records
      .into_iter()
      .flat_map(|record| normalize_value(&mut normalizer, record))
      .collect::<Vec<_>>();

    let AgentEvent::ToolCall(second_result) = &events[3] else {
      panic!("expected second result");
    };
    assert_eq!(second_result.turn_id.as_deref(), Some("turn-b"));
    assert_eq!(
      second_result.input.as_ref().and_then(|input| input.get("session_id")),
      Some(&json!(2))
    );
    assert_eq!(
      second_result.output.as_ref().and_then(|output| output.get("text")),
      Some(&json!("second"))
    );

    let AgentEvent::ToolCall(first_result) = &events[4] else {
      panic!("expected first result");
    };
    assert_eq!(first_result.turn_id.as_deref(), Some("turn-a"));
    assert_eq!(
      first_result.input.as_ref().and_then(|input| input.get("session_id")),
      Some(&json!(1))
    );
    assert_eq!(
      first_result.output.as_ref().and_then(|output| output.get("text")),
      Some(&json!("first"))
    );
  }

  #[test]
  fn bounds_unmatched_code_mode_correlations() {
    let mut normalizer = CodexNormalizer::new();
    normalize_value(
      &mut normalizer,
      json!({"type": "session_meta", "payload": {"id": "code-mode-session"}}),
    );

    for index in 0..=MAX_PENDING_CODE_MODE_CALLS {
      normalize_value(
        &mut normalizer,
        json!({"type": "response_item", "payload": {
          "type": "custom_tool_call",
          "id": format!("item-{index}"),
          "call_id": format!("call-{index}"),
          "name": "exec",
          "input": format!(
            "const r = await tools.write_stdin({{session_id: {index}, chars: \"\"}});\ntext(JSON.stringify(r));\n"
          ),
        }}),
      );
    }

    assert_eq!(normalizer.pending_code_mode_call_count, MAX_PENDING_CODE_MODE_CALLS);
    assert_eq!(normalizer.pending_code_mode_order.len(), MAX_PENDING_CODE_MODE_CALLS);
    assert_eq!(
      normalizer
        .pending_code_mode_calls
        .values()
        .map(VecDeque::len)
        .sum::<usize>(),
      MAX_PENDING_CODE_MODE_CALLS
    );
  }

  #[test]
  fn normalizes_current_protocol_records_without_unknown_noise() {
    let events = normalize_fixture(
      r#"{"type":"session_meta","payload":{"id":"session-1","cwd":"/tmp/project"}}
{"type":"response_item","payload":{"type":"custom_tool_call_output","id":"out-1","call_id":"call-1","name":"exec","output":[{"type":"input_text","text":"done"}]}}
{"type":"response_item","payload":{"type":"agent_message","id":"amsg-1","author":"/root","recipient":"/root/reviewer","content":[{"type":"input_text","text":"review this"}]}}
{"type":"inter_agent_communication_metadata","payload":{"trigger_turn":true}}
{"type":"world_state","payload":{"full":false,"state":{"large":"value"}}}
{"type":"turn_context","payload":{"turn_id":"turn-1","effort":"ultra"}}"#,
    );

    assert_eq!(events.len(), 6);
    let AgentEvent::ToolCall(output) = &events[1] else {
      panic!("expected custom tool output");
    };
    assert_eq!(output.message_id.as_deref(), Some("out-1"));
    assert!(output.output.as_ref().is_some_and(Value::is_array));
    let AgentEvent::AgentActivity(activity) = &events[2] else {
      panic!("expected agent activity");
    };
    assert_eq!(activity.actor_agent_path.as_deref(), Some("/root"));
    assert_eq!(activity.target_agent_path.as_deref(), Some("/root/reviewer"));
    assert_eq!(activity.kind, "messaged");
    assert!(events[3..].iter().all(|event| matches!(event, AgentEvent::Metadata(_))));
  }

  #[test]
  fn normalizes_canonical_item_lifecycle_without_duplicate_conversation_events() {
    let events = normalize_fixture(include_str!("../fixtures/item_lifecycle_session.jsonl"));

    assert_eq!(events.len(), 17);
    assert!(matches!(&events[0], AgentEvent::SessionStarted(event) if event.session_id == "canonical-session"));
    assert!(matches!(&events[1], AgentEvent::Message(event)
      if matches!(event.role, Role::User)
        && event.message_id.as_deref() == Some("user-1")
        && event.text == "hello world"));
    assert!(matches!(&events[2], AgentEvent::Message(event)
      if matches!(event.role, Role::Assistant)
        && matches!(event.delivery, MessageDelivery::Commentary)
        && event.text == "working"));
    assert!(matches!(&events[3], AgentEvent::Reasoning(event)
      if event.summary.as_deref() == Some("checking")
        && event.encrypted_content.as_deref() == Some("encrypted-reasoning")));
    assert!(matches!(&events[4], AgentEvent::Metadata(event)
      if event.native_type == "event_msg.item_completed.Plan"
        && event.summary == "plan completed"));
    assert!(matches!(&events[5], AgentEvent::ToolCall(event)
      if matches!(event.phase, Phase::Started)
        && event.tool_call_id.as_deref() == Some("exec-1")
        && event.input.as_ref().is_some_and(Value::is_array)));
    assert!(matches!(&events[6], AgentEvent::ToolCall(event)
      if matches!(event.phase, Phase::Finished)
        && event.tool_call_id.as_deref() == Some("exec-1")
        && event.is_error == Some(false)));
    assert!(matches!(&events[7], AgentEvent::ToolCall(event)
      if matches!(event.tool_kind, ToolKind::FileEdit)
        && event.tool_call_id.as_deref() == Some("patch-1")
        && event.is_error == Some(true)));
    assert!(matches!(&events[8], AgentEvent::ToolCall(event)
      if matches!(event.phase, Phase::Started)
        && event.tool_call_id.as_deref() == Some("mcp-1")));
    assert!(matches!(&events[9], AgentEvent::ToolCall(event)
      if event.tool_call_id.as_deref() == Some("mcp-1")
        && event.tool_name.as_deref() == Some("codex_app.read_thread_terminal")
        && event.is_error == Some(true)));
    assert!(matches!(&events[10], AgentEvent::ToolCall(event)
      if matches!(event.tool_kind, ToolKind::Task)
        && event.tool_name.as_deref() == Some("wait")));
    assert!(matches!(&events[11], AgentEvent::AgentActivity(event)
      if event.event_id.as_deref() == Some("subagent-1")
        && event.target_session_id.as_deref() == Some("child-1")
        && event.occurred_at_ms == Some(1014)));
    assert!(matches!(&events[12], AgentEvent::ToolCall(event)
      if matches!(event.tool_kind, ToolKind::Search)
        && event.tool_call_id.as_deref() == Some("search-1")));
    assert!(
      matches!(&events[13], AgentEvent::Compaction(event) if event.state == tokn_session_core::CompactionState::Completed)
    );
    assert!(matches!(&events[14], AgentEvent::Unknown(event)
      if event.native_type.as_deref() == Some("event_msg.item_completed.FutureItem")));
    assert!(matches!(&events[15], AgentEvent::ToolCall(event)
      if event.tool_call_id.as_deref() == Some("dynamic-1")));
    assert!(matches!(&events[16], AgentEvent::Unknown(event)
      if event.native_type.as_deref() == Some("event_msg.item_completed.SubAgentActivity")));
  }

  #[test]
  fn paginated_history_uses_one_canonical_projection_per_item() {
    let mut normalizer = CodexNormalizer::new();
    normalize_value(
      &mut normalizer,
      json!({"type":"session_meta","payload":{"id":"session-1","history_mode":"paginated"}}),
    );
    assert!(
      normalize_value(
        &mut normalizer,
        json!({"type":"session_meta","payload":{"id":"copied-parent","history_mode":"legacy"}}),
      )
      .is_empty()
    );
    assert!(
      normalize_value(
        &mut normalizer,
        json!({"type":"response_item","payload":{"type":"message","id":"message-1","role":"assistant",
          "content":[{"type":"output_text","text":"canonical answer"}],"phase":"final_answer"}}),
      )
      .is_empty()
    );
    let mut events = normalize_value(
      &mut normalizer,
      json!({"type":"response_item","payload":{"type":"reasoning","id":"reasoning-1",
        "summary":[{"type":"summary_text","text":"canonical thought"}],
        "encrypted_content":"encrypted-reasoning"}}),
    );
    assert!(matches!(&events[..], [AgentEvent::Reasoning(event)]
      if event.encrypted_content.as_deref() == Some("encrypted-reasoning")));
    for raw in [
      json!({"type":"response_item","payload":{"type":"custom_tool_call","id":"raw-call","name":"exec",
        "call_id":"call-1","input":{"cmd":"cargo test"}}}),
      json!({"type":"response_item","payload":{"type":"custom_tool_call_output","id":"raw-output",
        "call_id":"call-1","name":"exec","output":"ok"}}),
    ] {
      assert!(normalize_value(&mut normalizer, raw).is_empty());
    }

    let canonical = [
      json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"session-1","turn_id":"turn-1",
        "item":{"type":"AgentMessage","id":"message-1","content":[{"type":"Text","text":"canonical answer"}],
          "phase":"final_answer"},"completed_at_ms":1}}),
      json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"session-1","turn_id":"turn-1",
        "item":{"type":"Reasoning","id":"reasoning-1","summary_text":["canonical thought"],"raw_content":[]},
        "completed_at_ms":2}}),
      json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"session-1","turn_id":"turn-1",
        "item":{"type":"CommandExecution","id":"exec-1","command":["cargo","test"],"cwd":"file:///tmp/project",
          "parsed_cmd":[],"source":"agent","status":"completed","stdout":"ok","stderr":"","exit_code":0},
        "completed_at_ms":3}}),
    ];
    events.extend(
      canonical
        .into_iter()
        .flat_map(|record| normalize_value(&mut normalizer, record)),
    );

    assert_eq!(events.len(), 3);
    assert_eq!(
      events
        .iter()
        .filter(|event| matches!(event, AgentEvent::Message(_)))
        .count(),
      1
    );
    assert_eq!(
      events
        .iter()
        .filter(|event| matches!(event, AgentEvent::Reasoning(_)))
        .count(),
      1
    );
    assert_eq!(
      events
        .iter()
        .filter(|event| matches!(event, AgentEvent::ToolCall(_)))
        .count(),
      1
    );
  }

  #[test]
  fn every_current_canonical_item_variant_has_an_explicit_disposition() {
    let mut normalizer = CodexNormalizer::new();
    normalize_value(
      &mut normalizer,
      json!({"type":"session_meta","payload":{"id":"session-1","history_mode":"paginated"}}),
    );
    let items = [
      json!({"type":"UserMessage","id":"user","content":[{"type":"text","text":"hello"}]}),
      json!({"type":"HookPrompt","id":"hook","fragments":[{"text":"hook text","hookRunId":"run-1"}]}),
      json!({"type":"AgentMessage","id":"agent","content":[{"type":"Text","text":"answer"}],"phase":"final_answer"}),
      json!({"type":"Plan","id":"plan","text":"inspect then fix"}),
      json!({"type":"Reasoning","id":"reasoning","summary_text":["summary"],"raw_content":["detail"]}),
      json!({"type":"CommandExecution","id":"command","command":["true"],"cwd":"file:///tmp","parsed_cmd":[],
        "source":"agent","status":"completed","stdout":"","stderr":"","exit_code":0}),
      json!({"type":"DynamicToolCall","id":"dynamic","namespace":"tools","tool":"lookup","arguments":{},
        "status":"completed","content_items":[],"success":true}),
      json!({"type":"CollabAgentToolCall","id":"collab","tool":"wait","status":"completed",
        "sender_thread_id":"session-1","receiver_thread_ids":[],"receiver_agents":[],"agents_states":{}}),
      json!({"type":"SubAgentActivity","id":"activity","kind":"completed","agent_thread_id":"child-1",
        "agent_path":"/root/child"}),
      json!({"type":"WebSearch","id":"hosted-search","query":"query","action":{"type":"search","query":"query"},
        "results":[]}),
      json!({"type":"ImageView","id":"image-view","path":"file:///tmp/image.png"}),
      json!({"type":"Extension","kind":"image_gen.generation","id":"extension-image","status":"completed",
        "revisedPrompt":"blue square","result":"image-data","savedPath":"/tmp/image.png"}),
      json!({"type":"Extension","kind":"clock.sleep","id":"extension-sleep","durationMs":10}),
      json!({"type":"Extension","kind":"web.search","id":"extension-search","query":"query","action":null,
        "results":[]}),
      json!({"type":"ImageGeneration","id":"hosted-image","status":"completed","revised_prompt":"blue square",
        "result":"image-data","saved_path":"/tmp/image.png"}),
      json!({"type":"EnteredReviewMode","id":"review-enter","target":{"type":"uncommittedChanges"},
        "user_facing_hint":"review changes"}),
      json!({"type":"ExitedReviewMode","id":"review-exit","review_output":null}),
      json!({"type":"FileChange","id":"file-change","changes":{},"status":"completed","stdout":"","stderr":""}),
      json!({"type":"McpToolCall","id":"mcp","server":"server","tool":"tool","arguments":{},"status":"completed",
        "result":{"content":[],"isError":true}}),
      json!({"type":"ContextCompaction","id":"compaction"}),
    ];

    for item in items {
      let label = if item["type"] == "Extension" {
        format!("Extension.{}", item["kind"].as_str().unwrap())
      } else {
        item["type"].as_str().unwrap().to_string()
      };
      let events = normalize_value(
        &mut normalizer,
        json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"session-1","turn_id":"turn-1",
          "item":item,"completed_at_ms":100}}),
      );
      if label == "Reasoning" {
        assert!(events.is_empty(), "{label} should be a validated suppressed duplicate");
        continue;
      }
      assert!(!events.is_empty(), "{label} should have a visible disposition");
      assert!(
        events.iter().all(|event| !matches!(event, AgentEvent::Unknown(_))),
        "{label} should not be unknown"
      );
      if label == "McpToolCall" {
        assert!(matches!(&events[..], [AgentEvent::ToolCall(event)] if event.is_error == Some(true)));
      }
    }
  }

  #[test]
  fn legacy_history_keeps_raw_projection_and_only_handles_unpaired_canonical_items() {
    let events = normalize_fixture(
      r#"{"type":"session_meta","payload":{"id":"session-1","history_mode":"legacy"}}
{"type":"response_item","payload":{"type":"message","id":"raw-message","role":"assistant","content":[{"type":"output_text","text":"raw answer"}],"phase":"final_answer"}}
{"type":"event_msg","payload":{"type":"item_completed","thread_id":"session-1","turn_id":"turn-1","item":{"type":"AgentMessage","id":"canonical-message","content":[{"type":"Text","text":"canonical answer"}],"phase":"final_answer"},"completed_at_ms":1}}
{"type":"event_msg","payload":{"type":"item_completed","thread_id":"session-1","turn_id":"turn-1","item":{"type":"Plan","id":"plan","text":"plan body"},"completed_at_ms":2}}
{"type":"event_msg","payload":{"type":"item_completed","thread_id":"session-1","turn_id":"turn-1","item":{"type":"Extension","kind":"clock.sleep","id":"sleep","durationMs":10},"completed_at_ms":3}}"#,
    );

    assert!(matches!(&events[1], AgentEvent::Message(event) if event.text == "raw answer"));
    assert!(matches!(&events[2], AgentEvent::Unknown(event)
      if event.native_type.as_deref() == Some("event_msg.item_completed")));
    assert!(matches!(&events[3], AgentEvent::Metadata(event) if event.summary == "plan completed"));
    assert!(matches!(&events[4], AgentEvent::ToolCall(event) if event.tool_name.as_deref() == Some("sleep")));
  }

  #[test]
  fn future_and_malformed_extensions_keep_specific_unknown_identity() {
    let events = normalize_fixture(
      r#"{"type":"session_meta","payload":{"id":"session-1","history_mode":"paginated"}}
{"type":"event_msg","payload":{"type":"item_completed","thread_id":"session-1","turn_id":"turn-1","item":{"type":"Extension","kind":"future.kind","id":"future"},"completed_at_ms":1}}
{"type":"event_msg","payload":{"type":"item_completed","thread_id":"session-1","turn_id":"turn-1","item":{"type":"Extension","kind":"web.search","id":"broken","query":"query","action":[]},"completed_at_ms":2}}"#,
    );

    assert!(matches!(&events[1], AgentEvent::Unknown(event)
      if event.native_type.as_deref() == Some("event_msg.item_completed.Extension.future.kind")));
    assert!(matches!(&events[2], AgentEvent::Unknown(event)
      if event.native_type.as_deref() == Some("event_msg.item_completed.Extension.web.search")));
  }

  #[test]
  fn keeps_unknown_response_identity_and_payload() {
    let events = normalize_fixture(
      r#"{"type":"session_meta","payload":{"id":"session-1"}}
{"type":"response_item","payload":{"type":"future_response","id":"future-1","answer":42}}"#,
    );

    let AgentEvent::Unknown(event) = &events[1] else {
      panic!("expected unknown event");
    };
    assert_eq!(event.native_type.as_deref(), Some("response_item.future_response"));
    assert_eq!(
      event.native.as_ref().and_then(|value| value.get("answer")),
      Some(&json!(42))
    );
  }

  #[test]
  fn paginated_history_preserves_unknown_response_items() {
    let events = normalize_fixture(
      r#"{"type":"session_meta","payload":{"id":"session-1","history_mode":"paginated"}}
{"type":"response_item","payload":{"type":"future_response","answer":42}}"#,
    );

    assert!(matches!(&events[1], AgentEvent::Unknown(event)
      if event.native_type.as_deref() == Some("response_item.future_response")
        && event.native.as_ref().is_some_and(|native| native["answer"] == 42)));
  }

  #[test]
  fn malformed_canonical_reasoning_stays_unknown() {
    let events = normalize_fixture(
      r#"{"type":"session_meta","payload":{"id":"session-1","history_mode":"paginated"}}
{"type":"event_msg","payload":{"type":"item_completed","thread_id":"session-1","turn_id":"turn-1","item":{"type":"Reasoning","id":"reasoning-1","summary_text":[42]},"completed_at_ms":1}}"#,
    );

    assert!(matches!(&events[1], AgentEvent::Unknown(event)
      if event.native_type.as_deref() == Some("event_msg.item_completed.Reasoning")));
  }

  #[test]
  fn preserves_assistant_message_delivery() {
    let events = normalize_fixture(
      r#"{"type":"session_meta","payload":{"id":"session-1"}}
{"type":"response_item","payload":{"type":"message","id":"commentary","role":"assistant","content":[{"type":"output_text","text":"working"}],"phase":"commentary"}}
{"type":"response_item","payload":{"type":"message","id":"final","role":"assistant","content":[{"type":"output_text","text":"done"}],"phase":"final"}}
{"type":"response_item","payload":{"type":"message","id":"final-answer","role":"assistant","content":[{"type":"output_text","text":"current done"}],"phase":"final_answer"}}
{"type":"response_item","payload":{"type":"message","id":"completed","role":"assistant","content":[{"type":"output_text","text":"legacy done"}],"phase":"completed"}}"#,
    );

    assert!(matches!(&events[1], AgentEvent::Message(event) if matches!(event.delivery, MessageDelivery::Commentary)));
    assert!(matches!(&events[2], AgentEvent::Message(event) if matches!(event.delivery, MessageDelivery::Final)));
    assert!(matches!(&events[3], AgentEvent::Message(event) if matches!(event.delivery, MessageDelivery::Final)));
    assert!(matches!(&events[4], AgentEvent::Message(event) if matches!(event.delivery, MessageDelivery::Final)));
  }

  #[test]
  fn keeps_first_session_header_when_parent_history_is_copied() {
    let events = normalize_fixture(
      r#"{"timestamp":"2026-07-24T17:52:40Z","type":"session_meta","payload":{"id":"child-session","parent_thread_id":"root-session","timestamp":"2026-07-24T17:52:40Z","cwd":"/tmp/project","agent_path":"/root/researcher"}}
{"timestamp":"2026-07-15T10:00:00Z","type":"session_meta","payload":{"id":"root-session","timestamp":"2026-07-15T10:00:00Z","cwd":"/tmp/project"}}
{"timestamp":"2026-07-24T17:54:07Z","type":"event_msg","payload":{"type":"sub_agent_activity","event_id":"call-agent","occurred_at_ms":1784915647361,"agent_thread_id":"root-session","agent_path":"/root","kind":"interacted"}}"#,
    );

    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], AgentEvent::SessionStarted(event) if event.session_id == "child-session"));
    assert!(
      matches!(&events[1], AgentEvent::AgentActivity(event) if event.target_session_id.as_deref() == Some("root-session"))
    );
  }

  #[test]
  fn default_normalizer_keeps_streaming_subagent_events_without_a_historical_boundary() {
    let events = normalize_fixture(
      r#"{"type":"session_meta","payload":{"id":"child-session","parent_thread_id":"root-session"}}
{"type":"response_item","payload":{"type":"message","id":"child-message","role":"assistant","content":[{"type":"output_text","text":"live child result"}],"phase":"final"}}"#,
    );

    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], AgentEvent::SessionStarted(event) if event.session_id == "child-session"));
    assert!(matches!(&events[1], AgentEvent::Message(event) if event.text == "live child result"));
  }

  #[test]
  fn historical_normalizer_starts_subagent_body_at_a_legacy_trigger_boundary() {
    let (events, status) = normalize_historical_fixture(
      r#"{"type":"session_meta","payload":{"id":"child-session","parent_thread_id":"root-session","source":{"subagent":{"thread_spawn":{"parent_thread_id":"root-session","depth":1,"agent_path":"/root/child"}}}}}
{"type":"event_msg","payload":{"type":"user_message","message":"copied parent request"}}
{"type":"inter_agent_communication","payload":{"id":"queued","author":"/root","recipient":"/root/child","content":"queued","trigger_turn":false}}
{"type":"inter_agent_communication","payload":{"id":"trigger","author":"/root","recipient":"/root/child","content":"start","trigger_turn":true}}
{"type":"response_item","payload":{"type":"message","id":"child-message","role":"assistant","content":[{"type":"output_text","text":"owned child result"}],"phase":"final"}}"#,
    );

    assert_eq!(status, SessionHistoryStatus::FilteredSubagent);
    assert_eq!(events.len(), 3);
    assert!(matches!(&events[0], AgentEvent::SessionStarted(event) if event.session_id == "child-session"));
    assert!(matches!(&events[1], AgentEvent::AgentActivity(event) if event.event_id.as_deref() == Some("trigger")));
    assert!(matches!(&events[2], AgentEvent::Message(event) if event.text == "owned child result"));
  }
}
