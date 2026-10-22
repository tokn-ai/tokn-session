use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;

/// Native request IDs distinguish a string from an integer with the same text.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(untagged)]
pub enum RpcId {
  String(String),
  Number(i64),
}

impl fmt::Display for RpcId {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::String(value) => formatter.write_str(value),
      Self::Number(value) => write!(formatter, "{value}"),
    }
  }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Request {
  pub id: RpcId,
  pub method: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub params: Option<Value>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

impl Request {
  pub fn new(id: RpcId, method: impl Into<String>, params: Value) -> Self {
    Self {
      id,
      method: method.into(),
      params: Some(params),
      extra: Map::new(),
    }
  }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Notification {
  pub method: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub params: Option<Value>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

impl Notification {
  pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
    Self {
      method: method.into(),
      params,
      extra: Map::new(),
    }
  }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Response {
  pub id: RpcId,
  pub result: Value,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ErrorBody {
  pub code: i64,
  pub message: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub data: Option<Value>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RpcError {
  /// A null/missing ID is accepted for uncorrelated peer errors.
  pub id: Option<RpcId>,
  pub error: ErrorBody,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

/// Lossless envelope classification. Malformed or ambiguous records are unknown.
#[derive(Clone, Debug, PartialEq)]
pub enum RpcMessage {
  Request { request: Request, native: Value },
  Response { response: Response, native: Value },
  Error { error: RpcError, native: Value },
  Notification { notification: Notification, native: Value },
  Unknown(Value),
}

impl RpcMessage {
  pub fn decode(native: Value) -> Self {
    let Some(object) = native.as_object() else {
      return Self::Unknown(native);
    };
    let has_method = object.contains_key("method");
    let has_result = object.contains_key("result");
    let has_error = object.contains_key("error");
    let has_id = object.contains_key("id");
    if usize::from(has_method) + usize::from(has_result) + usize::from(has_error) != 1 {
      return Self::Unknown(native);
    }
    if has_method && has_id {
      return match serde_json::from_value(native.clone()) {
        Ok(request) => Self::Request { request, native },
        Err(_) => Self::Unknown(native),
      };
    }
    if has_method {
      return match serde_json::from_value(native.clone()) {
        Ok(notification) => Self::Notification { notification, native },
        Err(_) => Self::Unknown(native),
      };
    }
    if has_result {
      return match serde_json::from_value(native.clone()) {
        Ok(response) => Self::Response { response, native },
        Err(_) => Self::Unknown(native),
      };
    }
    match serde_json::from_value(native.clone()) {
      Ok(error) => Self::Error { error, native },
      Err(_) => Self::Unknown(native),
    }
  }

  pub fn native(&self) -> &Value {
    match self {
      Self::Request { native, .. }
      | Self::Response { native, .. }
      | Self::Error { native, .. }
      | Self::Notification { native, .. }
      | Self::Unknown(native) => native,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn envelopes_preserve_native_fields_and_distinct_request_ids() {
    assert_ne!(RpcId::String("7".into()), RpcId::Number(7));
    for value in [
      json!({"id":7,"method":"future/request","params":{"unknown":true},"trace":{"future":1}}),
      json!({"id":"7","result":null,"future":3}),
      json!({"method":"future/notification","params":[1,2],"future":4}),
      json!({"id":7,"error":{"code":-32600,"message":"rejected","data":{"reason":"native"},"future":5}}),
    ] {
      let message = RpcMessage::decode(value.clone());
      assert!(!matches!(message, RpcMessage::Unknown(_)));
      assert_eq!(message.native(), &value);
      let encoded = match message {
        RpcMessage::Request { request, .. } => serde_json::to_value(request).unwrap(),
        RpcMessage::Response { response, .. } => serde_json::to_value(response).unwrap(),
        RpcMessage::Error { error, .. } => serde_json::to_value(error).unwrap(),
        RpcMessage::Notification { notification, .. } => serde_json::to_value(notification).unwrap(),
        _ => unreachable!(),
      };
      assert_eq!(encoded, value);
    }
  }

  #[test]
  fn malformed_and_ambiguous_envelopes_do_not_consume_requests() {
    for native in [
      json!(["not an envelope"]),
      json!({"id":1.5,"method":"question"}),
      json!({"id":null,"method":"question"}),
      json!({"id":1,"result":null,"error":{"code":1,"message":"both"}}),
      json!({"id":1,"method":"question","result":null}),
      json!({"result":"uncorrelated"}),
    ] {
      let message = RpcMessage::decode(native.clone());
      assert!(matches!(message, RpcMessage::Unknown(_)));
      assert_eq!(message.native(), &native);
    }
  }
}
