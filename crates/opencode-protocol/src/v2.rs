//! Lossless OpenCode V2 `session_message` projections. SQL supplies `id` and
//! `type`; callers combine them with `data` before decoding this wire value.
use crate::{ExtraFields, UnknownItem, string_field};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct MessageData {
  item: MessageItem,
  native: Value,
}
impl MessageData {
  pub fn item(&self) -> &MessageItem {
    &self.item
  }
  pub fn native(&self) -> &Value {
    &self.native
  }
}
impl Serialize for MessageData {
  fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
    self.native.serialize(serializer)
  }
}
impl<'de> Deserialize<'de> for MessageData {
  fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    let native = Value::deserialize(deserializer)?;
    let kind = string_field(&native, "type");
    let decoded = match kind.as_deref() {
      Some("user") => serde_json::from_value(native.clone()).map(MessageItem::User),
      Some("assistant") => serde_json::from_value(native.clone()).map(MessageItem::Assistant),
      Some("model-switched") => serde_json::from_value(native.clone()).map(MessageItem::ModelSwitched),
      Some("agent-switched") => serde_json::from_value(native.clone()).map(MessageItem::AgentSwitched),
      Some("system" | "synthetic") => serde_json::from_value(native.clone()).map(MessageItem::Text),
      Some("shell") => serde_json::from_value(native.clone()).map(MessageItem::Shell),
      Some("compaction") => serde_json::from_value(native.clone()).map(MessageItem::Compaction),
      _ => {
        return Ok(Self {
          item: MessageItem::Unknown(UnknownItem {
            native_type: kind,
            native: native.clone(),
            parse_error: None,
          }),
          native,
        });
      }
    };
    let item = decoded.unwrap_or_else(|error| {
      MessageItem::Unknown(UnknownItem {
        native_type: kind,
        native: native.clone(),
        parse_error: Some(error.to_string()),
      })
    });
    Ok(Self { item, native })
  }
}

#[derive(Clone, Debug, PartialEq)]
pub enum MessageItem {
  User(User),
  Assistant(Assistant),
  ModelSwitched(ModelSwitched),
  AgentSwitched(AgentSwitched),
  Text(TextMessage),
  Shell(Shell),
  Compaction(Compaction),
  Unknown(UnknownItem),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileAttachment {
  pub uri: String,
  pub mime: String,
  #[serde(default)]
  pub name: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Time {
  pub created: i64,
  #[serde(default)]
  pub completed: Option<i64>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelRef {
  pub id: String,
  #[serde(rename = "providerID")]
  pub provider_id: String,
  #[serde(default)]
  pub variant: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct User {
  pub id: String,
  pub text: String,
  pub time: Time,
  #[serde(default)]
  pub files: Vec<FileAttachment>,
  #[serde(default)]
  pub agents: Vec<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Assistant {
  pub id: String,
  pub agent: String,
  pub model: ModelRef,
  pub time: Time,
  pub content: Vec<ContentData>,
  #[serde(default)]
  pub finish: Option<String>,
  #[serde(default)]
  pub tokens: Option<Value>,
  #[serde(default)]
  pub error: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelSwitched {
  pub id: String,
  pub model: ModelRef,
  pub time: Time,
  #[serde(flatten)]
  pub extra: ExtraFields,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentSwitched {
  pub id: String,
  pub agent: String,
  pub time: Time,
  #[serde(flatten)]
  pub extra: ExtraFields,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextMessage {
  pub id: String,
  pub text: String,
  pub time: Time,
  #[serde(flatten)]
  pub extra: ExtraFields,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Shell {
  pub id: String,
  #[serde(rename = "callID")]
  pub call_id: String,
  pub command: String,
  pub output: String,
  pub time: Time,
  #[serde(flatten)]
  pub extra: ExtraFields,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Compaction {
  pub id: String,
  pub reason: String,
  pub summary: String,
  pub recent: String,
  pub time: Time,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContentData {
  item: ContentItem,
  native: Value,
}
impl ContentData {
  pub fn item(&self) -> &ContentItem {
    &self.item
  }
  pub fn native(&self) -> &Value {
    &self.native
  }
}
impl Serialize for ContentData {
  fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
    self.native.serialize(serializer)
  }
}
impl<'de> Deserialize<'de> for ContentData {
  fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    let native = Value::deserialize(deserializer)?;
    let kind = string_field(&native, "type");
    let decoded = match kind.as_deref() {
      Some("text" | "reasoning") => serde_json::from_value(native.clone()).map(ContentItem::Text),
      Some("tool") => serde_json::from_value(native.clone()).map(ContentItem::Tool),
      _ => {
        return Ok(Self {
          item: ContentItem::Unknown(UnknownItem {
            native_type: kind,
            native: native.clone(),
            parse_error: None,
          }),
          native,
        });
      }
    };
    let item = decoded.unwrap_or_else(|error| {
      ContentItem::Unknown(UnknownItem {
        native_type: kind,
        native: native.clone(),
        parse_error: Some(error.to_string()),
      })
    });
    Ok(Self { item, native })
  }
}
#[derive(Clone, Debug, PartialEq)]
pub enum ContentItem {
  Text(TextContent),
  Tool(ToolContent),
  Unknown(UnknownItem),
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextContent {
  pub id: String,
  pub text: String,
  #[serde(flatten)]
  pub extra: ExtraFields,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolContent {
  pub id: String,
  pub name: String,
  pub state: Value,
  pub time: Time,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;
  #[test]
  fn unfamiliar_and_malformed_values_remain_lossless() {
    for native in [json!({"type":"future","opaque":[1]}), json!({"type":"user","text":7})] {
      let message: MessageData = serde_json::from_value(native.clone()).unwrap();
      assert!(matches!(message.item(), MessageItem::Unknown(_)));
      assert_eq!(serde_json::to_value(message).unwrap(), native);
    }
  }
}
