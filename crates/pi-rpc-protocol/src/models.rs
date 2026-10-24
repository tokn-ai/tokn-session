use serde_json::Value;
use std::collections::HashSet;

pub const MAX_MODEL_ID_BYTES: usize = 512;
pub const MAX_MODEL_CATALOG_ENTRIES: usize = 512;

/// Validated native model metadata suitable for an adapter's compact mapping.
/// Full model objects may contain credentials and remain in native envelopes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PiModel {
  pub provider: String,
  pub id: String,
  pub name: Option<String>,
  pub supports_images: bool,
}

pub fn parse_model(value: &Value) -> Option<PiModel> {
  Some(PiModel {
    provider: value
      .get("provider")?
      .as_str()
      .filter(|value| valid_identity(value))?
      .to_owned(),
    id: value
      .get("id")?
      .as_str()
      .filter(|value| valid_identity(value))?
      .to_owned(),
    name: value
      .get("name")
      .and_then(Value::as_str)
      .filter(|value| valid_identity(value))
      .map(str::to_owned),
    supports_images: value
      .get("input")
      .and_then(Value::as_array)
      .is_some_and(|input| input.iter().any(|value| value.as_str() == Some("image"))),
  })
}

/// Validate the `models` array from Pi's `get_available_models` response.
pub fn parse_model_catalog(value: &Value) -> Result<Vec<PiModel>, String> {
  let values = value.as_array().ok_or("Pi returned an invalid model catalog")?;
  if values.len() > MAX_MODEL_CATALOG_ENTRIES {
    return Err("Pi model catalog exceeds the 512 model limit".into());
  }
  let mut identities = HashSet::new();
  values
    .iter()
    .map(|value| {
      let model = parse_model(value).ok_or("Pi returned an invalid model")?;
      if !identities.insert((model.provider.clone(), model.id.clone())) {
        return Err("Pi returned duplicate model identities".into());
      }
      Ok(model)
    })
    .collect()
}

fn valid_identity(value: &str) -> bool {
  !value.is_empty()
    && value.len() <= MAX_MODEL_ID_BYTES
    && value.trim() == value
    && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn model_catalog_is_bounded_unique_native_metadata() {
    let models = parse_model_catalog(&json!([
      {"id":"test","provider":"custom","name":"Test","input":["text","image"],
       "baseUrl":"secret-endpoint","headers":{"authorization":"secret"}}
    ]))
    .unwrap();
    assert_eq!(
      models,
      [PiModel {
        provider: "custom".into(),
        id: "test".into(),
        name: Some("Test".into()),
        supports_images: true,
      }]
    );
    assert!(
      parse_model_catalog(&json!([
        {"id":"test","provider":"custom"},
        {"id":"test","provider":"custom"}
      ]))
      .is_err()
    );
    assert!(parse_model_catalog(&json!(null)).is_err());
    let values: Vec<_> = (0..=MAX_MODEL_CATALOG_ENTRIES)
      .map(|id| json!({"id":id.to_string(),"provider":"custom"}))
      .collect();
    assert!(parse_model_catalog(&json!(values)).is_err());
    assert_eq!(
      parse_model_catalog(&json!(&values[..MAX_MODEL_CATALOG_ENTRIES]))
        .unwrap()
        .len(),
      MAX_MODEL_CATALOG_ENTRIES
    );
  }

  #[test]
  fn validates_identities_without_requiring_optional_display_metadata() {
    for invalid in [
      json!(null),
      json!(""),
      json!(" leading"),
      json!("line\nbreak"),
      json!("x".repeat(MAX_MODEL_ID_BYTES + 1)),
    ] {
      for field in ["id", "provider"] {
        let mut value = json!({"id":"model","provider":"provider"});
        value[field] = invalid.clone();
        assert!(parse_model(&value).is_none());
      }
    }
    let model =
      parse_model(&json!({"id":"model","provider":"provider","name":"invalid\nname","input":"image"})).unwrap();
    assert_eq!(model.name, None);
    assert!(!model.supports_images);
    assert!(parse_model(&json!({"id":"x".repeat(MAX_MODEL_ID_BYTES),"provider":"provider"})).is_some());
  }
}
