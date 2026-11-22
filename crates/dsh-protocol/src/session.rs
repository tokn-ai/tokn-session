use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

pub type ExtraFields = Map<String, Value>;

/// One decoded logical record from a persisted DSH session log.
///
/// The native value is retained independently of the typed view. Serializing
/// this type therefore reproduces every decoded JSON field, including fields
/// and record kinds this crate does not understand.
#[derive(Debug, Clone, PartialEq)]
pub struct DshSessionLine {
  item: DshSessionItem,
  native: Value,
}

impl DshSessionLine {
  pub fn item(&self) -> &DshSessionItem {
    &self.item
  }

  pub fn into_item(self) -> DshSessionItem {
    self.item
  }

  pub fn native(&self) -> &Value {
    &self.native
  }

  pub fn into_native(self) -> Value {
    self.native
  }

  pub fn into_parts(self) -> (DshSessionItem, Value) {
    (self.item, self.native)
  }
}

impl<'de> Deserialize<'de> for DshSessionLine {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let native = Value::deserialize(deserializer)?;
    let native_type = string_field(&native, "type");
    let item = decode_session_item(native_type, native.clone());
    Ok(Self { item, native })
  }
}

impl Serialize for DshSessionLine {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    self.native.serialize(serializer)
  }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DshSessionItem {
  Session(SessionHeader),
  Event(SessionEvent),
  TextChunks(TextChunksRow),
  ReasoningChunks(TextChunksRow),
  ToolCallChunks(ToolCallChunksRow),
  Unknown(UnknownItem),
}

impl DshSessionItem {
  pub fn native_type(&self) -> Option<&str> {
    match self {
      Self::Session(_) => Some("session"),
      Self::Event(event) => Some(event.native_type()),
      Self::TextChunks(_) => Some("text-chunks"),
      Self::ReasoningChunks(_) => Some("reasoning-chunks"),
      Self::ToolCallChunks(_) => Some("tool-call-chunks"),
      Self::Unknown(item) => item.native_type.as_deref(),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
  #[serde(rename = "type")]
  pub native_type: String,
  pub version: u64,
  pub id: String,
  pub created_at: u64,
  #[serde(default)]
  pub cwd: Option<String>,
  #[serde(default)]
  pub parent_session: Option<String>,
  #[serde(default)]
  pub seed_length: Option<u64>,
  #[serde(default)]
  pub is_seeded: Option<bool>,
  #[serde(default)]
  pub origin: Option<String>,
  #[serde(default)]
  pub delegation_depth: u64,
  #[serde(default)]
  pub agent_preset: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
  TurnStart(EventRecord<TurnStartData>),
  TurnEnd(EventRecord<TurnEndData>),
  StepStart(EventRecord<StepData>),
  StepEnd(EventRecord<StepData>),
  UserMessage(EventRecord<UserMessage>),
  SystemMessage(EventRecord<PromptMessageData>),
  DeveloperMessage(EventRecord<PromptMessageData>),
  AssistantChunk(EventRecord<AssistantChunkData>),
  AssistantMessage(EventRecord<AssistantMessageData>),
  AssistantAttempt(EventRecord<AssistantAttemptData>),
  ToolCall(EventRecord<ToolCallData>),
  ToolResult(EventRecord<ToolResultData>),
  TodoWrite(EventRecord<TodoWriteData>),
  RequestHeader(EventRecord<RequestHeaderData>),
  RequestContext(EventRecord<RequestContextData>),
  SessionEndSeed(EventRecord<EmptyData>),
}

impl SessionEvent {
  pub fn native_type(&self) -> &str {
    match self {
      Self::TurnStart(_) => "turn/start",
      Self::TurnEnd(_) => "turn/end",
      Self::StepStart(_) => "step/start",
      Self::StepEnd(_) => "step/end",
      Self::UserMessage(_) => "user/message",
      Self::SystemMessage(_) => "system/message",
      Self::DeveloperMessage(_) => "developer/message",
      Self::AssistantChunk(_) => "assistant/chunk",
      Self::AssistantMessage(_) => "assistant/message",
      Self::AssistantAttempt(_) => "assistant/attempt",
      Self::ToolCall(_) => "tool/call",
      Self::ToolResult(_) => "tool/result",
      Self::TodoWrite(_) => "todo/write",
      Self::RequestHeader(_) => "request/header",
      Self::RequestContext(_) => "request/context",
      Self::SessionEndSeed(_) => "session/end-seed",
    }
  }

  pub fn seq(&self) -> u64 {
    match self {
      Self::TurnStart(item) => item.seq,
      Self::TurnEnd(item) => item.seq,
      Self::StepStart(item) => item.seq,
      Self::StepEnd(item) => item.seq,
      Self::UserMessage(item) => item.seq,
      Self::SystemMessage(item) | Self::DeveloperMessage(item) => item.seq,
      Self::AssistantChunk(item) => item.seq,
      Self::AssistantMessage(item) => item.seq,
      Self::AssistantAttempt(item) => item.seq,
      Self::ToolCall(item) => item.seq,
      Self::ToolResult(item) => item.seq,
      Self::TodoWrite(item) => item.seq,
      Self::RequestHeader(item) => item.seq,
      Self::RequestContext(item) => item.seq,
      Self::SessionEndSeed(item) => item.seq,
    }
  }

  pub fn time(&self) -> i64 {
    match self {
      Self::TurnStart(item) => item.time,
      Self::TurnEnd(item) => item.time,
      Self::StepStart(item) => item.time,
      Self::StepEnd(item) => item.time,
      Self::UserMessage(item) => item.time,
      Self::SystemMessage(item) | Self::DeveloperMessage(item) => item.time,
      Self::AssistantChunk(item) => item.time,
      Self::AssistantMessage(item) => item.time,
      Self::AssistantAttempt(item) => item.time,
      Self::ToolCall(item) => item.time,
      Self::ToolResult(item) => item.time,
      Self::TodoWrite(item) => item.time,
      Self::RequestHeader(item) => item.time,
      Self::RequestContext(item) => item.time,
      Self::SessionEndSeed(item) => item.time,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRecord<T> {
  #[serde(rename = "type")]
  pub native_type: String,
  pub seq: u64,
  pub time: i64,
  pub data: T,
  #[serde(default)]
  pub ignorable: Option<bool>,
  #[serde(default)]
  pub source_event_seqs: Option<Vec<u64>>,
  #[serde(default)]
  pub surface_op: Option<SurfaceOp>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceOp {
  Append,
  Replace(SurfaceReplace),
  Unknown(Value),
}

impl<'de> Deserialize<'de> for SurfaceOp {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let native = Value::deserialize(deserializer)?;
    Ok(match &native {
      Value::String(value) if value == "append" => Self::Append,
      Value::Object(value) if value.get("op").and_then(Value::as_str) == Some("replace") => {
        match serde_json::from_value(native.clone()) {
          Ok(value) => Self::Replace(value),
          Err(_) => Self::Unknown(native),
        }
      }
      _ => Self::Unknown(native),
    })
  }
}

impl Serialize for SurfaceOp {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    match self {
      Self::Append => "append".serialize(serializer),
      Self::Replace(value) => value.serialize(serializer),
      Self::Unknown(value) => value.serialize(serializer),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurfaceReplace {
  pub op: String,
  #[serde(rename = "startSeq", alias = "start")]
  pub start: u64,
  #[serde(rename = "endSeq", alias = "end")]
  pub end: u64,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnStartData {
  pub turn: u64,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnEndData {
  pub turn: u64,
  pub reason: TaggedValue,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepData {
  pub turn: u64,
  pub step: u64,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantChunkData {
  pub turn: u64,
  pub step: u64,
  pub chunk: StreamChunk,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessageData {
  pub turn: u64,
  pub step: u64,
  pub message: AssistantMessage,
  #[serde(default)]
  pub usage: Option<TokenUsage>,
  /// Compact, timed native stream; unknown records stay inspectable.
  #[serde(default)]
  pub stream: Vec<Value>,
  #[serde(default)]
  pub interrupted: Option<bool>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptMessageData {
  pub turn: u64,
  pub step: u64,
  pub message: UserMessage,
  #[serde(default)]
  pub header_seq: Option<u64>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantAttemptData {
  pub turn: u64,
  pub step: u64,
  pub stream: Vec<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallData {
  pub turn: u64,
  pub step: u64,
  pub call_id: String,
  pub name: String,
  pub arguments: String,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultData {
  pub turn: u64,
  pub step: u64,
  pub message: ToolResultMessage,
  #[serde(default)]
  pub error: Option<ToolError>,
  #[serde(default)]
  pub meta: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolError {
  pub name: String,
  pub code: String,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoWriteData {
  pub todos: Vec<TodoItem>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
  pub content: String,
  pub status: String,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestHeaderData {
  pub header: EpochHeader,
  pub reason: String,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpochHeader {
  pub config: LlmCallConfig,
  #[serde(default)]
  pub adapter_defaults: Option<Value>,
  #[serde(default)]
  pub system: Option<String>,
  #[serde(default)]
  pub tools: Option<Vec<ToolSchema>>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmCallConfig {
  pub provider: String,
  pub model: String,
  #[serde(default)]
  pub reasoning_effort: Option<String>,
  #[serde(default)]
  pub temperature: Option<f64>,
  #[serde(default)]
  pub max_tokens: Option<u64>,
  #[serde(default)]
  pub stop: Option<Vec<String>>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
  pub name: String,
  pub description: String,
  pub parameters: Value,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestContextData {
  pub provider: String,
  pub model: String,
  #[serde(default)]
  pub context_window: Option<u64>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

pub type EmptyData = ExtraFields;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
  pub id: String,
  pub role: String,
  pub content: Vec<ContentBlock>,
  pub source: TaggedValue,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
  pub id: String,
  pub role: String,
  pub content: Vec<ContentBlock>,
  pub source: TaggedValue,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultMessage {
  pub id: String,
  pub role: String,
  pub content: Vec<ContentBlock>,
  pub source: TaggedValue,
  #[serde(default, rename = "toolCallId")]
  pub tool_call_id: Option<String>,
  #[serde(default, rename = "isError")]
  pub is_error: Option<bool>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
  Text(TextBlock),
  Reasoning(TextBlock),
  Image(ImageBlock),
  File(FileBlock),
  ToolAddition(ToolChangeBlock),
  ToolRemoval(ToolChangeBlock),
  ToolCall(ToolCallBlock),
  ToolResult(ToolResultBlock),
  Unknown(UnknownItem),
}

impl<'de> Deserialize<'de> for ContentBlock {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let native = Value::deserialize(deserializer)?;
    let native_type = string_field(&native, "type");
    Ok(match native_type.as_deref() {
      Some("text") => decode_nested(native_type, native, Self::Text),
      Some("reasoning") => decode_nested(native_type, native, Self::Reasoning),
      Some("image") => decode_nested(native_type, native, Self::Image),
      Some("file") => decode_nested(native_type, native, Self::File),
      Some("tool-addition") => decode_nested(native_type, native, Self::ToolAddition),
      Some("tool-removal") => decode_nested(native_type, native, Self::ToolRemoval),
      Some("tool-call") => decode_nested(native_type, native, Self::ToolCall),
      Some("tool-result") => decode_nested(native_type, native, Self::ToolResult),
      _ => Self::Unknown(UnknownItem::new(native_type, native, None)),
    })
  }
}

impl Serialize for ContentBlock {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    match self {
      Self::Text(value) => value.serialize(serializer),
      Self::Reasoning(value) => value.serialize(serializer),
      Self::Image(value) => value.serialize(serializer),
      Self::File(value) => value.serialize(serializer),
      Self::ToolAddition(value) | Self::ToolRemoval(value) => value.serialize(serializer),
      Self::ToolCall(value) => value.serialize(serializer),
      Self::ToolResult(value) => value.serialize(serializer),
      Self::Unknown(value) => value.native.serialize(serializer),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextBlock {
  #[serde(rename = "type")]
  pub native_type: String,
  pub text: String,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageBlock {
  #[serde(rename = "type")]
  pub native_type: String,
  pub attachment: Value,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileBlock {
  #[serde(rename = "type")]
  pub native_type: String,
  pub attachment: Value,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolChangeBlock {
  #[serde(rename = "type")]
  pub native_type: String,
  pub tool_name: String,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallBlock {
  #[serde(rename = "type")]
  pub native_type: String,
  pub id: String,
  pub name: String,
  pub arguments: String,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultBlock {
  #[serde(rename = "type")]
  pub native_type: String,
  pub tool_call_id: String,
  pub content: Vec<ContentBlock>,
  #[serde(default)]
  pub is_error: Option<bool>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamChunk {
  BlockStart(BlockStartChunk),
  TextDelta(TextDeltaChunk),
  ReasoningDelta(TextDeltaChunk),
  ToolCallDelta(ToolCallDeltaChunk),
  BlockEnd(BlockEndChunk),
  Usage(UsageChunk),
  Finish(FinishChunk),
  Unknown(UnknownItem),
}

impl<'de> Deserialize<'de> for StreamChunk {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let native = Value::deserialize(deserializer)?;
    let native_type = string_field(&native, "type");
    Ok(match native_type.as_deref() {
      Some("block-start") => decode_stream(native_type, native, Self::BlockStart),
      Some("text-delta") => decode_stream(native_type, native, Self::TextDelta),
      Some("reasoning-delta") => decode_stream(native_type, native, Self::ReasoningDelta),
      Some("tool-call-delta") => decode_stream(native_type, native, Self::ToolCallDelta),
      Some("block-end") => decode_stream(native_type, native, Self::BlockEnd),
      Some("usage") => decode_stream(native_type, native, Self::Usage),
      Some("finish") => decode_stream(native_type, native, Self::Finish),
      _ => Self::Unknown(UnknownItem::new(native_type, native, None)),
    })
  }
}

impl Serialize for StreamChunk {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    match self {
      Self::BlockStart(value) => value.serialize(serializer),
      Self::TextDelta(value) => value.serialize(serializer),
      Self::ReasoningDelta(value) => value.serialize(serializer),
      Self::ToolCallDelta(value) => value.serialize(serializer),
      Self::BlockEnd(value) => value.serialize(serializer),
      Self::Usage(value) => value.serialize(serializer),
      Self::Finish(value) => value.serialize(serializer),
      Self::Unknown(value) => value.native.serialize(serializer),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockStartChunk {
  #[serde(rename = "type")]
  pub native_type: String,
  pub index: u64,
  pub block_type: String,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextDeltaChunk {
  #[serde(rename = "type")]
  pub native_type: String,
  pub index: u64,
  pub text: String,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallDeltaChunk {
  #[serde(rename = "type")]
  pub native_type: String,
  pub index: u64,
  pub id: String,
  #[serde(default)]
  pub name: Option<String>,
  pub arguments_delta: String,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockEndChunk {
  #[serde(rename = "type")]
  pub native_type: String,
  pub index: u64,
  pub block: ContentBlock,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageChunk {
  #[serde(rename = "type")]
  pub native_type: String,
  pub usage: TokenUsage,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishChunk {
  #[serde(rename = "type")]
  pub native_type: String,
  pub reason: TaggedValue,
  #[serde(default)]
  pub replay_state: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
  pub input_tokens: u64,
  pub output_tokens: u64,
  #[serde(default)]
  pub cache_read_tokens: Option<u64>,
  #[serde(default)]
  pub cache_write_tokens: Option<u64>,
  #[serde(default)]
  pub reasoning_tokens: Option<u64>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

/// A merge-extensible object discriminated by `kind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaggedValue {
  pub kind: String,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextChunksRow {
  #[serde(rename = "type")]
  pub native_type: String,
  pub seq0: u64,
  pub time0: i64,
  pub data: TextChunksData,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextChunksData {
  pub turn: u64,
  pub step: u64,
  pub index: u64,
  pub dt: Vec<i64>,
  pub texts: Vec<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallChunksRow {
  #[serde(rename = "type")]
  pub native_type: String,
  pub seq0: u64,
  pub time0: i64,
  pub data: ToolCallChunksData,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallChunksData {
  pub turn: u64,
  pub step: u64,
  pub index: u64,
  pub id: String,
  #[serde(default)]
  pub name: Option<String>,
  pub dt: Vec<i64>,
  pub args: Vec<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnknownItem {
  pub native_type: Option<String>,
  pub native: Value,
  pub parse_error: Option<String>,
}

impl UnknownItem {
  fn new(native_type: Option<String>, native: Value, parse_error: Option<String>) -> Self {
    Self {
      native_type,
      native,
      parse_error,
    }
  }
}

fn decode_session_item(native_type: Option<String>, native: Value) -> DshSessionItem {
  match native_type.as_deref() {
    Some("session") => decode_item(native_type, native, DshSessionItem::Session),
    Some("text-chunks") => decode_item(native_type, native, DshSessionItem::TextChunks),
    Some("reasoning-chunks") => decode_item(native_type, native, DshSessionItem::ReasoningChunks),
    Some("tool-call-chunks") => decode_item(native_type, native, DshSessionItem::ToolCallChunks),
    Some(event_type) if is_core_event(event_type) => decode_event(native_type, native),
    _ => DshSessionItem::Unknown(UnknownItem::new(native_type, native, None)),
  }
}

fn decode_event(native_type: Option<String>, native: Value) -> DshSessionItem {
  let event = match native_type.as_deref() {
    Some("turn/start") => decode_event_record(native_type, native, SessionEvent::TurnStart),
    Some("turn/end") => decode_event_record(native_type, native, SessionEvent::TurnEnd),
    Some("step/start") => decode_event_record(native_type, native, SessionEvent::StepStart),
    Some("step/end") => decode_event_record(native_type, native, SessionEvent::StepEnd),
    Some("user/message") => decode_event_record(native_type, native, SessionEvent::UserMessage),
    Some("system/message") => decode_event_record(native_type, native, SessionEvent::SystemMessage),
    Some("developer/message") => decode_event_record(native_type, native, SessionEvent::DeveloperMessage),
    Some("assistant/chunk") => decode_event_record(native_type, native, SessionEvent::AssistantChunk),
    Some("assistant/message") => decode_event_record(native_type, native, SessionEvent::AssistantMessage),
    Some("assistant/attempt") => decode_event_record(native_type, native, SessionEvent::AssistantAttempt),
    Some("tool/call") => decode_event_record(native_type, native, SessionEvent::ToolCall),
    Some("tool/result") => decode_event_record(native_type, native, SessionEvent::ToolResult),
    Some("todo/write") => decode_event_record(native_type, native, SessionEvent::TodoWrite),
    Some("request/header") => decode_event_record(native_type, native, SessionEvent::RequestHeader),
    Some("request/context") => decode_event_record(native_type, native, SessionEvent::RequestContext),
    Some("session/end-seed") => decode_event_record(native_type, native, SessionEvent::SessionEndSeed),
    _ => unreachable!("decode_event is only called for modeled core events"),
  };
  event.map_or_else(DshSessionItem::Unknown, DshSessionItem::Event)
}

fn is_core_event(native_type: &str) -> bool {
  matches!(
    native_type,
    "turn/start"
      | "turn/end"
      | "step/start"
      | "step/end"
      | "user/message"
      | "system/message"
      | "developer/message"
      | "assistant/chunk"
      | "assistant/message"
      | "assistant/attempt"
      | "tool/call"
      | "tool/result"
      | "todo/write"
      | "request/header"
      | "request/context"
      | "session/end-seed"
  )
}

fn decode_item<T>(native_type: Option<String>, native: Value, wrap: impl FnOnce(T) -> DshSessionItem) -> DshSessionItem
where
  T: DeserializeOwned,
{
  match serde_json::from_value(native.clone()) {
    Ok(item) => wrap(item),
    Err(error) => DshSessionItem::Unknown(UnknownItem::new(native_type, native, Some(error.to_string()))),
  }
}

fn decode_event_record<T>(
  native_type: Option<String>,
  native: Value,
  wrap: impl FnOnce(EventRecord<T>) -> SessionEvent,
) -> Result<SessionEvent, UnknownItem>
where
  T: DeserializeOwned,
{
  match serde_json::from_value(native.clone()) {
    Ok(item) => Ok(wrap(item)),
    Err(error) => Err(UnknownItem::new(native_type, native, Some(error.to_string()))),
  }
}

fn decode_nested<T>(native_type: Option<String>, native: Value, wrap: impl FnOnce(T) -> ContentBlock) -> ContentBlock
where
  T: DeserializeOwned,
{
  match serde_json::from_value(native.clone()) {
    Ok(item) => wrap(item),
    Err(error) => ContentBlock::Unknown(UnknownItem::new(native_type, native, Some(error.to_string()))),
  }
}

fn decode_stream<T>(native_type: Option<String>, native: Value, wrap: impl FnOnce(T) -> StreamChunk) -> StreamChunk
where
  T: DeserializeOwned,
{
  match serde_json::from_value(native.clone()) {
    Ok(item) => wrap(item),
    Err(error) => StreamChunk::Unknown(UnknownItem::new(native_type, native, Some(error.to_string()))),
  }
}

fn string_field(value: &Value, field: &str) -> Option<String> {
  value.get(field).and_then(Value::as_str).map(str::to_string)
}
