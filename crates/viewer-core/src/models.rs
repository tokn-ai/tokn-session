//! Session-scoped model discovery and confirmed selection contracts.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
  collections::{HashMap, VecDeque},
  future::Future,
  sync::{Arc, Mutex},
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelIdentity {
  pub model_provider: Option<String>,
  pub model_id: String,
  pub display_name: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStateSource {
  Live,
  Recorded,
  Catalog,
  Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionModelState {
  pub model: Option<ModelIdentity>,
  pub source: ModelStateSource,
  pub revision: String,
}

impl SessionModelState {
  pub(crate) fn live(model: Option<ModelIdentity>, generation: &str) -> Self {
    Self::new(model, ModelStateSource::Live, generation)
  }

  pub(crate) fn unknown() -> Self {
    Self::new(None, ModelStateSource::Unknown, "")
  }

  pub(crate) fn new(model: Option<ModelIdentity>, source: ModelStateSource, generation: &str) -> Self {
    let revision = format!(
      "{:x}",
      Sha256::digest(serde_json::to_vec(&(generation, &model, source)).unwrap())
    );
    Self {
      model,
      source,
      revision,
    }
  }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionModelStatusRequest {
  pub session_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionModelStatus {
  pub state: SessionModelState,
  pub can_select: bool,
  pub can_list: bool,
  pub reason: Option<String>,
  pub runtime_generation: Option<String>,
  pub updates_pi_default: bool,
}

impl SessionModelStatus {
  pub(crate) fn unavailable(reason: impl Into<String>) -> Self {
    Self {
      state: SessionModelState::unknown(),
      can_select: false,
      can_list: false,
      reason: Some(reason.into()),
      runtime_generation: None,
      updates_pi_default: false,
    }
  }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelChoice {
  pub model: ModelIdentity,
  pub supports_images: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ListSessionModelsRequest {
  pub session_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ListSessionModelsResponse {
  pub status: SessionModelStatus,
  pub models: Vec<ModelChoice>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SelectSessionModelRequest {
  pub session_key: String,
  pub request_id: String,
  pub model_provider: Option<String>,
  pub model_id: String,
  pub expected_revision: String,
  pub runtime_generation: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSelectionStatus {
  Applied,
  NotApplied,
  Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SelectSessionModelResponse {
  pub request_id: String,
  pub status: ModelSelectionStatus,
  pub message: String,
  pub state: Option<SessionModelState>,
}

impl SelectSessionModelResponse {
  pub(crate) fn not_applied(request_id: &str, message: impl Into<String>) -> Self {
    Self {
      request_id: request_id.into(),
      status: ModelSelectionStatus::NotApplied,
      message: message.into(),
      state: None,
    }
  }
}

pub(crate) fn validate_selection(request: &SelectSessionModelRequest) -> Result<(), String> {
  if uuid::Uuid::parse_str(&request.request_id).is_err() {
    return Err("Invalid model request ID".into());
  }
  for (value, label) in [
    (&request.model_id, "model"),
    (&request.expected_revision, "model revision"),
  ] {
    if !valid_identity(value) {
      return Err(format!("Invalid {label}"));
    }
  }
  for value in [&request.model_provider, &request.runtime_generation]
    .into_iter()
    .flatten()
  {
    if !valid_identity(value) {
      return Err("Invalid model provider or runtime generation".into());
    }
  }
  Ok(())
}

pub(crate) fn valid_identity(value: &str) -> bool {
  !value.is_empty() && value.len() <= 512 && value.trim() == value && !value.chars().any(char::is_control)
}

/// Only compact display metadata crosses the viewer boundary. Native model
/// objects can include provider URLs and authentication headers.
pub(crate) fn pi_model(value: &serde_json::Value) -> Option<ModelIdentity> {
  let model_id = value.get("id")?.as_str()?.to_string();
  let model_provider = value.get("provider")?.as_str()?.to_string();
  if !valid_identity(&model_id) || !valid_identity(&model_provider) {
    return None;
  }
  Some(ModelIdentity {
    model_provider: Some(model_provider),
    model_id,
    display_name: value
      .get("name")
      .and_then(serde_json::Value::as_str)
      .filter(|name| valid_identity(name))
      .map(str::to_owned),
  })
}

pub(crate) fn pi_choices(value: &serde_json::Value) -> Result<Vec<ModelChoice>, String> {
  let values = value.as_array().ok_or("Pi returned an invalid model catalog")?;
  if values.len() > 512 {
    return Err("Pi model catalog exceeds the 512 model limit".into());
  }
  let mut identities = std::collections::HashSet::new();
  values
    .iter()
    .map(|value| {
      let model = pi_model(value).ok_or("Pi returned an invalid model")?;
      if !identities.insert((model.model_provider.clone(), model.model_id.clone())) {
        return Err("Pi returned duplicate model identities".into());
      }
      Ok(ModelChoice {
        model,
        supports_images: value
          .get("input")
          .and_then(serde_json::Value::as_array)
          .is_some_and(|input| input.iter().any(|value| value.as_str() == Some("image"))),
      })
    })
    .collect()
}

#[derive(Default)]
struct ControlReceipts {
  receipts: HashMap<String, (String, SelectSessionModelResponse)>,
  completed: VecDeque<String>,
}

#[derive(Clone, Default)]
pub(crate) struct ModelControls(Arc<Mutex<ControlReceipts>>);

impl ModelControls {
  pub(crate) async fn submit<F>(
    &self,
    identity: String,
    request: SelectSessionModelRequest,
    action: F,
  ) -> SelectSessionModelResponse
  where
    F: Future<Output = SelectSessionModelResponse> + Send + 'static,
  {
    let fingerprint = format!(
      "{:x}",
      Sha256::digest(
        serde_json::to_vec(&(
          identity,
          &request.model_provider,
          &request.model_id,
          &request.expected_revision,
          &request.runtime_generation
        ))
        .unwrap()
      )
    );
    let pending = SelectSessionModelResponse {
      request_id: request.request_id.clone(),
      status: ModelSelectionStatus::Unknown,
      message: "Model selection is still pending; refresh its status before trying another change".into(),
      state: None,
    };
    {
      let mut store = self.0.lock().unwrap_or_else(|error| error.into_inner());
      if let Some((previous, response)) = store.receipts.get(&request.request_id) {
        return if previous == &fingerprint {
          response.clone()
        } else {
          SelectSessionModelResponse::not_applied(
            &request.request_id,
            "This request ID was already used for another model change",
          )
        };
      }
      while store.receipts.len() >= 256 {
        let Some(oldest) = store.completed.pop_front() else {
          return SelectSessionModelResponse::not_applied(&request.request_id, "Too many model changes are pending");
        };
        store.receipts.remove(&oldest);
      }
      store
        .receipts
        .insert(request.request_id.clone(), (fingerprint, pending.clone()));
    }
    let store = self.clone();
    // Disconnecting a browser must not cancel an admitted native mutation.
    tokio::spawn(async move {
      let response = tokio::spawn(action)
        .await
        .unwrap_or_else(|_| SelectSessionModelResponse {
          request_id: request.request_id.clone(),
          status: ModelSelectionStatus::Unknown,
          message: "Model selection was interrupted; refresh its current model before another change".into(),
          state: None,
        });
      let mut store = store.0.lock().unwrap_or_else(|error| error.into_inner());
      if let Some((_, previous)) = store.receipts.get_mut(&request.request_id) {
        *previous = response.clone();
      }
      store.completed.push_back(request.request_id);
      response
    })
    .await
    .unwrap_or(pending)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn pi_catalog_is_bounded_allowlisted_metadata() {
    let choices = pi_choices(
      &json!([{ "id":"test", "provider":"custom", "name":"Test", "input":["text","image"],
      "baseUrl":"secret-endpoint", "headers":{"authorization":"secret"} }]),
    )
    .unwrap();
    let display = serde_json::to_string(&choices).unwrap();
    assert!(choices[0].supports_images);
    assert!(!display.contains("secret"));
    assert!(pi_choices(&json!([{"id":"test","provider":"custom"},{"id":"test","provider":"custom"}])).is_err());
  }

  #[tokio::test]
  async fn model_receipts_deduplicate_unknown_outcomes_and_conflicting_requests() {
    let controls = ModelControls::default();
    let request = SelectSessionModelRequest {
      session_key: "session".into(),
      request_id: uuid::Uuid::new_v4().to_string(),
      model_provider: Some("test".into()),
      model_id: "first".into(),
      expected_revision: "revision".into(),
      runtime_generation: Some("worker".into()),
    };
    let unknown = SelectSessionModelResponse {
      request_id: request.request_id.clone(),
      status: ModelSelectionStatus::Unknown,
      message: "lost acknowledgement".into(),
      state: None,
    };
    assert_eq!(
      controls
        .submit("owner".into(), request.clone(), async move { unknown })
        .await
        .status,
      ModelSelectionStatus::Unknown
    );
    assert_eq!(
      controls
        .submit("owner".into(), request.clone(), async {
          panic!("must not repeat an ambiguous mutation")
        })
        .await
        .status,
      ModelSelectionStatus::Unknown
    );
    let mut conflict = request;
    conflict.model_id = "other".into();
    assert_eq!(
      controls
        .submit("owner".into(), conflict, async { panic!("must not reuse a receipt") })
        .await
        .status,
      ModelSelectionStatus::NotApplied
    );
  }

  #[tokio::test]
  async fn disconnecting_a_model_caller_keeps_the_mutation_and_receipt_alive() {
    let controls = ModelControls::default();
    let request = SelectSessionModelRequest {
      session_key: "session".into(),
      request_id: uuid::Uuid::new_v4().to_string(),
      model_provider: Some("test".into()),
      model_id: "first".into(),
      expected_revision: "revision".into(),
      runtime_generation: Some("worker".into()),
    };
    let (started, ready) = tokio::sync::oneshot::channel();
    let (complete, wait) = tokio::sync::oneshot::channel();
    let worker_controls = controls.clone();
    let worker_request = request.clone();
    let response_id = request.request_id.clone();
    let caller = tokio::spawn(async move {
      worker_controls
        .submit("owner".into(), worker_request, async move {
          let _ = started.send(());
          let _ = wait.await;
          SelectSessionModelResponse {
            request_id: response_id,
            status: ModelSelectionStatus::Applied,
            message: "selected".into(),
            state: None,
          }
        })
        .await
    });
    ready.await.unwrap();
    caller.abort();
    assert_eq!(
      controls
        .submit("owner".into(), request.clone(), async {
          panic!("pending change must not be repeated")
        })
        .await
        .status,
      ModelSelectionStatus::Unknown
    );
    complete.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
      loop {
        if controls
          .submit("owner".into(), request.clone(), async {
            panic!("confirmed change must not repeat")
          })
          .await
          .status
          == ModelSelectionStatus::Applied
        {
          break;
        }
        tokio::task::yield_now().await;
      }
    })
    .await
    .unwrap();
  }
}
