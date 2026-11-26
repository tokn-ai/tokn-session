use serde::Deserialize;
use serde_json::Value;
use tokn_session_core::{ModelOrigin, ResponseMetadata};

// The pi-ai adapter's versioned response envelope distinguishes the requested
// replay identity from a concrete model explicitly reported by its provider.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PiAiResponse<'a> {
  kind: &'a str,
  version: u64,
  provider: &'a str,
  model: &'a str,
  #[serde(default, borrow)]
  response_model: Option<&'a str>,
}

pub(super) fn for_message(message: &Value) -> Option<ResponseMetadata> {
  let source = &message["source"];
  if source["kind"] != "model" {
    return None;
  }
  let requested = ResponseMetadata::recorded_model(
    source["provider"].as_str(),
    source["model"].as_str(),
    ModelOrigin::Request,
  );
  let reported = PiAiResponse::deserialize(&source["replayState"]["response"])
    .ok()
    .filter(|response| {
      response.kind == "pi-ai" && response.version == 2 && source["model"].as_str() == Some(response.model)
    })
    .and_then(|response| {
      ResponseMetadata::recorded_model(Some(response.provider), response.response_model, ModelOrigin::Response)
    });
  ResponseMetadata::new(reported.or(requested), None)
}
