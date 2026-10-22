use crate::RpcId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const INITIALIZE: &str = "initialize";
pub const INITIALIZED: &str = "initialized";
pub const MODEL_LIST: &str = "model/list";
pub const THREAD_START: &str = "thread/start";
pub const THREAD_RESUME: &str = "thread/resume";
pub const THREAD_READ: &str = "thread/read";
pub const TURN_START: &str = "turn/start";
pub const TURN_STEER: &str = "turn/steer";
pub const TURN_INTERRUPT: &str = "turn/interrupt";
pub const TOOL_REQUEST_USER_INPUT: &str = "item/tool/requestUserInput";
pub const COMMAND_EXECUTION_REQUEST_APPROVAL: &str = "item/commandExecution/requestApproval";
pub const FILE_CHANGE_REQUEST_APPROVAL: &str = "item/fileChange/requestApproval";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
  pub name: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub title: Option<String>,
  pub version: String,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeCapabilities {
  #[serde(default)]
  pub experimental_api: bool,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub opt_out_notification_methods: Option<Vec<String>>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
  pub client_info: ClientInfo,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub capabilities: Option<InitializeCapabilities>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
  pub user_agent: String,
  #[serde(default)]
  pub codex_home: Option<String>,
  #[serde(default)]
  pub platform_family: Option<String>,
  #[serde(default)]
  pub platform_os: Option<String>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelListParams {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub cursor: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub limit: Option<u32>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub include_hidden: Option<bool>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
  pub id: String,
  pub model: String,
  pub display_name: String,
  #[serde(default)]
  pub description: String,
  #[serde(default)]
  pub hidden: bool,
  /// Legacy peers omitted this field and native Codex defaults to text + image.
  /// An explicit null remains unknown instead of inventing a capability.
  #[serde(default = "legacy_input_modalities")]
  pub input_modalities: Option<Vec<String>>,
  #[serde(default)]
  pub supported_reasoning_efforts: Vec<Value>,
  #[serde(default)]
  pub default_reasoning_effort: Option<String>,
  #[serde(default)]
  pub is_default: bool,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelListResponse {
  pub data: Vec<Model>,
  #[serde(default)]
  pub next_cursor: Option<String>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

fn legacy_input_modalities() -> Option<Vec<String>> {
  Some(vec!["text".into(), "image".into()])
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartParams {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub model: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub model_provider: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub cwd: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub approval_policy: Option<Value>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub sandbox: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub config: Option<BTreeMap<String, Value>>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub ephemeral: Option<bool>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeParams {
  pub thread_id: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub model: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub model_provider: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub cwd: Option<String>,
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  pub exclude_turns: bool,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadReadParams {
  pub thread_id: String,
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  pub include_turns: bool,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

/// Thread status and item bodies remain native JSON so new states stay visible.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
  pub id: String,
  pub status: Value,
  #[serde(default)]
  pub model: Option<String>,
  #[serde(default)]
  pub model_provider: Option<String>,
  #[serde(default)]
  pub name: Option<String>,
  #[serde(default)]
  pub cwd: Option<String>,
  #[serde(default)]
  pub path: Option<String>,
  #[serde(default)]
  pub turns: Vec<Turn>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartResponse {
  pub thread: Thread,
  pub model: String,
  pub model_provider: String,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

pub type ThreadResumeResponse = ThreadStartResponse;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadReadResponse {
  pub thread: Thread,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

/// Text/image are represented by their native tagged object without closing the
/// enum to future input kinds. Native semantic validation belongs to the server.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInput {
  #[serde(rename = "type")]
  pub kind: String,
  #[serde(flatten)]
  pub fields: Map<String, Value>,
}

impl UserInput {
  pub fn text(text: impl Into<String>) -> Self {
    Self {
      kind: "text".into(),
      fields: Map::from_iter([
        ("text".into(), Value::String(text.into())),
        ("text_elements".into(), Value::Array(Vec::new())),
      ]),
    }
  }

  pub fn image(url: impl Into<String>) -> Self {
    Self {
      kind: "image".into(),
      fields: Map::from_iter([("url".into(), Value::String(url.into()))]),
    }
  }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
  pub thread_id: String,
  pub input: Vec<UserInput>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub client_user_message_id: Option<String>,
  /// Native turn/start may steer an active turn; model overrides can also alter
  /// thread settings when steering. This is not model-on-idle-only admission.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub model: Option<String>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSteerParams {
  pub thread_id: String,
  pub input: Vec<UserInput>,
  pub expected_turn_id: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub client_user_message_id: Option<String>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptParams {
  pub thread_id: String,
  pub turn_id: String,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
  pub id: String,
  pub status: String,
  #[serde(default)]
  pub items: Vec<Value>,
  #[serde(default)]
  pub error: Option<Value>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartResponse {
  pub turn: Turn,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSteerResponse {
  pub turn_id: String,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnNotification {
  pub thread_id: String,
  pub turn: Turn,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerRequestResolvedNotification {
  pub thread_id: String,
  pub request_id: RpcId,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRequestUserInputOption {
  pub label: String,
  #[serde(default)]
  pub description: String,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRequestUserInputQuestion {
  pub id: String,
  pub header: String,
  pub question: String,
  #[serde(default)]
  pub is_other: bool,
  #[serde(default)]
  pub is_secret: bool,
  #[serde(default)]
  pub options: Option<Vec<ToolRequestUserInputOption>>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

fn blocking_by_default() -> bool {
  true
}

fn deserialize_blocking<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
  Ok(Option::<bool>::deserialize(deserializer)?.unwrap_or(true))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRequestUserInputParams {
  pub thread_id: String,
  pub turn_id: String,
  pub item_id: String,
  pub questions: Vec<ToolRequestUserInputQuestion>,
  /// Both blocking and asynchronous native questions share this server method.
  /// Older app-server versions omitted this field and always blocked.
  #[serde(default = "blocking_by_default", deserialize_with = "deserialize_blocking")]
  pub is_blocking: bool,
  #[serde(default)]
  pub auto_resolution_ms: Option<u64>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolRequestUserInputAnswer {
  pub answers: Vec<String>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ToolRequestUserInputResponse {
  pub answers: BTreeMap<String, ToolRequestUserInputAnswer>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecutionRequestApprovalParams {
  pub thread_id: String,
  pub turn_id: String,
  pub item_id: String,
  #[serde(default)]
  pub approval_id: Option<String>,
  #[serde(default)]
  pub command: Option<String>,
  #[serde(default)]
  pub cwd: Option<String>,
  #[serde(default)]
  pub reason: Option<String>,
  /// Native decisions can contain policy-amendment objects. Retain their exact
  /// shape; clients must allowlist rather than silently widening approval scope.
  #[serde(default)]
  pub available_decisions: Option<Vec<Value>>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeRequestApprovalParams {
  pub thread_id: String,
  pub turn_id: String,
  pub item_id: String,
  #[serde(default)]
  pub reason: Option<String>,
  #[serde(default)]
  pub grant_root: Option<String>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ApprovalResponse {
  pub decision: Value,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn inputs_follow_native_field_exceptions() {
    let text = serde_json::to_value(UserInput::text("hello")).unwrap();
    assert_eq!(text, json!({"type":"text","text":"hello","text_elements":[]}));
    let image = serde_json::to_value(UserInput::image("data:image/png;base64,AA==")).unwrap();
    assert_eq!(image, json!({"type":"image","url":"data:image/png;base64,AA=="}));
    let request = serde_json::to_value(TurnSteerParams {
      thread_id: "thread".into(),
      expected_turn_id: "turn".into(),
      input: vec![UserInput::text("hello")],
      ..Default::default()
    })
    .unwrap();
    assert_eq!(request["threadId"], "thread");
    assert_eq!(request["expectedTurnId"], "turn");
    assert!(request.get("model").is_none());
  }

  #[test]
  fn questions_preserve_async_and_secret_semantics() {
    let native = json!({"threadId":"t","turnId":"r","itemId":"i","isBlocking":false,
      "questions":[{"id":"q","header":"Name","question":"What name?","isOther":true,"isSecret":true,
        "options":[{"label":"First","description":"one","future":1}],"future":2}],"future":3});
    let question: ToolRequestUserInputParams = serde_json::from_value(native).unwrap();
    assert!(!question.is_blocking);
    assert!(question.questions[0].is_other);
    assert!(question.questions[0].is_secret);
    assert_eq!(question.extra["future"], 3);
    assert_eq!(question.questions[0].options.as_ref().unwrap()[0].extra["future"], 1);
    let legacy: ToolRequestUserInputParams = serde_json::from_value(json!({
      "threadId":"t","turnId":"r","itemId":"i","questions":[]
    }))
    .unwrap();
    assert!(legacy.is_blocking);
    let legacy_null: ToolRequestUserInputParams = serde_json::from_value(json!({
      "threadId":"t","turnId":"r","itemId":"i","questions":[],"isBlocking":null
    }))
    .unwrap();
    assert!(legacy_null.is_blocking);
  }

  #[test]
  fn catalog_thread_and_approval_keep_volatile_fields() {
    let models: ModelListResponse = serde_json::from_value(json!({"data":[{
      "id":"catalog-id","model":"native-model","displayName":"Native","inputModalities":["text","image"],
      "supportedReasoningEfforts":[{"reasoningEffort":"ultra","future":1}],"futureModel":true
    }],"nextCursor":"page-2","futurePage":true}))
    .unwrap();
    assert_eq!(models.data[0].model, "native-model");
    assert_eq!(models.data[0].input_modalities.as_ref().unwrap(), &["text", "image"]);
    assert_eq!(models.data[0].extra["futureModel"], true);
    let thread: ThreadReadResponse = serde_json::from_value(json!({"thread":{
      "id":"t","model":"m","modelProvider":"local","status":{"type":"future","future":1},
      "turns":[{"id":"r","status":"future","items":[{"type":"futureTool"}],"future":2}]
    }}))
    .unwrap();
    assert_eq!(thread.thread.status["future"], 1);
    assert_eq!(thread.thread.turns[0].extra["future"], 2);
    let approval: CommandExecutionRequestApprovalParams = serde_json::from_value(json!({
      "threadId":"t","turnId":"r","itemId":"i","approvalId":"callback",
      "availableDecisions":["decline",{"acceptWithExecpolicyAmendment":{"execpolicyAmendment":["safe"]}}],
      "kind":"stdin","future":true
    }))
    .unwrap();
    assert_eq!(approval.extra["kind"], "stdin");
    assert_eq!(approval.available_decisions.unwrap()[0], "decline");
  }

  #[test]
  fn legacy_model_modalities_match_native_defaults_without_widening_explicit_lists() {
    let minimal = json!({"id":"id","model":"model","displayName":"Model"});
    let legacy: Model = serde_json::from_value(minimal.clone()).unwrap();
    assert_eq!(legacy.input_modalities, Some(vec!["text".into(), "image".into()]));
    let mut explicit = minimal.clone();
    explicit["inputModalities"] = json!(["text"]);
    let text_only: Model = serde_json::from_value(explicit).unwrap();
    assert_eq!(text_only.input_modalities, Some(vec!["text".into()]));
    let mut unknown = minimal;
    unknown["inputModalities"] = Value::Null;
    let unknown: Model = serde_json::from_value(unknown).unwrap();
    assert_eq!(unknown.input_modalities, None);
  }
}
