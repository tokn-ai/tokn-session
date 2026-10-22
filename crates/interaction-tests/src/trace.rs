use crate::TestResult;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
  path::Path,
  sync::{Arc, Mutex},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireRecord {
  Http {
    method: String,
    path: String,
    request: Value,
    status: u16,
    response: Value,
  },
  Event {
    channel: String,
    body: Value,
  },
  Stdio {
    direction: String,
    body: Value,
  },
}

#[derive(Clone, Default)]
pub struct Trace(Arc<Mutex<Vec<WireRecord>>>);
impl Trace {
  pub fn push(&self, record: WireRecord) {
    self.0.lock().unwrap().push(record);
  }
  pub(crate) fn begin_http(&self, method: String, path: String, request: Value) -> usize {
    let mut records = self.0.lock().unwrap();
    let index = records.len();
    records.push(WireRecord::Http {
      method,
      path,
      request,
      status: 0,
      response: Value::Null,
    });
    index
  }
  pub(crate) fn complete_http(&self, index: usize, status: u16, response: Value) {
    let mut records = self.0.lock().unwrap();
    let WireRecord::Http {
      status: recorded_status,
      response: recorded_response,
      ..
    } = &mut records[index]
    else {
      unreachable!("HTTP reservation changed kind")
    };
    *recorded_status = status;
    *recorded_response = response;
  }
  pub fn records(&self) -> Vec<WireRecord> {
    self.0.lock().unwrap().clone()
  }
  pub fn fixture(&self, provider: &str, revision: &str, scenario: &str) -> Fixture {
    Fixture {
      schema_version: 1,
      provider: provider.into(),
      upstream_revision: revision.into(),
      scenario: scenario.into(),
      records: self.records(),
    }
  }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Fixture {
  pub schema_version: u32,
  pub provider: String,
  pub upstream_revision: String,
  pub scenario: String,
  pub records: Vec<WireRecord>,
}
impl Fixture {
  /// Explicit replacements preserve wire shape. Do not blindly replace IDs or
  /// drop fields: request/response correlation must survive replay.
  pub fn sanitize(&mut self, replacements: &[(&str, &str)]) {
    let mut json = serde_json::to_value(&*self).expect("fixture serializes");
    fn visit(value: &mut Value, replacements: &[(&str, &str)]) {
      match value {
        Value::String(text) => {
          for (from, to) in replacements {
            if !from.is_empty() {
              *text = text.replace(from, to);
            }
          }
        }
        Value::Array(values) => {
          for value in values {
            visit(value, replacements);
          }
        }
        Value::Object(values) => {
          let previous = std::mem::take(values);
          for (mut key, mut value) in previous {
            for (from, to) in replacements {
              if !from.is_empty() {
                key = key.replace(from, to);
              }
            }
            if value.is_string()
              && matches!(
                key.to_ascii_lowercase().as_str(),
                "apikey" | "api_key" | "authorization" | "password" | "access_token" | "refresh_token"
              )
            {
              value = Value::String("<redacted>".into());
            } else {
              visit(&mut value, replacements);
            }
            values.insert(key, value);
          }
        }
        _ => {}
      }
    }
    let encode = |text: &str| {
      let mut url = reqwest::Url::parse("http://fixture/").unwrap();
      url.query_pairs_mut().append_pair("value", text);
      url.query().unwrap().strip_prefix("value=").unwrap().to_owned()
    };
    let encoded = replacements
      .iter()
      .map(|(from, to)| (encode(from), encode(to)))
      .collect::<Vec<_>>();
    let encoded_refs = encoded
      .iter()
      .map(|(from, to)| (from.as_str(), to.as_str()))
      .collect::<Vec<_>>();
    visit(&mut json, &encoded_refs);
    visit(&mut json, replacements);
    *self = serde_json::from_value(json).expect("sanitization preserves fixture shape");
  }
  pub fn save(&self, path: impl AsRef<Path>) -> TestResult {
    if self
      .records
      .iter()
      .any(|record| matches!(record, WireRecord::Http { status: 0, .. }))
    {
      return Err("Cannot save a native capture with unfinished HTTP exchanges".into());
    }
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(self)?))?;
    Ok(())
  }
  pub fn load(path: impl AsRef<Path>) -> TestResult<Self> {
    let fixture: Self = serde_json::from_slice(&std::fs::read(path)?)?;
    if fixture.schema_version != 1 || fixture.upstream_revision.len() != 40 {
      return Err("Invalid native fixture provenance".into());
    }
    Ok(fixture)
  }
}
