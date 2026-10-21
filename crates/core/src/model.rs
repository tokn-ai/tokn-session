use serde::{Deserialize, Serialize};

/// Model vendor and ID are distinct from the agent/session provider.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelIdentity {
  pub model_provider: Option<String>,
  pub model_id: String,
}

/// What the provider actually recorded, rather than an assertion that every
/// observation represents a user changing the selected model.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelOrigin {
  Selection,
  Request,
  Response,
  Catalog,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelObservation {
  pub model: ModelIdentity,
  pub origin: ModelOrigin,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn model_identity_and_origin_keep_provider_scope_and_snake_case_wire_fields() {
    let observation = ModelObservation {
      model: ModelIdentity {
        model_provider: Some("custom".into()),
        model_id: "model/id".into(),
      },
      origin: ModelOrigin::Request,
    };
    let wire = serde_json::to_value(&observation).unwrap();
    assert_eq!(
      wire,
      serde_json::json!({
        "model":{"model_provider":"custom","model_id":"model/id"}, "origin":"request"
      })
    );
    assert_eq!(serde_json::from_value::<ModelObservation>(wire).unwrap(), observation);
  }
}
