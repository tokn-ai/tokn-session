//! Sessionless, bounded catalog discovery. Temporary runtimes are never bound
//! or registered and are always closed before their profile lease is released.
use super::*;
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_MODELS: usize = 512;
const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;

pub(crate) fn creation_model_identity(model: &ModelIdentity) -> ModelIdentity {
  ModelIdentity {
    model_provider: model.model_provider.clone(),
    model_id: model.model_id.clone(),
    display_name: None,
  }
}

pub(crate) fn validate_creation_model(model: &ModelIdentity) -> Result<(), InteractionError> {
  let valid = |value: &str| {
    !value.is_empty() && value.trim() == value && value.len() <= 512 && !value.chars().any(char::is_control)
  };
  if !valid(&model.model_id) || model.model_provider.as_deref().is_some_and(|provider| !valid(provider)) {
    return Err(InteractionError::invalid("Invalid creation model identity"));
  }
  Ok(())
}

pub(super) async fn query_models(adapter: &dyn InteractionAdapter) -> Result<Vec<ModelChoice>, InteractionError> {
  let descriptor = adapter.descriptor();
  if !descriptor.capabilities.supports(Operation::Models) {
    return Err(InteractionError::unsupported(
      "This runtime does not expose a model catalog",
    ));
  }
  let reply = tokio::time::timeout(
    PROBE_TIMEOUT,
    adapter.execute(InteractionRequest {
      request_id: uuid::Uuid::new_v4().to_string(),
      target: SessionTarget {
        runtime_id: descriptor.runtime_id,
        instance_id: descriptor.instance_id,
        session_id: None,
      },
      command: Command::Models,
    }),
  )
  .await
  .map_err(|_| InteractionError::unavailable("Model catalog request timed out"))?
  .map_err(|_| InteractionError::unavailable("Could not read the runtime model catalog"))?;
  let Reply::Models(models) = reply else {
    return Err(InteractionError::unavailable(
      "Runtime returned an invalid model catalog",
    ));
  };
  let mut identities = std::collections::BTreeSet::new();
  if models.len() > MAX_MODELS
    || models.iter().any(|choice| {
      validate_creation_model(&choice.model).is_err()
        || choice
          .model
          .display_name
          .as_ref()
          .is_some_and(|label| label.len() > 1024)
        || !identities.insert((choice.model.model_provider.as_deref(), choice.model.model_id.as_str()))
    })
    || serde_json::to_vec(&models).map_or(true, |bytes| bytes.len() > MAX_CATALOG_BYTES)
  {
    return Err(InteractionError::unavailable(
      "Runtime model catalog exceeds supported bounds",
    ));
  }
  Ok(models)
}

impl<H: RuntimeHost> RuntimeCoordinator<H> {
  /// May launch a short-lived metadata process, but never creates or resumes a
  /// native session. Caller cancellation does not interrupt owned cleanup.
  pub async fn creation_models(
    &self,
    provider: Provider,
    working_directory: PathBuf,
    selection: Option<RuntimeSelection>,
  ) -> Result<Vec<ModelChoice>, InteractionError> {
    if !working_directory.is_absolute() {
      return Err(InteractionError::invalid("Invalid model catalog working directory"));
    }
    {
      let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      self.ensure_open().map_err(InteractionError::unavailable)?;
      if state.active_actions >= 64 {
        return Err(InteractionError::unavailable(
          "Too many runtime operations; retry shortly",
        ));
      }
      state.active_actions += 1;
    }
    let manager = self.clone();
    tokio::spawn(async move {
      let _active = ActiveAction {
        inner: manager.0.clone(),
        id: String::new(),
        complete: true,
      };
      manager
        .creation_models_inner(provider, working_directory, selection)
        .await
    })
    .await
    .map_err(|_| InteractionError::unavailable("Model catalog discovery was interrupted"))?
  }

  async fn creation_models_inner(
    &self,
    provider: Provider,
    working_directory: PathBuf,
    selection: Option<RuntimeSelection>,
  ) -> Result<Vec<ModelChoice>, InteractionError> {
    let (profile_id, revision) = match selection {
      Some(selection) => {
        validate_uuid(&selection.profile_id).map_err(InteractionError::invalid)?;
        (selection.profile_id, selection.expected_revision)
      }
      None => tokio::time::timeout(PROBE_TIMEOUT, self.0.host.select_profile(provider, &working_directory))
        .await
        .map_err(|_| InteractionError::unavailable("Model runtime selection timed out"))?
        .map_err(InteractionError::unavailable)?,
    };
    let gate = self.gate(&profile_id);
    let _guard = tokio::time::timeout(PROBE_TIMEOUT, gate.lock())
      .await
      .map_err(|_| InteractionError::unavailable("The selected runtime is busy; retry model discovery"))?;
    self.ensure_open().map_err(InteractionError::unavailable)?;
    let manager = self.clone();
    let id = profile_id.clone();
    let resolved = tokio::task::spawn_blocking(move || {
      let profile = manager.0.host.resolve_profile(&id, revision)?;
      manager.0.host.validate_launch_ready(&profile)?;
      Ok::<_, String>(profile)
    })
    .await
    .map_err(|_| InteractionError::unavailable("Could not read the model runtime profile"))?
    .map_err(InteractionError::unavailable)?;
    let descriptor = self.0.host.describe(&resolved);
    if descriptor.provider != provider {
      return Err(InteractionError::invalid(
        "The selected runtime belongs to another provider",
      ));
    }
    let runtime_id = format!("rpc:{profile_id}");
    let existing = {
      let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      if state.quarantined.contains_key(&runtime_id) {
        return Err(InteractionError::unavailable(
          "Runtime cleanup was not confirmed; this profile remains reserved",
        ));
      }
      match state.runtimes.get(&runtime_id) {
        Some(record) if record.info.status == "connected" && record.info.profile_revision == revision => {
          Some(record.handle.clone())
        }
        Some(_) => {
          return Err(InteractionError::unavailable(
            "Runtime configuration or connection changed; reconnect before loading models",
          ));
        }
        None => {
          if state.runtimes.len() + state.quarantined.len() >= 32 {
            return Err(InteractionError::unavailable(
              "Runtime limit reached; stop an existing runtime",
            ));
          }
          None
        }
      }
    };
    if let Some(handle) = existing {
      return query_models(handle.adapter().as_ref()).await;
    }
    let lease = if let Some(directory) = &self.0.lease_directory {
      let path = directory.join(format!("{profile_id}.lock"));
      Some(
        tokio::task::spawn_blocking(move || tokn_session_storage::acquire_private_lock(&path))
          .await
          .map_err(|_| InteractionError::unavailable("Could not reserve model runtime ownership"))?
          .map_err(InteractionError::unavailable)?,
      )
    } else {
      None
    };
    // Configuration deletion uses the same cross-process lease. Recheck after
    // acquiring it so a stale profile can never be launched by this probe.
    let manager = self.clone();
    tokio::task::spawn_blocking(move || manager.0.host.resolve_profile(&profile_id, revision).map(|_| ()))
      .await
      .map_err(|_| InteractionError::unavailable("Could not recheck model runtime configuration"))?
      .map_err(InteractionError::unavailable)?;
    self.ensure_open().map_err(InteractionError::unavailable)?;
    let handle = match tokio::time::timeout(PROBE_TIMEOUT, self.0.host.open(&runtime_id, &resolved)).await {
      Ok(Ok(handle)) => handle,
      failed => {
        if descriptor.ownership == Ownership::Managed {
          self.retain_unconfirmed_start(None, lease, &resolved);
        }
        let mut reason = match failed {
          Err(_) => "Model runtime startup timed out".into(),
          Ok(Err(error)) => self.0.host.redact_error(&error, &resolved),
          Ok(Ok(_)) => unreachable!(),
        };
        if descriptor.ownership == Ownership::Managed {
          reason.push_str(" Startup cleanup was not confirmed; this profile remains reserved until host shutdown.");
        }
        return Err(InteractionError::unavailable(reason));
      }
    };
    let adapter = handle.adapter();
    let result = if self.is_stopping() {
      Err(InteractionError::unavailable("Runtime manager is shutting down"))
    } else {
      query_models(adapter.as_ref()).await
    };
    match tokio::time::timeout(CLEANUP_TIMEOUT, adapter.close()).await {
      Ok(Ok(())) => result,
      _ => {
        self.retain_unconfirmed_start(Some(handle), lease, &resolved);
        Err(InteractionError::unavailable(
          "Model discovery cleanup was not confirmed; this runtime profile remains reserved",
        ))
      }
    }
  }
}
