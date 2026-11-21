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
  #[serde(skip_serializing_if = "Option::is_none")]
  pub options: Option<ModelOptions>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ModelOptions {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub reasoning_level: Option<String>,
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
    Ok(Self {
      provider_id,
      model_id,
      options: None,
    })
  }

  fn matches(&self, model: &ModelIdentity) -> bool {
    model.model_provider.as_deref() == Some(self.provider_id.as_str()) && model.model_id == self.model_id
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
  pub pending_request_ids: Option<Vec<String>>,
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

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeModel {
  #[serde(rename = "ref")]
  pub selection: ModelSelection,
  pub label: String,
  pub disabled_reason: Option<String>,
  pub properties: Option<ModelProperties>,
  pub reasoning: Option<ModelReasoning>,
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) struct ModelProperties {
  pub input_format: ModelInputFormat,
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) struct ModelInputFormat {
  #[serde(default)]
  pub supports_image: bool,
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) struct ModelReasoning {
  pub levels: Vec<ReasoningLevel>,
  pub default_level: Option<String>,
}

#[derive(Clone, Deserialize, PartialEq)]
pub(super) struct ReasoningLevel {
  pub value: String,
}

impl NativeModel {
  pub fn same_identity(&self, other: &Self) -> bool {
    self.selection.provider_id == other.selection.provider_id && self.selection.model_id == other.selection.model_id
  }

  pub fn same_configuration(&self, other: &Self) -> bool {
    self.label == other.label
      && self.disabled_reason == other.disabled_reason
      && self.properties == other.properties
      && self.reasoning == other.reasoning
  }

  fn supports_images(&self) -> bool {
    self
      .properties
      .as_ref()
      .is_some_and(|properties| properties.input_format.supports_image)
  }
}

pub(super) struct CapturedCatalog {
  pub source_session: String,
  pub models: Vec<NativeModel>,
}

#[derive(Deserialize)]
pub(super) struct SlashCommand {
  pub name: String,
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
  pub fn input_model(
    &self,
    requested: Option<&ModelIdentity>,
    images: bool,
  ) -> Result<(ModelSelection, ModelIdentity), InteractionError> {
    let current = self.settings.model.current.as_ref().or(self.session.model.as_ref());
    let identity = match requested {
      Some(requested) => ModelSelection::from_model(requested.clone())?.into_model(None),
      None => current
        .cloned()
        .map(|selection| selection.into_model(None))
        .ok_or_else(|| InteractionError::new(ErrorKind::Rejected, "ZCode has no current model to receive images"))?,
    };
    let model = self
      .settings
      .model
      .available
      .iter()
      .find(|model| model.disabled_reason.is_none() && model.selection.matches(&identity))
      .ok_or_else(|| InteractionError::new(ErrorKind::Rejected, "ZCode model is unavailable in the current catalog"))?;
    if images && !model.supports_images() {
      return Err(InteractionError::new(
        ErrorKind::Rejected,
        "The selected ZCode model does not support image input",
      ));
    }
    let mut selection = current
      .filter(|selection| selection.matches(&identity))
      .cloned()
      .unwrap_or_else(|| model.selection.clone());
    let reasoning = selection
      .options
      .as_ref()
      .and_then(|options| options.reasoning_level.as_ref());
    if let Some(options) = &model.reasoning {
      let level = reasoning.or(options.default_level.as_ref());
      if !options.levels.is_empty()
        && !level.is_some_and(|level| options.levels.iter().any(|option| &option.value == level))
      {
        return Err(InteractionError::new(
          ErrorKind::Rejected,
          "ZCode model does not advertise a valid reasoning choice",
        ));
      }
      if let Some(level) = level {
        selection.options = Some(ModelOptions {
          reasoning_level: Some(level.clone()),
        });
      }
    }
    let identity = selection.clone().into_model(Some(model.label.clone()));
    Ok((selection, identity))
  }

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
      .map(|model| {
        let supports_images = model.supports_images();
        ModelChoice {
          model: model.selection.into_model(Some(model.label)),
          supports_images,
        }
      })
      .collect()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn model_identity(id: &str) -> ModelIdentity {
    ModelIdentity {
      model_provider: Some("fixture".into()),
      model_id: id.into(),
      display_name: None,
    }
  }

  fn model_snapshot() -> Value {
    json!({
      "protocol":{"name":"ZCode Protocol","version":1},
      "session":{"sessionId":"sess_fixture","sessionKind":"interactive",
        "workspace":{"workspacePath":"/fixture","workspaceKey":"/fixture"}},
      "projection":{"sessionId":"sess_fixture","status":"idle","turnCount":1},
      "runtime":{"stateRevision":7},
      "settings":{"model":{
        "current":{"providerId":"fixture","modelId":"a","options":{"reasoningLevel":"low"}},
        "available":[
          {"ref":{"providerId":"fixture","modelId":"a"},"label":"A",
            "properties":{"inputFormat":{"supportsImage":true}},
            "reasoning":{"levels":[{"value":"low"},{"value":"high"}],"defaultLevel":"high"}},
          {"ref":{"providerId":"fixture","modelId":"b"},"label":"B",
            "properties":{"inputFormat":{"supportsImage":true}},
            "reasoning":{"levels":[{"value":"high"}],"defaultLevel":"high"}},
          {"ref":{"providerId":"fixture","modelId":"unknown-media"},"label":"Unknown media"}
        ]
      }}
    })
  }

  #[test]
  fn freezes_native_reasoning_and_truthful_image_capabilities_for_each_model_intent() {
    let snapshot: Snapshot = serde_json::from_value(model_snapshot()).unwrap();
    let (current, identity) = snapshot.input_model(None, true).unwrap();
    assert_eq!(identity.model_id, "a");
    assert_eq!(current.options.unwrap().reasoning_level.as_deref(), Some("low"));
    let (selected, identity) = snapshot.input_model(Some(&model_identity("b")), true).unwrap();
    assert_eq!(identity.model_id, "b");
    assert_eq!(selected.options.unwrap().reasoning_level.as_deref(), Some("high"));
    assert_eq!(
      snapshot
        .input_model(Some(&model_identity("unknown-media")), true)
        .err()
        .unwrap()
        .kind,
      ErrorKind::Rejected
    );
    assert!(
      snapshot
        .input_model(Some(&model_identity("unknown-media")), false)
        .is_ok()
    );
    let choices = snapshot.models();
    assert!(choices[0].supports_images);
    assert!(!choices[2].supports_images);
  }

  #[test]
  fn missing_or_invalid_reasoning_defaults_and_disabled_models_reject_before_admission() {
    for change in ["missing-default", "unsupported-default", "disabled"] {
      let mut value = model_snapshot();
      let selected = &mut value["settings"]["model"]["available"][1];
      match change {
        "missing-default" => {
          selected["reasoning"].as_object_mut().unwrap().remove("defaultLevel");
        }
        "unsupported-default" => selected["reasoning"]["defaultLevel"] = json!("other"),
        _ => selected["disabledReason"] = json!("fixture disabled"),
      }
      let snapshot: Snapshot = serde_json::from_value(value).unwrap();
      assert_eq!(
        snapshot
          .input_model(Some(&model_identity("b")), true)
          .err()
          .unwrap()
          .kind,
        ErrorKind::Rejected
      );
    }
    let selection: ModelSelection = serde_json::from_value(json!({
      "providerId":"fixture","modelId":"a","options":{}
    }))
    .unwrap();
    assert_eq!(serde_json::to_value(selection).unwrap()["options"], json!({}));
  }

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
