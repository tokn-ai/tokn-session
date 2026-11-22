use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

pub type ExtraFields = Map<String, Value>;

/// One JSONL record from a persisted Pi session file.
///
/// The original record is retained independently of the typed view so callers
/// can forward or inspect fields this crate does not model.
#[derive(Debug, Clone, PartialEq)]
pub struct PiSessionLine {
  timestamp: Option<String>,
  item: PiSessionItem,
  native: Value,
}

impl PiSessionLine {
  pub fn timestamp(&self) -> Option<&str> {
    self.timestamp.as_deref()
  }

  pub fn item(&self) -> &PiSessionItem {
    &self.item
  }

  pub fn into_item(self) -> PiSessionItem {
    self.item
  }

  pub fn native(&self) -> &Value {
    &self.native
  }

  pub fn into_native(self) -> Value {
    self.native
  }

  pub fn into_parts(self) -> (PiSessionItem, Value) {
    (self.item, self.native)
  }
}

impl<'de> Deserialize<'de> for PiSessionLine {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let native = Value::deserialize(deserializer)?;
    let timestamp = string_field(&native, "timestamp");
    let native_type = native.get("type").and_then(Value::as_str).map(str::to_string);
    let item = decode_session_item(native_type, native.clone());

    Ok(Self {
      timestamp,
      item,
      native,
    })
  }
}

impl Serialize for PiSessionLine {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    self.native.serialize(serializer)
  }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PiSessionItem {
  Session(SessionHeader),
  ModelChange(ModelChangeItem),
  ThinkingLevelChange(ThinkingLevelChangeItem),
  Usage(UsageItem),
  Message(MessageItem),
  Compaction(CompactionItem),
  BranchSummary(BranchSummaryItem),
  Custom(CustomItem),
  CustomMessage(CustomMessageItem),
  ContextEdit(ContextEditItem),
  Label(LabelItem),
  SessionInfo(SessionInfoItem),
  Leaf(LeafItem),
  ActiveToolsChange(ActiveToolsChangeItem),
  Error(ErrorItem),
  Unknown(UnknownItem),
}

impl PiSessionItem {
  pub fn native_type(&self) -> Option<&str> {
    match self {
      Self::Session(_) => Some("session"),
      Self::ModelChange(_) => Some("model_change"),
      Self::ThinkingLevelChange(_) => Some("thinking_level_change"),
      Self::Usage(_) => Some("usage"),
      Self::Message(_) => Some("message"),
      Self::Compaction(_) => Some("compaction"),
      Self::BranchSummary(_) => Some("branch_summary"),
      Self::Custom(_) => Some("custom"),
      Self::CustomMessage(_) => Some("custom_message"),
      Self::ContextEdit(_) => Some("context_edit"),
      Self::Label(_) => Some("label"),
      Self::SessionInfo(_) => Some("session_info"),
      Self::Leaf(_) => Some("leaf"),
      Self::ActiveToolsChange(_) => Some("active_tools_change"),
      Self::Error(_) => Some("error"),
      Self::Unknown(item) => item.native_type.as_deref(),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
  #[serde(default)]
  pub version: Option<u64>,
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub timestamp: Option<String>,
  #[serde(default)]
  pub cwd: Option<String>,
  #[serde(default)]
  pub parent_session: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelChangeItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub parent_id: Option<String>,
  #[serde(default)]
  pub timestamp: Option<String>,
  #[serde(default)]
  pub provider: Option<String>,
  #[serde(default)]
  pub model_id: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingLevelChangeItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub parent_id: Option<String>,
  #[serde(default)]
  pub timestamp: Option<String>,
  #[serde(default)]
  pub thinking_level: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MessageItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub parent_id: Option<String>,
  #[serde(default)]
  pub timestamp: Option<String>,
  #[serde(default)]
  pub message: Option<Message>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UsageItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub parent_id: Option<String>,
  #[serde(default)]
  pub timestamp: Option<String>,
  #[serde(default)]
  pub kind: Option<String>,
  #[serde(default)]
  pub provider: Option<String>,
  #[serde(default)]
  pub model: Option<String>,
  #[serde(default)]
  pub usage: Option<Value>,
  #[serde(default)]
  pub note: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

/// An append-only change to model context, not a rewrite of transcript history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ContextEditItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub parent_id: Option<String>,
  #[serde(default)]
  pub timestamp: Option<String>,
  #[serde(default)]
  pub target_id: Option<String>,
  #[serde(default)]
  pub replacement: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
  System(SystemMessage),
  User(UserMessage),
  Assistant(AssistantMessage),
  ToolResult(ToolResultMessage),
  Unknown(UnknownItem),
}

impl<'de> Deserialize<'de> for Message {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let native = Value::deserialize(deserializer)?;
    Ok(decode_message(native))
  }
}

impl Serialize for Message {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    match self {
      Self::System(message) => message.serialize(serializer),
      Self::User(message) => message.serialize(serializer),
      Self::Assistant(message) => message.serialize(serializer),
      Self::ToolResult(message) => message.serialize(serializer),
      Self::Unknown(item) => item.native.serialize(serializer),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SystemMessage {
  #[serde(default)]
  pub content: UserContent,
  #[serde(default)]
  pub sections: Option<Value>,
  #[serde(default)]
  pub tools_added: Option<Value>,
  #[serde(default)]
  pub tools_removed: Option<Value>,
  #[serde(default)]
  pub timestamp: Option<u64>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
  #[serde(default)]
  pub content: UserContent,
  #[serde(default)]
  pub timestamp: Option<u64>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum UserContent {
  Text(String),
  Blocks(Vec<ContentBlock>),
  #[default]
  Missing,
  Unknown(Value),
}

impl<'de> Deserialize<'de> for UserContent {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let native = Value::deserialize(deserializer)?;
    Ok(match native {
      Value::String(text) => Self::Text(text),
      Value::Array(_) => match serde_json::from_value(native.clone()) {
        Ok(blocks) => Self::Blocks(blocks),
        Err(_) => Self::Unknown(native),
      },
      value => Self::Unknown(value),
    })
  }
}

impl Serialize for UserContent {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    match self {
      Self::Text(text) => text.serialize(serializer),
      Self::Blocks(blocks) => blocks.serialize(serializer),
      Self::Missing => Value::Null.serialize(serializer),
      Self::Unknown(value) => value.serialize(serializer),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
  #[serde(default)]
  pub content: Vec<ContentBlock>,
  #[serde(default)]
  pub api: Option<String>,
  #[serde(default)]
  pub provider: Option<String>,
  #[serde(default)]
  pub model: Option<String>,
  #[serde(default)]
  pub response_model: Option<String>,
  #[serde(default)]
  pub response_id: Option<String>,
  #[serde(default)]
  pub diagnostics: Option<Value>,
  #[serde(default)]
  pub usage: Option<Value>,
  #[serde(default)]
  pub stop_reason: Option<String>,
  #[serde(default)]
  pub error_message: Option<String>,
  #[serde(default)]
  pub timestamp: Option<u64>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
  #[serde(default)]
  pub tool_call_id: Option<String>,
  #[serde(default)]
  pub tool_name: Option<String>,
  #[serde(default)]
  pub content: Vec<ContentBlock>,
  #[serde(default)]
  pub details: Option<Value>,
  #[serde(default)]
  pub usage: Option<Value>,
  #[serde(default)]
  pub added_tool_names: Vec<String>,
  #[serde(default)]
  pub is_error: Option<bool>,
  #[serde(default)]
  pub timestamp: Option<u64>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
  Text(TextContent),
  Thinking(ThinkingContent),
  ToolCall(ToolCallContent),
  Image(ImageContent),
  Unknown(UnknownItem),
}

impl ContentBlock {
  pub fn native_type(&self) -> Option<&str> {
    match self {
      Self::Text(_) => Some("text"),
      Self::Thinking(_) => Some("thinking"),
      Self::ToolCall(_) => Some("toolCall"),
      Self::Image(_) => Some("image"),
      Self::Unknown(item) => item.native_type.as_deref(),
    }
  }
}

impl<'de> Deserialize<'de> for ContentBlock {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let native = Value::deserialize(deserializer)?;
    Ok(decode_content_block(native))
  }
}

impl Serialize for ContentBlock {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    match self {
      Self::Text(content) => content.serialize(serializer),
      Self::Thinking(content) => content.serialize(serializer),
      Self::ToolCall(content) => content.serialize(serializer),
      Self::Image(content) => content.serialize(serializer),
      Self::Unknown(item) => item.native.serialize(serializer),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TextContent {
  #[serde(default)]
  pub text: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingContent {
  #[serde(default)]
  pub thinking: Option<String>,
  #[serde(default)]
  pub thinking_signature: Option<String>,
  #[serde(default)]
  pub redacted: Option<bool>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallContent {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub name: Option<String>,
  #[serde(default)]
  pub arguments: Value,
  #[serde(default)]
  pub thought_signature: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
  #[serde(default)]
  pub data: Option<String>,
  #[serde(default)]
  pub mime_type: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CompactionItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub parent_id: Option<String>,
  #[serde(default)]
  pub timestamp: Option<String>,
  #[serde(default)]
  pub summary: Option<String>,
  #[serde(default)]
  pub first_kept_entry_id: Option<String>,
  #[serde(default)]
  pub tokens_before: Option<u64>,
  #[serde(default)]
  pub retained_tail: Option<Vec<Message>>,
  #[serde(default)]
  pub details: Option<Value>,
  #[serde(default)]
  pub usage: Option<Value>,
  #[serde(default)]
  pub from_hook: Option<bool>,
  #[serde(default)]
  pub system_message: Option<SystemMessage>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub parent_id: Option<String>,
  #[serde(default)]
  pub timestamp: Option<String>,
  #[serde(default)]
  pub from_id: Option<String>,
  #[serde(default)]
  pub summary: Option<String>,
  #[serde(default)]
  pub details: Option<Value>,
  #[serde(default)]
  pub usage: Option<Value>,
  #[serde(default)]
  pub from_hook: Option<bool>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CustomItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub parent_id: Option<String>,
  #[serde(default)]
  pub timestamp: Option<String>,
  #[serde(default)]
  pub custom_type: Option<String>,
  #[serde(default)]
  pub data: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessageItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub parent_id: Option<String>,
  #[serde(default)]
  pub timestamp: Option<String>,
  #[serde(default)]
  pub custom_type: Option<String>,
  #[serde(default)]
  pub content: UserContent,
  #[serde(default)]
  pub details: Option<Value>,
  #[serde(default)]
  pub display: Option<bool>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LabelItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub parent_id: Option<String>,
  #[serde(default)]
  pub timestamp: Option<String>,
  #[serde(default)]
  pub target_id: Option<String>,
  #[serde(default)]
  pub label: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub parent_id: Option<String>,
  #[serde(default)]
  pub timestamp: Option<String>,
  #[serde(default)]
  pub name: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LeafItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub parent_id: Option<String>,
  #[serde(default)]
  pub timestamp: Option<String>,
  #[serde(default)]
  pub target_id: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ActiveToolsChangeItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub parent_id: Option<String>,
  #[serde(default)]
  pub timestamp: Option<String>,
  #[serde(default)]
  pub active_tool_names: Vec<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ErrorItem {
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub parent_id: Option<String>,
  #[serde(default)]
  pub timestamp: Option<String>,
  #[serde(default)]
  pub message: Option<String>,
  #[serde(default)]
  pub error: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnknownItem {
  pub native_type: Option<String>,
  pub native: Value,
  pub parse_error: Option<String>,
}

fn decode_session_item(native_type: Option<String>, native: Value) -> PiSessionItem {
  match native_type.as_deref() {
    Some("session") => decode_item(native_type, native, PiSessionItem::Session),
    Some("model_change") => decode_item(native_type, native, PiSessionItem::ModelChange),
    Some("thinking_level_change") => decode_item(native_type, native, PiSessionItem::ThinkingLevelChange),
    Some("usage") => decode_item(native_type, native, PiSessionItem::Usage),
    Some("message") => decode_item(native_type, native, PiSessionItem::Message),
    Some("compaction") => decode_item(native_type, native, PiSessionItem::Compaction),
    Some("branch_summary") => decode_item(native_type, native, PiSessionItem::BranchSummary),
    Some("custom") => decode_item(native_type, native, PiSessionItem::Custom),
    Some("custom_message") => decode_item(native_type, native, PiSessionItem::CustomMessage),
    Some("context_edit") => decode_item(native_type, native, PiSessionItem::ContextEdit),
    Some("label") => decode_item(native_type, native, PiSessionItem::Label),
    Some("session_info") => decode_item(native_type, native, PiSessionItem::SessionInfo),
    Some("leaf") => decode_item(native_type, native, PiSessionItem::Leaf),
    Some("active_tools_change") => decode_item(native_type, native, PiSessionItem::ActiveToolsChange),
    Some("error") => decode_item(native_type, native, PiSessionItem::Error),
    _ => PiSessionItem::Unknown(UnknownItem {
      native_type,
      native,
      parse_error: None,
    }),
  }
}

fn decode_message(native: Value) -> Message {
  let native_type = native.get("role").and_then(Value::as_str).map(str::to_string);
  match native_type.as_deref() {
    Some("system") => decode_nested(native_type, native, Message::System),
    Some("user") => decode_nested(native_type, native, Message::User),
    Some("assistant") => decode_nested(native_type, native, Message::Assistant),
    Some("toolResult") => decode_nested(native_type, native, Message::ToolResult),
    _ => Message::Unknown(UnknownItem {
      native_type,
      native,
      parse_error: None,
    }),
  }
}

fn decode_content_block(native: Value) -> ContentBlock {
  let native_type = native.get("type").and_then(Value::as_str).map(str::to_string);
  match native_type.as_deref() {
    Some("text") => decode_content(native_type, native, ContentBlock::Text),
    Some("thinking") => decode_content(native_type, native, ContentBlock::Thinking),
    Some("toolCall") => decode_content(native_type, native, ContentBlock::ToolCall),
    Some("image") => decode_content(native_type, native, ContentBlock::Image),
    _ => ContentBlock::Unknown(UnknownItem {
      native_type,
      native,
      parse_error: None,
    }),
  }
}

fn decode_item<T>(native_type: Option<String>, native: Value, wrap: impl FnOnce(T) -> PiSessionItem) -> PiSessionItem
where
  T: DeserializeOwned,
{
  match serde_json::from_value(native.clone()) {
    Ok(item) => wrap(item),
    Err(error) => PiSessionItem::Unknown(UnknownItem {
      native_type,
      native,
      parse_error: Some(error.to_string()),
    }),
  }
}

fn decode_nested<T>(native_type: Option<String>, native: Value, wrap: impl FnOnce(T) -> Message) -> Message
where
  T: DeserializeOwned,
{
  match serde_json::from_value(native.clone()) {
    Ok(message) => wrap(message),
    Err(error) => Message::Unknown(UnknownItem {
      native_type,
      native,
      parse_error: Some(error.to_string()),
    }),
  }
}

fn decode_content<T>(native_type: Option<String>, native: Value, wrap: impl FnOnce(T) -> ContentBlock) -> ContentBlock
where
  T: DeserializeOwned,
{
  match serde_json::from_value(native.clone()) {
    Ok(content) => wrap(content),
    Err(error) => ContentBlock::Unknown(UnknownItem {
      native_type,
      native,
      parse_error: Some(error.to_string()),
    }),
  }
}

fn string_field(value: &Value, field: &str) -> Option<String> {
  value.get(field).and_then(Value::as_str).map(str::to_string)
}
