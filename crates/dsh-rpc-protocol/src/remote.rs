//! Gateway Remote mux/control contracts, independent of the legacy ApiProxy.
//! Unknown provider fields remain available to native event consumers.
use crate::api::ExtraFields;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenStream<'a> {
  #[serde(rename = "type")]
  pub native_type: &'static str,
  pub stream_id: &'a str,
  pub endpoint: &'a str,
  pub payload: Value,
}

impl<'a> OpenStream<'a> {
  pub fn new(stream_id: &'a str, endpoint: &'a str) -> Self {
    Self {
      native_type: "open",
      stream_id,
      endpoint,
      payload: serde_json::json!({"args":{}}),
    }
  }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamFrame {
  #[serde(rename = "type")]
  pub native_type: String,
  pub stream_id: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub value: Option<Value>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub error: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionBaseline {
  /// Inclusive watermark, including -1 before any durable event exists.
  pub as_of_seq: i64,
  pub values: ExtraFields,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ControlBaseline {
  pub projections: BTreeMap<String, ProjectionBaseline>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ControlFrame {
  Baseline {
    value: ControlBaseline,
    #[serde(flatten)]
    extra: ExtraFields,
  },
  Projection {
    #[serde(rename = "sessionId")]
    session_id: String,
    key: String,
    value: Value,
    seq: i64,
    #[serde(flatten)]
    extra: ExtraFields,
  },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum EventFrame {
  Ready {
    #[serde(rename = "clientId")]
    client_id: String,
    host: Value,
    #[serde(flatten)]
    extra: ExtraFields,
  },
  Emit {
    event: String,
    args: Vec<Value>,
    #[serde(flatten)]
    extra: ExtraFields,
  },
  Waterfall {
    event: String,
    #[serde(rename = "eventId")]
    event_id: String,
    #[serde(rename = "agentId")]
    agent_id: String,
    request: ExtraFields,
    #[serde(flatten)]
    extra: ExtraFields,
  },
  Cancel {
    #[serde(rename = "eventId")]
    event_id: String,
    #[serde(flatten)]
    extra: ExtraFields,
  },
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn opens_exact_remote_stream_and_preserves_future_projection_fields() {
    assert_eq!(
      serde_json::to_value(OpenStream::new("control", "session/control")).unwrap(),
      json!({"type":"open","streamId":"control","endpoint":"session/control","payload":{"args":{}}})
    );
    let native = json!({"type":"baseline","value":{"projections":{"session":{"asOfSeq":-1,"values":{"future":{"preserved":true}},"future":42}}},"future":"frame"});
    let frame: ControlFrame = serde_json::from_value(native.clone()).unwrap();
    assert_eq!(serde_json::to_value(frame).unwrap(), native);
  }
}
