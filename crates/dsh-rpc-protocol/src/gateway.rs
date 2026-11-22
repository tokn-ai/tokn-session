//! DSH 0.2 Gateway streams. ApiProxy's legacy envelopes remain in `api`.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClientFrame {
  #[serde(rename_all = "camelCase")]
  Open {
    stream_id: String,
    endpoint: String,
    payload: Value,
  },
  #[serde(rename_all = "camelCase")]
  End { stream_id: String },
  #[serde(rename_all = "camelCase")]
  Cancel { stream_id: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerFrame {
  #[serde(rename_all = "camelCase")]
  Item {
    stream_id: String,
    #[serde(default)]
    value: Value,
  },
  #[serde(rename_all = "camelCase")]
  Error {
    stream_id: String,
    error: super::api::RpcError,
  },
  #[serde(rename_all = "camelCase")]
  End { stream_id: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RemoteEvent {
  #[serde(rename_all = "camelCase")]
  Ready {
    client_id: String,
    host: Value,
  },
  Emit {
    event: String,
    args: Vec<Value>,
  },
  #[serde(rename_all = "camelCase")]
  Waterfall {
    event: String,
    event_id: String,
    agent_id: String,
    request: Value,
  },
  #[serde(rename_all = "camelCase")]
  Cancel {
    event_id: String,
  },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
  pub cursor: i64,
  pub active: bool,
  pub running: bool,
  pub queue: Option<Vec<Value>>,
  pub pending_input: Vec<String>,
  #[serde(flatten)]
  pub extra: super::api::ExtraFields,
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn gateway_stream_names_and_scoped_identity_are_native() {
    let frame = ClientFrame::Open {
      stream_id: "events".into(),
      endpoint: "$events".into(),
      payload: json!({"args":{}}),
    };
    assert_eq!(
      serde_json::to_value(frame).unwrap(),
      json!({"type":"open","streamId":"events","endpoint":"$events","payload":{"args":{}}})
    );
    let native = json!({"type":"waterfall","event":"approval/request","eventId":"event-1","agentId":"s","request":{"toolName":"shell"}});
    let event: RemoteEvent = serde_json::from_value(native.clone()).unwrap();
    assert_eq!(serde_json::to_value(event).unwrap(), native);
  }
}
