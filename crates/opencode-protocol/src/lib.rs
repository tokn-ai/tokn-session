#![doc = include_str!("../README.md")]

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub mod run;
pub mod v1;
pub mod v2;

pub type ExtraFields = Map<String, Value>;

/// A tagged value that is either unfamiliar or malformed for its known tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnknownItem {
  pub native_type: Option<String>,
  pub native: Value,
  pub parse_error: Option<String>,
}

pub(crate) fn string_field(value: &Value, field: &str) -> Option<String> {
  value.get(field).and_then(Value::as_str).map(str::to_string)
}
