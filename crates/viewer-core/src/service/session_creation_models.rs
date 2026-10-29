//! Lazy, project-scoped model discovery for drafts without a saved conversation.
use super::*;
use crate::workspace::{
  CreateProjectSessionRequest, ListSessionCreationModelsRequest, RegisteredProject, SessionCreationModels,
};
use sha2::{Digest, Sha256};
use tokn_interaction_core::{ModelChoice, Provider as LiveProvider, runtime::RuntimeSelection};

const MAX_MODELS: usize = 512;

impl ViewerService {
  pub async fn list_session_creation_models(
    &self,
    request: ListSessionCreationModelsRequest,
  ) -> Result<SessionCreationModels, String> {
    self.check_managed_workspace()?;
    let service = self.clone();
    let scope = request.clone();
    let (project, runtime) = tokio::task::spawn_blocking(move || service.creation_model_scope(&scope))
      .await
      .map_err(|_| "Could not validate the model catalog scope")??;
    if request.provider == LiveProvider::Dsh {
      return Ok(SessionCreationModels {
        models: Vec::new(),
        catalog_revision: None,
        runtime_profile_id: runtime.as_ref().map(|runtime| runtime.profile_id.clone()),
        runtime_revision: runtime.as_ref().map(|runtime| runtime.expected_revision),
        reason: Some("DSH uses its runtime default when creating a session.".into()),
      });
    }
    let cwd = PathBuf::from(&project.folder_path);
    let models = if request.provider == LiveProvider::Pi {
      self.sessions.managed.creation_models(cwd).await?
    } else {
      self
        .sessions
        .runtimes
        .creation_models(request.provider, cwd, runtime.clone())
        .await
        .map_err(|error| error.to_string())?
    };
    let models = normalize_models(models)?;
    // A catalog from a replaced project/default/profile must never verify a
    // selection made against the new configuration.
    let service = self.clone();
    let provider = request.provider;
    let (current_project, current_runtime) =
      tokio::task::spawn_blocking(move || service.creation_model_scope(&request))
        .await
        .map_err(|_| "Could not recheck the model catalog scope")??;
    if current_project.folder_path != project.folder_path || current_runtime != runtime {
      return Err("The project or runtime changed while loading models. Open the model picker again.".into());
    }
    let revision = catalog_revision(&project, provider, &runtime, &models)?;
    Ok(SessionCreationModels {
      reason: models
        .is_empty()
        .then(|| "This runtime did not advertise any models. You can use its default.".into()),
      models,
      catalog_revision: Some(revision),
      runtime_profile_id: runtime.as_ref().map(|runtime| runtime.profile_id.clone()),
      runtime_revision: runtime.as_ref().map(|runtime| runtime.expected_revision),
    })
  }

  fn creation_model_scope(
    &self,
    request: &ListSessionCreationModelsRequest,
  ) -> Result<(RegisteredProject, Option<RuntimeSelection>), String> {
    let project = self.validated_project(&request.project_id)?;
    let selected = super::session_creation::selected_creation_runtime(
      request.runtime_profile_id.as_deref(),
      request.runtime_revision,
    )?;
    let runtime = if request.provider == LiveProvider::Pi {
      if selected.is_some() {
        return Err("Pi uses its managed-session configuration; choose Automatic".into());
      }
      self.sessions.managed.available()?;
      None
    } else {
      self.sessions.runtimes.creation_model_selection(
        request.provider,
        Path::new(&project.folder_path),
        selected.as_ref(),
      )?
    };
    Ok((project, runtime))
  }

  pub(super) async fn preflight_creation_model(&self, request: &CreateProjectSessionRequest) -> Result<(), String> {
    let Some(selection) = &request.model else {
      return Ok(());
    };
    if selection.catalog_revision.len() != 64
      || !selection.catalog_revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
      return Err("Choose a model again; its catalog verification is missing.".into());
    }
    // Revalidate the user's original Automatic/explicit selection, then pin the
    // resolved runtime from the catalog for native creation after admission.
    let catalog = self
      .list_session_creation_models(ListSessionCreationModelsRequest {
        project_id: request.project_id.clone(),
        provider: request.provider,
        runtime_profile_id: request.runtime_profile_id.clone(),
        runtime_revision: request.runtime_revision,
      })
      .await?;
    validate_selection(request, &catalog)
  }
}

fn normalize_models(mut models: Vec<ModelChoice>) -> Result<Vec<ModelChoice>, String> {
  if models.len() > MAX_MODELS {
    return Err("The runtime returned too many models.".into());
  }
  let valid = |value: &str| {
    !value.is_empty() && value.len() <= 512 && value.trim() == value && !value.chars().any(char::is_control)
  };
  if models.iter().any(|choice| {
    !valid(&choice.model.model_id)
      || choice
        .model
        .model_provider
        .as_deref()
        .is_some_and(|value| !valid(value))
      || choice
        .model
        .display_name
        .as_ref()
        .is_some_and(|value| value.len() > 1024)
  }) {
    return Err("The runtime returned an invalid model identity.".into());
  }
  models.sort_by(|left, right| {
    (&left.model.model_provider, &left.model.model_id).cmp(&(&right.model.model_provider, &right.model.model_id))
  });
  // Conflicting duplicate entries are ambiguous, especially for image support.
  if models.windows(2).any(|pair| {
    pair[0].model.model_provider == pair[1].model.model_provider && pair[0].model.model_id == pair[1].model.model_id
  }) {
    return Err("The runtime returned duplicate model identities.".into());
  }
  Ok(models)
}

fn catalog_revision(
  project: &RegisteredProject,
  provider: LiveProvider,
  runtime: &Option<RuntimeSelection>,
  models: &[ModelChoice],
) -> Result<String, String> {
  let identities: Vec<_> = models
    .iter()
    .map(|choice| {
      (
        &choice.model.model_provider,
        &choice.model.model_id,
        choice.supports_images,
      )
    })
    .collect();
  let bytes = serde_json::to_vec(&(&project.project_id, &project.folder_path, provider, runtime, identities))
    .map_err(|error| error.to_string())?;
  Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn validate_selection(request: &CreateProjectSessionRequest, catalog: &SessionCreationModels) -> Result<(), String> {
  let Some(selection) = &request.model else { return Ok(()) };
  if catalog.catalog_revision.as_deref() != Some(&selection.catalog_revision)
    || catalog.runtime_profile_id != selection.runtime_profile_id
    || catalog.runtime_revision != selection.runtime_revision
  {
    return Err("The available models or runtime changed. Choose a model again, or use the runtime default.".into());
  }
  let choice = catalog
    .models
    .iter()
    .find(|choice| {
      choice.model.model_id == selection.model_id && choice.model.model_provider == selection.model_provider
    })
    .ok_or("The selected model is no longer available. Choose a model again, or use the runtime default.")?;
  if !request.image_ids.is_empty() && !choice.supports_images {
    return Err("The selected model does not support images. Keep the draft and choose an image-capable model.".into());
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::workspace::SessionCreationModelSelection;
  use tokn_interaction_core::ModelIdentity;

  fn choice(id: &str) -> ModelChoice {
    ModelChoice {
      model: ModelIdentity {
        model_provider: None,
        model_id: id.into(),
        display_name: None,
      },
      supports_images: false,
    }
  }

  #[test]
  fn catalog_rejects_ambiguous_or_unbounded_identities() {
    assert!(normalize_models(vec![choice("same"), choice("same")]).is_err());
    assert!(normalize_models(vec![choice("bad\nmodel")]).is_err());
    assert!(normalize_models(vec![choice("a"); MAX_MODELS + 1]).is_err());
    assert_eq!(
      normalize_models(vec![choice("b"), choice("a")]).unwrap()[0]
        .model
        .model_id,
      "a"
    );
  }

  #[test]
  fn image_and_runtime_validation_happens_before_admission() {
    let mut request = CreateProjectSessionRequest {
      request_id: uuid::Uuid::new_v4().to_string(),
      project_id: "project".into(),
      prompt: "hello".into(),
      image_ids: vec!["image".into()],
      provider: LiveProvider::Codex,
      runtime_profile_id: None,
      runtime_revision: None,
      model: Some(SessionCreationModelSelection {
        model_provider: None,
        model_id: "text-only".into(),
        catalog_revision: "a".repeat(64),
        runtime_profile_id: Some("runtime".into()),
        runtime_revision: Some(1),
      }),
    };
    let catalog = SessionCreationModels {
      models: vec![choice("text-only")],
      catalog_revision: Some("a".repeat(64)),
      runtime_profile_id: Some("runtime".into()),
      runtime_revision: Some(1),
      reason: None,
    };
    assert!(validate_selection(&request, &catalog).unwrap_err().contains("images"));
    request.image_ids.clear();
    assert!(validate_selection(&request, &catalog).is_ok());
    request.model.as_mut().unwrap().runtime_revision = Some(2);
    assert!(validate_selection(&request, &catalog).is_err());
  }
}
