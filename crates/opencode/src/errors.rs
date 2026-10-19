use serde_json::Value;
use tokn_session_core::{AgentEvent, ErrorEvent, MetadataEvent, MetadataKind, Provider};

pub(crate) struct ProviderError {
  pub cancelled: bool,
  pub message: String,
}

pub(crate) fn classify(error: &Value) -> Option<ProviderError> {
  if !error.is_object() && !error.is_string() {
    return None;
  }
  let name = nonblank(error.get("name"));
  let message = nonblank(error.pointer("/data/message"))
    .or_else(|| nonblank(error.get("message")))
    .or_else(|| nonblank(Some(error)))
    .or(name)
    .unwrap_or("Provider request failed");
  Some(ProviderError {
    cancelled: error.get("name").and_then(Value::as_str) == Some("MessageAbortedError"),
    message: message.into(),
  })
}

pub(crate) fn normalize(
  provider: Provider,
  session_id: Option<String>,
  error: &Value,
  native_type: &str,
  native: Value,
  timestamp: Option<String>,
) -> Option<Vec<AgentEvent>> {
  let error = classify(error)?;
  let mut events = Vec::new();
  if !error.cancelled {
    events.push(AgentEvent::Error(ErrorEvent {
      provider,
      session_id: session_id.clone(),
      message: error.message.clone(),
      timestamp: timestamp.clone(),
    }));
  }
  events.push(diagnostic(provider, session_id, &error, native_type, native, timestamp));
  Some(events)
}

pub(crate) fn diagnostic(
  provider: Provider,
  session_id: Option<String>,
  error: &ProviderError,
  native_type: &str,
  native: Value,
  timestamp: Option<String>,
) -> AgentEvent {
  AgentEvent::Metadata(MetadataEvent {
    provider,
    session_id,
    kind: MetadataKind::Diagnostic,
    native_type: native_type.into(),
    summary: if error.cancelled {
      "Provider response interrupted".into()
    } else {
      error.message.clone()
    },
    native,
    timestamp,
  })
}

fn nonblank(value: Option<&Value>) -> Option<&str> {
  value
    .and_then(Value::as_str)
    .map(str::trim)
    .filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests;
