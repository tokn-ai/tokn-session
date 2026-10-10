use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

pub type ExtraFields = Map<String, Value>;

/// One JSONL record from a persisted Codex rollout.
///
/// The native record is retained so decoding never prevents downstream
/// consumers from inspecting or forwarding fields this crate does not model.
#[derive(Debug, Clone, PartialEq)]
pub struct RolloutLine {
  timestamp: Option<String>,
  ordinal: Option<u64>,
  item: RolloutItem,
  token_usage_record: Option<Box<TokenUsageRecordItem>>,
  native: Value,
}

impl RolloutLine {
  pub fn timestamp(&self) -> Option<&str> {
    self.timestamp.as_deref()
  }

  pub fn ordinal(&self) -> Option<u64> {
    self.ordinal
  }

  pub fn item(&self) -> &RolloutItem {
    &self.item
  }

  pub fn into_item(self) -> RolloutItem {
    self.item
  }

  /// Typed accounting for newer rollouts without extending the published
  /// exhaustive `RolloutItem` enum. The same record remains `Unknown` in
  /// `item()`; malformed accounting has no typed view and remains inspectable.
  pub fn token_usage_record(&self) -> Option<&TokenUsageRecordItem> {
    self.token_usage_record.as_deref()
  }

  pub fn native(&self) -> &Value {
    &self.native
  }

  pub fn into_native(self) -> Value {
    self.native
  }
}

impl<'de> Deserialize<'de> for RolloutLine {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let native = Value::deserialize(deserializer)?;
    let timestamp = string_field(&native, "timestamp");
    let ordinal = native.get("ordinal").and_then(Value::as_u64);
    let native_type = native.get("type").and_then(Value::as_str).map(str::to_string);
    let payload = native.get("payload").unwrap_or(&Value::Null);
    let is_token_usage_record = native_type.as_deref() == Some("token_usage_record");
    let mut item = decode_rollout_item(native_type, payload);
    let token_usage_record = if is_token_usage_record {
      match TokenUsageRecordItem::deserialize(payload) {
        Ok(record) => Some(Box::new(record)),
        Err(error) => {
          if let RolloutItem::Unknown(unknown) = &mut item {
            unknown.parse_error = Some(error.to_string());
          }
          None
        }
      }
    } else {
      None
    };

    Ok(Self {
      timestamp,
      ordinal,
      item,
      token_usage_record,
      native,
    })
  }
}

impl Serialize for RolloutLine {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    self.native.serialize(serializer)
  }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RolloutItem {
  SessionMeta(SessionMetaItem),
  ResponseItem(ResponseItem),
  InterAgentCommunication(InterAgentCommunicationItem),
  InterAgentCommunicationMetadata(InterAgentCommunicationMetadataItem),
  Compacted(CompactedItem),
  TurnContext(Box<TurnContextItem>),
  WorldState(WorldStateItem),
  EventMessage(EventMessage),
  Unknown(UnknownItem),
}

impl RolloutItem {
  pub fn native_type(&self) -> Option<&str> {
    match self {
      Self::SessionMeta(_) => Some("session_meta"),
      Self::ResponseItem(_) => Some("response_item"),
      Self::InterAgentCommunication(_) => Some("inter_agent_communication"),
      Self::InterAgentCommunicationMetadata(_) => Some("inter_agent_communication_metadata"),
      Self::Compacted(_) => Some("compacted"),
      Self::TurnContext(_) => Some("turn_context"),
      Self::WorldState(_) => Some("world_state"),
      Self::EventMessage(_) => Some("event_msg"),
      Self::Unknown(item) => item.native_type.as_deref(),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SessionMetaItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub timestamp: Option<String>,
  #[serde(default)]
  pub cwd: Option<String>,
  #[serde(default)]
  pub model_provider: Option<String>,
  #[serde(default)]
  pub parent_thread_id: Option<String>,
  #[serde(default)]
  pub source: Option<Value>,
  #[serde(default)]
  pub git: Option<SessionGitInfo>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

impl SessionMetaItem {
  /// The newer history-mode field stays in `extra` so existing struct
  /// literals remain valid. Invalid types are errors, never a legacy fallback.
  pub fn history_mode(&self) -> Result<Option<&str>, serde_json::Error> {
    match self.extra.get("history_mode") {
      Some(value) => Option::<&str>::deserialize(value),
      None => Ok(None),
    }
  }

  /// Decode an inherited prefix without silently treating malformed metadata
  /// as a standalone rollout. Unknown extension fields remain in the result.
  pub fn history_base(&self) -> Result<Option<HistoryPosition>, serde_json::Error> {
    match self.extra.get("history_base") {
      Some(value) => Option::<HistoryPosition>::deserialize(value),
      None => Ok(None),
    }
  }
}

/// Exclusive physical prefix inherited from another rollout segment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryPosition {
  pub thread_id: String,
  pub end_ordinal_exclusive: u64,
  pub end_byte_offset: u64,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SessionGitInfo {
  #[serde(default)]
  pub commit_hash: Option<String>,
  #[serde(default)]
  pub branch: Option<String>,
  #[serde(default)]
  pub repository_url: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct InterAgentCommunicationItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub author: Option<String>,
  #[serde(default)]
  pub recipient: Option<String>,
  #[serde(default)]
  pub other_recipients: Vec<String>,
  #[serde(default)]
  pub content: Option<String>,
  #[serde(default)]
  pub encrypted_content: Option<String>,
  #[serde(default)]
  pub internal_chat_message_metadata_passthrough: Option<Value>,
  #[serde(default)]
  pub trigger_turn: Option<bool>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct InterAgentCommunicationMetadataItem {
  #[serde(default)]
  pub trigger_turn: Option<bool>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CompactedItem {
  #[serde(default)]
  pub message: Option<String>,
  #[serde(default)]
  pub replacement_history: Option<Vec<ResponseItem>>,
  #[serde(default)]
  pub window_number: Option<u64>,
  #[serde(default)]
  pub first_window_id: Option<String>,
  #[serde(default)]
  pub previous_window_id: Option<String>,
  #[serde(default)]
  pub window_id: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

/// Persisted model-response usage with its native accounting attribution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TokenUsageRecordItem {
  #[serde(default)]
  pub thread_id: Option<String>,
  #[serde(default)]
  pub turn_id: Option<String>,
  #[serde(default)]
  pub session_id: Option<String>,
  #[serde(default)]
  pub root_turn_id: Option<String>,
  #[serde(default)]
  pub response_id: Option<String>,
  #[serde(default)]
  pub usage: Option<TokenUsageCounters>,
  #[serde(default)]
  pub turn_token_usage: Option<TokenUsageCounters>,
  #[serde(default)]
  pub thread_token_usage: Option<TokenUsageCounters>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

/// Reported counters; cached input is already included in input tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenUsageCounters {
  pub input_tokens: u64,
  pub cached_input_tokens: u64,
  #[serde(default)]
  pub cache_write_input_tokens: Option<u64>,
  pub output_tokens: u64,
  pub reasoning_output_tokens: u64,
  pub total_tokens: u64,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

/// Per-turn runtime context.
///
/// Permission and collaboration fields deliberately remain JSON because their
/// internal schemas and enum values change more frequently than rollout files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TurnContextItem {
  #[serde(default)]
  pub turn_id: Option<String>,
  #[serde(default)]
  pub cwd: Option<String>,
  #[serde(default)]
  pub workspace_roots: Option<Vec<String>>,
  #[serde(default)]
  pub current_date: Option<String>,
  #[serde(default)]
  pub timezone: Option<String>,
  #[serde(default)]
  pub approval_policy: Option<String>,
  #[serde(default)]
  pub approvals_reviewer: Option<String>,
  #[serde(default)]
  pub sandbox_policy: Option<Value>,
  #[serde(default)]
  pub permission_profile: Option<Value>,
  #[serde(default)]
  pub network: Option<Value>,
  #[serde(default)]
  pub file_system_sandbox_policy: Option<Value>,
  #[serde(default)]
  pub model: Option<String>,
  #[serde(default)]
  pub comp_hash: Option<String>,
  #[serde(default)]
  pub personality: Option<String>,
  #[serde(default)]
  pub collaboration_mode: Option<Value>,
  #[serde(default)]
  pub multi_agent_version: Option<String>,
  #[serde(default)]
  pub multi_agent_mode: Option<String>,
  #[serde(default)]
  pub realtime_active: Option<bool>,
  #[serde(default)]
  pub effort: Option<String>,
  #[serde(default)]
  pub summary: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct WorldStateItem {
  #[serde(default)]
  pub full: Option<bool>,
  #[serde(default)]
  pub state: Value,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

/// An `event_msg` payload retained as a lossless tagged value.
///
/// Event-message variants are numerous and change independently of the
/// persisted rollout envelope. Consumers can inspect `event_type` and decode
/// `native` into the fields they need without losing new variants.
#[derive(Debug, Clone, PartialEq)]
pub struct EventMessage {
  pub event_type: Option<String>,
  pub native: Value,
}

impl<'de> Deserialize<'de> for EventMessage {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let native = Value::deserialize(deserializer)?;
    let event_type = native.get("type").and_then(Value::as_str).map(str::to_string);
    Ok(Self { event_type, native })
  }
}

impl Serialize for EventMessage {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    self.native.serialize(serializer)
  }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResponseItem {
  AdditionalTools(AdditionalToolsItem),
  Message(MessageItem),
  AgentMessage(AgentMessageItem),
  Reasoning(ReasoningItem),
  LocalShellCall(LocalShellCallItem),
  FunctionCall(FunctionCallItem),
  ToolSearchCall(ToolSearchCallItem),
  FunctionCallOutput(FunctionCallOutputItem),
  CustomToolCall(CustomToolCallItem),
  CustomToolCallOutput(CustomToolCallOutputItem),
  ToolSearchOutput(ToolSearchOutputItem),
  WebSearchCall(WebSearchCallItem),
  ImageGenerationCall(ImageGenerationCallItem),
  Compaction(ResponseControlItem),
  CompactionTrigger(ResponseControlItem),
  ContextCompaction(ResponseControlItem),
  Unknown(UnknownItem),
}

impl ResponseItem {
  pub fn native_type(&self) -> Option<&str> {
    match self {
      Self::AdditionalTools(_) => Some("additional_tools"),
      Self::Message(_) => Some("message"),
      Self::AgentMessage(_) => Some("agent_message"),
      Self::Reasoning(_) => Some("reasoning"),
      Self::LocalShellCall(_) => Some("local_shell_call"),
      Self::FunctionCall(_) => Some("function_call"),
      Self::ToolSearchCall(_) => Some("tool_search_call"),
      Self::FunctionCallOutput(_) => Some("function_call_output"),
      Self::CustomToolCall(_) => Some("custom_tool_call"),
      Self::CustomToolCallOutput(_) => Some("custom_tool_call_output"),
      Self::ToolSearchOutput(_) => Some("tool_search_output"),
      Self::WebSearchCall(_) => Some("web_search_call"),
      Self::ImageGenerationCall(_) => Some("image_generation_call"),
      Self::Compaction(_) => Some("compaction"),
      Self::CompactionTrigger(_) => Some("compaction_trigger"),
      Self::ContextCompaction(_) => Some("context_compaction"),
      Self::Unknown(item) => item.native_type.as_deref(),
    }
  }
}

impl<'de> Deserialize<'de> for ResponseItem {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let native = Value::deserialize(deserializer)?;
    Ok(decode_response_item(&native))
  }
}

impl Serialize for ResponseItem {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    match self {
      Self::AdditionalTools(item) => item.serialize(serializer),
      Self::Message(item) => item.serialize(serializer),
      Self::AgentMessage(item) => item.serialize(serializer),
      Self::Reasoning(item) => item.serialize(serializer),
      Self::LocalShellCall(item) => item.serialize(serializer),
      Self::FunctionCall(item) => item.serialize(serializer),
      Self::ToolSearchCall(item) => item.serialize(serializer),
      Self::FunctionCallOutput(item) => item.serialize(serializer),
      Self::CustomToolCall(item) => item.serialize(serializer),
      Self::CustomToolCallOutput(item) => item.serialize(serializer),
      Self::ToolSearchOutput(item) => item.serialize(serializer),
      Self::WebSearchCall(item) => item.serialize(serializer),
      Self::ImageGenerationCall(item) => item.serialize(serializer),
      Self::Compaction(item) => item.serialize(serializer),
      Self::CompactionTrigger(item) => item.serialize(serializer),
      Self::ContextCompaction(item) => item.serialize(serializer),
      Self::Unknown(item) => item.payload.serialize(serializer),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ContentItem {
  #[serde(default, rename = "type")]
  pub content_type: Option<String>,
  #[serde(default)]
  pub text: Option<String>,
  #[serde(default)]
  pub image_url: Option<String>,
  #[serde(default)]
  pub audio_url: Option<String>,
  #[serde(default)]
  pub encrypted_content: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AdditionalToolsItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub role: Option<String>,
  #[serde(default)]
  pub tools: Vec<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MessageItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub role: Option<String>,
  #[serde(default)]
  pub content: Vec<ContentItem>,
  #[serde(default)]
  pub phase: Option<String>,
  #[serde(default)]
  pub internal_chat_message_metadata_passthrough: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AgentMessageItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub author: Option<String>,
  #[serde(default)]
  pub recipient: Option<String>,
  #[serde(default)]
  pub content: Vec<ContentItem>,
  #[serde(default)]
  pub internal_chat_message_metadata_passthrough: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ReasoningItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub summary: Vec<ContentItem>,
  #[serde(default)]
  pub content: Option<Vec<ContentItem>>,
  #[serde(default)]
  pub encrypted_content: Option<String>,
  #[serde(default)]
  pub internal_chat_message_metadata_passthrough: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LocalShellCallItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub call_id: Option<String>,
  #[serde(default)]
  pub status: Option<String>,
  #[serde(default)]
  pub action: Option<Value>,
  #[serde(default)]
  pub internal_chat_message_metadata_passthrough: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FunctionCallItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub name: Option<String>,
  #[serde(default)]
  pub namespace: Option<String>,
  #[serde(default)]
  pub arguments: Value,
  #[serde(default)]
  pub call_id: Option<String>,
  #[serde(default)]
  pub internal_chat_message_metadata_passthrough: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ToolSearchCallItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub call_id: Option<String>,
  #[serde(default)]
  pub status: Option<String>,
  #[serde(default)]
  pub execution: Option<String>,
  #[serde(default)]
  pub arguments: Value,
  #[serde(default)]
  pub internal_chat_message_metadata_passthrough: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FunctionCallOutputItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub call_id: Option<String>,
  #[serde(default)]
  pub output: Value,
  #[serde(default)]
  pub internal_chat_message_metadata_passthrough: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CustomToolCallItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub status: Option<String>,
  #[serde(default)]
  pub call_id: Option<String>,
  #[serde(default)]
  pub name: Option<String>,
  #[serde(default)]
  pub namespace: Option<String>,
  #[serde(default)]
  pub input: Value,
  #[serde(default)]
  pub internal_chat_message_metadata_passthrough: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CustomToolCallOutputItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub call_id: Option<String>,
  #[serde(default)]
  pub name: Option<String>,
  #[serde(default)]
  pub output: Value,
  #[serde(default)]
  pub internal_chat_message_metadata_passthrough: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ToolSearchOutputItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub call_id: Option<String>,
  #[serde(default)]
  pub status: Option<String>,
  #[serde(default)]
  pub execution: Option<String>,
  #[serde(default)]
  pub tools: Vec<Value>,
  #[serde(default)]
  pub internal_chat_message_metadata_passthrough: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct WebSearchCallItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub status: Option<String>,
  #[serde(default)]
  pub action: Option<Value>,
  #[serde(default)]
  pub internal_chat_message_metadata_passthrough: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ImageGenerationCallItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub status: Option<String>,
  #[serde(default)]
  pub revised_prompt: Option<String>,
  #[serde(default)]
  pub result: Option<String>,
  #[serde(default)]
  pub internal_chat_message_metadata_passthrough: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ResponseControlItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub encrypted_content: Option<String>,
  #[serde(default)]
  pub internal_chat_message_metadata_passthrough: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnknownItem {
  pub native_type: Option<String>,
  pub payload: Value,
  pub parse_error: Option<String>,
}

fn decode_rollout_item(native_type: Option<String>, payload: &Value) -> RolloutItem {
  match native_type.as_deref() {
    Some("session_meta") => {
      let parsed = SessionMetaItem::deserialize(payload).and_then(|item| {
        item.history_mode()?;
        item.history_base()?;
        Ok(item)
      });
      match parsed {
        Ok(item) => RolloutItem::SessionMeta(item),
        Err(error) => RolloutItem::Unknown(UnknownItem {
          native_type,
          payload: payload.clone(),
          parse_error: Some(error.to_string()),
        }),
      }
    }
    Some("response_item") => RolloutItem::ResponseItem(decode_response_item(payload)),
    Some("inter_agent_communication") => decode_payload(native_type, payload, RolloutItem::InterAgentCommunication),
    Some("inter_agent_communication_metadata") => {
      decode_payload(native_type, payload, RolloutItem::InterAgentCommunicationMetadata)
    }
    Some("compacted") => decode_payload(native_type, payload, RolloutItem::Compacted),
    Some("turn_context") => decode_payload(native_type, payload, |item| RolloutItem::TurnContext(Box::new(item))),
    Some("world_state") => decode_payload(native_type, payload, RolloutItem::WorldState),
    Some("event_msg") => decode_payload(native_type, payload, RolloutItem::EventMessage),
    _ => RolloutItem::Unknown(UnknownItem {
      native_type,
      payload: payload.clone(),
      parse_error: None,
    }),
  }
}

fn decode_response_item(native: &Value) -> ResponseItem {
  let native_type = native.get("type").and_then(Value::as_str).map(str::to_string);
  match native_type.as_deref() {
    Some("additional_tools") => decode_response_payload(native_type, native, ResponseItem::AdditionalTools),
    Some("message") => decode_response_payload(native_type, native, ResponseItem::Message),
    Some("agent_message") => decode_response_payload(native_type, native, ResponseItem::AgentMessage),
    Some("reasoning") => decode_response_payload(native_type, native, ResponseItem::Reasoning),
    Some("local_shell_call") => decode_response_payload(native_type, native, ResponseItem::LocalShellCall),
    Some("function_call") => decode_response_payload(native_type, native, ResponseItem::FunctionCall),
    Some("tool_search_call") => decode_response_payload(native_type, native, ResponseItem::ToolSearchCall),
    Some("function_call_output") => decode_response_payload(native_type, native, ResponseItem::FunctionCallOutput),
    Some("custom_tool_call") => decode_response_payload(native_type, native, ResponseItem::CustomToolCall),
    Some("custom_tool_call_output") => decode_response_payload(native_type, native, ResponseItem::CustomToolCallOutput),
    Some("tool_search_output") => decode_response_payload(native_type, native, ResponseItem::ToolSearchOutput),
    Some("web_search_call") => decode_response_payload(native_type, native, ResponseItem::WebSearchCall),
    Some("image_generation_call") => decode_response_payload(native_type, native, ResponseItem::ImageGenerationCall),
    Some("compaction") | Some("compaction_summary") => {
      decode_response_payload(native_type, native, ResponseItem::Compaction)
    }
    Some("compaction_trigger") => decode_response_payload(native_type, native, ResponseItem::CompactionTrigger),
    Some("context_compaction") => decode_response_payload(native_type, native, ResponseItem::ContextCompaction),
    _ => ResponseItem::Unknown(UnknownItem {
      native_type,
      payload: native.clone(),
      parse_error: None,
    }),
  }
}

fn decode_payload<T>(native_type: Option<String>, payload: &Value, wrap: impl FnOnce(T) -> RolloutItem) -> RolloutItem
where
  T: DeserializeOwned,
{
  match T::deserialize(payload) {
    Ok(item) => wrap(item),
    Err(error) => RolloutItem::Unknown(UnknownItem {
      native_type,
      payload: payload.clone(),
      parse_error: Some(error.to_string()),
    }),
  }
}

fn decode_response_payload<T>(
  native_type: Option<String>,
  payload: &Value,
  wrap: impl FnOnce(T) -> ResponseItem,
) -> ResponseItem
where
  T: DeserializeOwned,
{
  match T::deserialize(payload) {
    Ok(item) => wrap(item),
    Err(error) => ResponseItem::Unknown(UnknownItem {
      native_type,
      payload: payload.clone(),
      parse_error: Some(error.to_string()),
    }),
  }
}

fn string_field(value: &Value, field: &str) -> Option<String> {
  value.get(field).and_then(Value::as_str).map(str::to_string)
}
