//! Standard session configuration selectors. IDs and categories remain opaque
//! strings; interpreting a model or permission choice belongs to the client.
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigOption {
  pub id: String,
  pub name: String,
  pub description: Option<String>,
  pub category: Option<String>,
  #[serde(rename = "_meta")]
  pub meta: Option<Value>,
  #[serde(flatten)]
  pub value: SessionConfigValue,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum SessionConfigValue {
  Select {
    current_value: String,
    options: ConfigSelectOptions,
  },
  Boolean {
    current_value: bool,
  },
  #[serde(other)]
  Unknown,
}

/// ACP allows an array of values or an array of groups, never a mixed array.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum ConfigSelectOptions {
  Flat(Vec<ConfigSelectOption>),
  Grouped(Vec<ConfigSelectGroup>),
}

#[derive(Clone, Debug, Deserialize)]
pub struct ConfigSelectOption {
  pub value: String,
  pub name: String,
  pub description: Option<String>,
  #[serde(rename = "_meta")]
  pub meta: Option<Value>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ConfigSelectGroup {
  pub group: String,
  pub name: String,
  pub options: Vec<ConfigSelectOption>,
  #[serde(rename = "_meta")]
  pub meta: Option<Value>,
}

/// The setter returns the complete authoritative configuration list.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionConfigOptionResult {
  pub config_options: Vec<SessionConfigOption>,
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  fn selector(options: Value) -> Value {
    json!({
      "id":"opaque/config:7", "name":"Model", "category":"model", "type":"select",
      "currentValue":"opaque:model/a", "options":options,
      "_meta":{"example.invalid/extra":{"opaque":true}}
    })
  }

  #[test]
  fn supports_flat_and_grouped_choices_without_interpreting_their_ids() {
    let leaf = json!({"value":"opaque:model/a", "name":"Model A", "_meta":{"extra":42}});
    let flat: SessionConfigOption = serde_json::from_value(selector(json!([leaf.clone()]))).unwrap();
    assert_eq!(flat.id, "opaque/config:7");
    assert_eq!(flat.category.as_deref(), Some("model"));
    assert_eq!(flat.meta.as_ref().unwrap()["example.invalid/extra"]["opaque"], true);
    let SessionConfigValue::Select {
      current_value,
      options: ConfigSelectOptions::Flat(values),
    } = flat.value
    else {
      panic!("flat select expected")
    };
    assert_eq!(current_value, "opaque:model/a");
    assert_eq!(values[0].value, current_value);
    assert_eq!(values[0].meta.as_ref().unwrap()["extra"], 42);
    let grouped: SessionConfigOption = serde_json::from_value(selector(json!([{
      "group":"provider/one", "name":"Provider One", "options":[leaf]
    }])))
    .unwrap();
    let SessionConfigValue::Select {
      options: ConfigSelectOptions::Grouped(groups),
      ..
    } = grouped.value
    else {
      panic!("grouped select expected")
    };
    assert_eq!(groups[0].group, "provider/one");
    assert_eq!(groups[0].options[0].value, "opaque:model/a");
  }

  #[test]
  fn rejects_mixed_arrays_and_incomplete_known_selectors() {
    let leaf = json!({"value":"one", "name":"One"});
    let group = json!({"group":"group", "name":"Group", "options":[leaf.clone()]});
    for options in [json!([leaf, group]), json!([{"group":"group", "options":[]}])] {
      assert!(serde_json::from_value::<SessionConfigOption>(selector(options)).is_err());
    }
    let mut invalid = selector(json!([]));
    invalid["currentValue"] = json!(true);
    assert!(serde_json::from_value::<SessionConfigOption>(invalid).is_err());
    assert!(serde_json::from_value::<SetSessionConfigOptionResult>(json!({})).is_err());
  }

  #[test]
  fn keeps_categories_optional_and_tolerates_future_option_types() {
    let value: SessionConfigOption = serde_json::from_value(json!({
      "id":"future", "name":"Future", "type":"future_type", "customShape":{"value":1}
    }))
    .unwrap();
    assert!(value.category.is_none());
    assert!(matches!(value.value, SessionConfigValue::Unknown));
    let boolean: SessionConfigOption = serde_json::from_value(json!({
      "id":"toggle", "name":"Toggle", "type":"boolean", "currentValue":false
    }))
    .unwrap();
    assert!(matches!(
      boolean.value,
      SessionConfigValue::Boolean { current_value: false }
    ));
  }
}
