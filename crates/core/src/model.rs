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

/// Metadata recorded for the model call that produced this assistant message.
/// A routing request and a backend report carry different provenance.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResponseMetadata {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub model: Option<ModelObservation>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub fast_mode: Option<ResponseFastMode>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFastMode {
  Requested,
  Reported,
}

impl ResponseMetadata {
  pub fn new(model: Option<ModelObservation>, fast_mode: Option<ResponseFastMode>) -> Option<Self> {
    let model = model.and_then(|model| {
      Self::recorded_model(
        model.model.model_provider.as_deref(),
        Some(&model.model.model_id),
        model.origin,
      )
    });
    (model.is_some() || fast_mode.is_some()).then_some(Self { model, fast_mode })
  }

  pub fn recorded_model(provider: Option<&str>, id: Option<&str>, origin: ModelOrigin) -> Option<ModelObservation> {
    fn valid(value: &str) -> bool {
      !value.is_empty() && value.len() <= 512 && value.trim() == value && !value.chars().any(char::is_control)
    }

    let id = id.filter(|value| valid(value))?;
    matches!(origin, ModelOrigin::Request | ModelOrigin::Response).then(|| ModelObservation {
      model: ModelIdentity {
        model_provider: provider.filter(|value| valid(value)).map(str::to_owned),
        model_id: id.into(),
      },
      origin,
    })
  }
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
