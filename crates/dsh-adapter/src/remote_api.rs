//! Request mapping for the authenticated Typert Remote generation (DSH 0.2).
//! Method arguments belong inside the generated namespace's named `args` object.
use serde_json::{Value, json};
use tokn_interaction_core::InteractionError;

pub(crate) fn request(
  method: &str,
  rpc_id: &str,
  mut payload: Value,
) -> Result<(&'static str, Value), InteractionError> {
  let endpoint = match method {
    "llm.models" => return Ok(("session/modelCatalog", json!({"args":{}}))),
    "$events/result" => return Ok(("$events/result", json!({"args":payload}))),
    "session.list" => "session/list",
    "session.create" => "session/create",
    "session.selectModel" => "session/selectModel",
    "session.projections" => "session/projections",
    "session.cancel" => "session/cancel",
    "session.prompt" => {
      payload
        .as_object_mut()
        .ok_or_else(|| InteractionError::invalid("Invalid DSH prompt request"))?
        .insert("requestId".into(), Value::String(rpc_id.into()));
      "session/prompt"
    }
    _ => {
      return Err(InteractionError::unsupported(
        "This operation is unavailable on the DSH Remote protocol",
      ));
    }
  };
  Ok((endpoint, json!({"args":{"request":payload}})))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn maps_generated_names_arguments_and_durable_prompt_identity() {
    assert_eq!(
      request("llm.models", "catalog", json!({})).unwrap(),
      ("session/modelCatalog", json!({"args":{}}))
    );
    assert_eq!(
      request("session.list", "list", json!({})).unwrap(),
      ("session/list", json!({"args":{"request":{}}}))
    );
    assert_eq!(
      request(
        "session.prompt",
        "input-once",
        json!({"sessionId":"new-session","mode":"steer","content":[]})
      )
      .unwrap(),
      (
        "session/prompt",
        json!({"args":{"request":{"sessionId":"new-session","mode":"steer","content":[],"requestId":"input-once"}}})
      )
    );
    assert_eq!(
      request(
        "$events/result",
        "response",
        json!({"clientId":"client","eventId":"event","outcome":{"kind":"next"}})
      )
      .unwrap(),
      (
        "$events/result",
        json!({"args":{"clientId":"client","eventId":"event","outcome":{"kind":"next"}}})
      )
    );
    assert!(request("session.models", "old-contract", json!({})).is_err());
  }
}
