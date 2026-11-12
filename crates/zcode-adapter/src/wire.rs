use super::*;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Workspace {
  pub workspace_path: String,
  pub workspace_key: String,
  pub workspace_identity: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeSession {
  pub session_id: String,
  pub workspace: Workspace,
  pub session_kind: String,
  pub model: Option<ModelSelection>,
}

#[derive(Deserialize)]
pub(super) struct SessionDirectory {
  pub sessions: Vec<NativeSession>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ModelSelection {
  pub provider_id: String,
  pub model_id: String,
}

impl ModelSelection {
  pub fn into_model(self, display_name: Option<String>) -> ModelIdentity {
    ModelIdentity {
      model_provider: Some(self.provider_id),
      model_id: self.model_id,
      display_name,
    }
  }

  pub fn from_model(model: ModelIdentity) -> Result<Self, InteractionError> {
    let model_id = model.model_id;
    let provider_id = model
      .model_provider
      .filter(|id| !id.trim().is_empty() && id.trim() == id)
      .ok_or_else(|| InteractionError::invalid("ZCode model selection requires a native provider ID"))?;
    if model_id.trim().is_empty() || model_id.trim() != model_id {
      return Err(InteractionError::invalid(
        "ZCode model selection requires a native model ID",
      ));
    }
    Ok(Self { provider_id, model_id })
  }
}

#[derive(Deserialize)]
pub(super) struct Snapshot {
  pub protocol: Protocol,
  pub session: NativeSession,
  pub runtime: Runtime,
  pub settings: Settings,
  pub projection: Projection,
  #[serde(rename = "slashCommands", default)]
  pub commands: Vec<SlashCommand>,
}

#[derive(Deserialize)]
pub(super) struct Protocol {
  pub name: String,
  pub version: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Runtime {
  pub active_turn_id: Option<String>,
  pub state_revision: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Projection {
  pub session_id: String,
  pub status: String,
  pub turn_count: u64,
}

impl Projection {
  pub fn matches_session(&self, id: &str) -> bool {
    // The native reducer has no session event to project before the first turn.
    // The caller independently checks the authoritative session record's ID.
    self.session_id == id || (self.session_id == "unknown" && self.turn_count == 0 && self.status == "idle")
  }
}

#[derive(Deserialize)]
pub(super) struct Settings {
  pub model: ModelSettings,
}

#[derive(Deserialize)]
pub(super) struct ModelSettings {
  pub current: Option<ModelSelection>,
  pub available: Vec<NativeModel>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeModel {
  #[serde(rename = "ref")]
  pub selection: ModelSelection,
  pub label: String,
  pub disabled_reason: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct SlashCommand {
  pub name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Admission {
  pub session_id: String,
  pub accepted: bool,
}

pub(super) fn decode<T: serde::de::DeserializeOwned>(value: Value, mutation: bool) -> Result<T, InteractionError> {
  serde_json::from_value(value).map_err(|_| {
    if mutation {
      InteractionError::uncertain("ZCode returned an invalid response after a mutation")
    } else {
      InteractionError::unavailable("ZCode returned an invalid app-server response")
    }
  })
}

pub(super) fn status(value: &str) -> ExecutionState {
  match value {
    "idle" | "completed" | "error" => ExecutionState::Idle,
    "running" | "waiting" => ExecutionState::Running,
    "paused" => ExecutionState::Stopped,
    _ => ExecutionState::Unknown,
  }
}

impl Snapshot {
  pub fn state(&self) -> SessionState {
    SessionState {
      session_id: Some(self.session.session_id.clone()),
      status: if self.runtime.active_turn_id.is_some() {
        ExecutionState::Running
      } else {
        status(&self.projection.status)
      },
      model: self
        .settings
        .model
        .current
        .clone()
        .or_else(|| self.session.model.clone())
        .map(|model| model.into_model(None)),
      // Legacy state has no authoritative queue count.
      pending_inputs: None,
      revision: Some(self.runtime.state_revision.to_string()),
    }
  }

  pub fn models(self) -> Vec<ModelChoice> {
    self
      .settings
      .model
      .available
      .into_iter()
      .filter(|model| model.disabled_reason.is_none())
      .map(|model| ModelChoice {
        model: model.selection.into_model(Some(model.label)),
        supports_images: false,
      })
      .collect()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn allows_the_native_empty_projection_but_rejects_conflicting_ids() {
    let mut projection = Projection {
      session_id: "unknown".into(),
      status: "idle".into(),
      turn_count: 0,
    };
    assert!(projection.matches_session("sess_expected"));
    projection.turn_count = 1;
    assert!(!projection.matches_session("sess_expected"));
    projection.turn_count = 0;
    projection.session_id = "sess_other".into();
    assert!(!projection.matches_session("sess_expected"));
  }
}
