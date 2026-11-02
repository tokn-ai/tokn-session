//! Explicit native creation and adoption without an implicit native resume.
use super::*;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeSelection {
  pub profile_id: String,
  pub expected_revision: u64,
}

#[derive(Clone, Debug)]
pub struct CreateNativeSessionRequest {
  pub working_directory: PathBuf,
  /// Provider-supported, host-planned identity. Never substitute it for a
  /// different identity returned by the native server.
  pub native_id: Option<String>,
  pub model: Option<ModelIdentity>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct NativeSessionDescriptor {
  pub native_id: String,
  pub working_directory: PathBuf,
  pub history_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CreatedNativeSession {
  pub provider: Provider,
  pub profile_id: String,
  pub target: SessionTarget,
  pub native: NativeSessionDescriptor,
  pub capabilities: Capabilities,
}

impl<H: RuntimeHost> RuntimeCoordinator<H> {
  /// The caller owns durable creation admission. This operation is detached
  /// from caller cancellation and counted while shutdown waits for native work.
  pub async fn create_native(
    &self,
    provider: Provider,
    working_directory: PathBuf,
    selection: Option<RuntimeSelection>,
    native_id: Option<String>,
    model: Option<ModelIdentity>,
  ) -> Result<CreatedNativeSession, InteractionError> {
    if !working_directory.is_absolute() || native_id.as_deref().is_some_and(|id| !valid_native_id(id)) {
      return Err(InteractionError::invalid("Invalid native creation target"));
    }
    if let Some(model) = &model {
      validate_creation_model(model)?;
    }
    let model = model.as_ref().map(creation_model_identity);
    {
      let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      self.ensure_open().map_err(InteractionError::unavailable)?;
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
        .create_native_inner(provider, working_directory, selection, native_id, model)
        .await
    })
    .await
    .map_err(|_| InteractionError::uncertain("Native creation was interrupted; inspect its receipt before retrying"))?
  }

  async fn create_native_inner(
    &self,
    provider: Provider,
    working_directory: PathBuf,
    selection: Option<RuntimeSelection>,
    native_id: Option<String>,
    model: Option<ModelIdentity>,
  ) -> Result<CreatedNativeSession, InteractionError> {
    let (profile_id, revision) = match selection {
      Some(selection) => {
        validate_uuid(&selection.profile_id).map_err(InteractionError::invalid)?;
        (selection.profile_id, selection.expected_revision)
      }
      None => self
        .0
        .host
        .select_profile(provider, &working_directory)
        .await
        .map_err(InteractionError::unavailable)?,
    };
    // Validate provider before starting any process from an explicitly selected
    // profile. Runtime startup itself never creates a provider session.
    let manager = self.clone();
    let selected_id = profile_id.clone();
    let selected_model = model.clone();
    let descriptor = tokio::task::spawn_blocking(move || {
      let resolved = manager
        .0
        .host
        .resolve_profile(&selected_id, revision)
        .map_err(InteractionError::unavailable)?;
      manager
        .0
        .host
        .validate_launch_ready(&resolved)
        .map_err(InteractionError::unavailable)?;
      if let Some(model) = &selected_model {
        manager.0.host.validate_creation_model(&resolved, model)?;
      }
      Ok::<_, InteractionError>(manager.0.host.describe(&resolved))
    })
    .await
    .map_err(|_| InteractionError::unavailable("Could not read the creation runtime profile"))??;
    if descriptor.provider != provider {
      return Err(InteractionError::invalid(
        "The selected runtime belongs to another provider",
      ));
    }
    let info = self
      .start_inner(StartRuntimeRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        profile_id: profile_id.clone(),
        expected_revision: revision,
      })
      .await
      .map_err(InteractionError::unavailable)?;
    if let Some(id) = &native_id {
      self.reserve_created_owner(provider, id, &info).await?;
    }
    let gate = self.gate(&profile_id);
    let guard = gate.lock().await;
    self.ensure_open().map_err(InteractionError::unavailable)?;
    let handle = {
      let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      state
        .runtimes
        .get(&info.runtime_id)
        .filter(|record| record.info.instance_id == info.instance_id && record.info.status == "connected")
        .map(|record| record.handle.clone())
        .ok_or_else(|| InteractionError::unavailable("The creation runtime disconnected"))?
    };
    let native = handle.adapter();
    if let Some(model) = &model {
      let choices = models::query_models(native.as_ref()).await?;
      if !choices
        .iter()
        .any(|choice| creation_model_identity(&choice.model) == *model)
      {
        return Err(InteractionError::invalid(
          "The selected model is no longer available; refresh the model list",
        ));
      }
    }
    let native = handle
      .create_native(&CreateNativeSessionRequest {
        working_directory: working_directory.clone(),
        native_id: native_id.clone(),
        model: model.clone(),
      })
      .await?;
    drop(guard);
    if !valid_native_id(&native.native_id) {
      return Err(InteractionError::uncertain(
        "Native creation returned an invalid session identity",
      ));
    }
    self
      .reserve_created_owner(provider, &native.native_id, &info)
      .await
      .map_err(|_| InteractionError::uncertain("Native session was created but its ownership could not be reserved"))?;
    if native.working_directory != working_directory
      || native_id.as_ref().is_some_and(|id| id != &native.native_id)
      || native.history_path.as_ref().is_some_and(|path| !path.is_absolute())
    {
      return Err(InteractionError::uncertain(
        "Native creation returned a different session identity or working directory",
      ));
    }
    let created = CreatedNativeSession {
      provider,
      profile_id,
      target: SessionTarget {
        runtime_id: info.runtime_id,
        instance_id: info.instance_id,
        session_id: Some(native.native_id.clone()),
      },
      native,
      capabilities: info.capabilities,
    };
    if let Some(model) = model {
      let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      Self::check_created_state(&state, &created)
        .map_err(|_| InteractionError::uncertain("The native session was created but its runtime ownership changed"))?;
      let key = created_model_key(&created);
      if let Some(existing) = state.created_models.get(&key) {
        if existing.model != model {
          return Err(InteractionError::uncertain(
            "The created session already belongs to another model intent",
          ));
        }
      } else {
        // Native ownership has the same bound; do not discard a completed or
        // uncertain preparation and accidentally allow another model mutation.
        if state.created_models.len() >= 1024 {
          return Err(InteractionError::uncertain(
            "The created-session model preparation limit was reached",
          ));
        }
        state.created_models.insert(
          key,
          CreatedModelPreparation {
            model,
            input_attempted: false,
            status: CreatedModelPreparationStatus::NotAttempted,
          },
        );
      }
    }
    Ok(created)
  }

  async fn reserve_created_owner(
    &self,
    provider: Provider,
    id: &str,
    info: &RuntimeInfo,
  ) -> Result<(), InteractionError> {
    let owner_key = format!("{provider:?}:{id}");
    let gate = self.gate(&format!("session:{owner_key}"));
    let _guard = gate.lock().await;
    {
      let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      if let Some(owner) = state.binding_owners.get(&owner_key) {
        return if owner.runtime_id == info.runtime_id && owner.instance_id == info.instance_id {
          Ok(())
        } else {
          Err(InteractionError::new(
            ErrorKind::Conflict,
            "This native session already has another runtime owner",
          ))
        };
      }
      if state.binding_owners.len() >= 1024 {
        return Err(InteractionError::unavailable("Runtime session-owner limit reached"));
      }
    }
    let ownership = self
      .0
      .session_ownership
      .as_ref()
      .map_err(|error| InteractionError::unavailable(error.clone()))?
      .clone();
    let native_id = id.to_owned();
    let lease = tokio::task::spawn_blocking(move || ownership.acquire(provider, &native_id))
      .await
      .map_err(|_| InteractionError::unavailable("Could not reserve native session ownership"))?
      .map_err(InteractionError::unavailable)?;
    let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
    if !state
      .runtimes
      .get(&info.runtime_id)
      .is_some_and(|record| record.info.instance_id == info.instance_id && record.info.status == "connected")
    {
      return Err(InteractionError::unavailable("The creation runtime disconnected"));
    }
    state.binding_owners.insert(
      owner_key,
      BindingOwner {
        runtime_id: info.runtime_id.clone(),
        instance_id: info.instance_id.clone(),
        _lease: lease,
      },
    );
    Ok(())
  }

  fn check_created(&self, created: &CreatedNativeSession) -> Result<(), InteractionError> {
    self.ensure_open().map_err(InteractionError::unavailable)?;
    let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
    Self::check_created_state(&state, created)
  }

  fn check_created_state(state: &State, created: &CreatedNativeSession) -> Result<(), InteractionError> {
    let owner_key = format!("{:?}:{}", created.provider, created.native.native_id);
    let valid = state.runtimes.get(&created.target.runtime_id).is_some_and(|record| {
      record.info.instance_id == created.target.instance_id
        && record.info.profile_id == created.profile_id
        && record.info.provider == created.provider
        && record.info.status == "connected"
    }) && state.binding_owners.get(&owner_key).is_some_and(|owner| {
      owner.runtime_id == created.target.runtime_id && owner.instance_id == created.target.instance_id
    }) && created.target.session_id.as_deref() == Some(created.native.native_id.as_str());
    if valid {
      Ok(())
    } else {
      Err(InteractionError::new(
        ErrorKind::StaleTarget,
        "The created session's runtime changed",
      ))
    }
  }

  /// One creation-only preparation, after durable creation and before any
  /// initial input. The caller's durable creation receipt remains authoritative
  /// across host restarts; this process-local guard also rejects concurrent or
  /// changed attempts and retains uncertain outcomes without native replay.
  pub async fn prepare_created_model(
    &self,
    created: &CreatedNativeSession,
    model: ModelIdentity,
  ) -> Result<(), InteractionError> {
    models::validate_creation_model(&model)?;
    let model = models::creation_model_identity(&model);
    let key = created_model_key(created);
    {
      self.ensure_open().map_err(InteractionError::unavailable)?;
      let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      Self::check_created_state(&state, created)?;
      if state.created_input_fences.contains(&key) {
        return Err(cancelled_created_input());
      }
      if state.bindings.values().any(|binding| binding.target == created.target) {
        return Err(InteractionError::invalid(
          "A published session cannot use creation-only model preparation",
        ));
      }
      let preparation = state
        .created_models
        .get_mut(&key)
        .ok_or_else(|| InteractionError::invalid("This session has no acknowledged creation-time model intent"))?;
      if preparation.model != model {
        return Err(InteractionError::new(
          ErrorKind::Conflict,
          "The created session belongs to another model intent",
        ));
      }
      if preparation.input_attempted {
        return Err(InteractionError::invalid(
          "Creation-only model preparation must precede the first message",
        ));
      }
      match &preparation.status {
        CreatedModelPreparationStatus::Complete(result) => return result.clone(),
        CreatedModelPreparationStatus::Pending => {
          return Err(InteractionError::uncertain(
            "Model preparation is still pending; inspect its creation receipt",
          ));
        }
        CreatedModelPreparationStatus::NotAttempted => {}
      }
      preparation.status = CreatedModelPreparationStatus::Pending;
      state.active_actions += 1;
    }
    let manager = self.clone();
    let created = created.clone();
    tokio::spawn(async move {
      let _active = ActiveAction {
        inner: manager.0.clone(),
        id: String::new(),
        complete: true,
      };
      let result = manager.prepare_created_model_inner(&created, &model).await;
      let mut state = manager.0.state.lock().unwrap_or_else(|error| error.into_inner());
      if let Some(preparation) = state.created_models.get_mut(&key) {
        preparation.status = CreatedModelPreparationStatus::Complete(result.clone());
      }
      result
    })
    .await
    .map_err(|_| InteractionError::uncertain("Model preparation was interrupted; inspect its creation receipt"))?
  }

  async fn prepare_created_model_inner(
    &self,
    created: &CreatedNativeSession,
    model: &ModelIdentity,
  ) -> Result<(), InteractionError> {
    let gate = self.gate(&created.profile_id);
    let _guard = gate.lock().await;
    self.check_created(created)?;
    let handle = {
      let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      if state.bindings.values().any(|binding| binding.target == created.target) {
        return Err(InteractionError::invalid(
          "A published session cannot use creation-only model preparation",
        ));
      }
      state.runtimes.get(&created.target.runtime_id).unwrap().handle.clone()
    };
    let state = handle
      .adapter()
      .execute(InteractionRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        target: created.target.clone(),
        command: Command::State,
      })
      .await?;
    let Reply::State(state) = state else {
      return Err(InteractionError::unavailable(
        "The created session returned invalid model-preparation state",
      ));
    };
    if state.status != ExecutionState::Idle || state.session_id.as_deref() != Some(created.native.native_id.as_str()) {
      return Err(InteractionError::invalid(
        "Creation-only model preparation requires the exact idle session",
      ));
    }
    self.check_created(created)?;
    handle.prepare_created_model(&created.native.native_id, model).await?;
    self.check_created(created)?;
    Ok(())
  }

  /// Immediately fence the automatic first message for this exact created
  /// target. This is memory-only and must run before cancellation awaits disk
  /// or native work. Later explicit input through a published binding is separate.
  pub fn fence_created_input(&self, created: &CreatedNativeSession) -> Result<(), InteractionError> {
    self.ensure_open().map_err(InteractionError::unavailable)?;
    let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
    Self::check_created_state(&state, created)?;
    let key = created_model_key(created);
    // Each fenced target owns one of the equally bounded native-session leases.
    // Never evict a fence and accidentally reenable its initial message.
    if !state.created_input_fences.contains(&key) && state.created_input_fences.len() >= 1024 {
      return Err(InteractionError::unavailable(
        "Created-session input fence limit reached",
      ));
    }
    state.created_input_fences.insert(key);
    Ok(())
  }

  fn check_created_input(&self, created: &CreatedNativeSession) -> Result<(), InteractionError> {
    self.ensure_open().map_err(InteractionError::unavailable)?;
    let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
    Self::check_created_state(&state, created)?;
    if state.created_input_fences.contains(&created_model_key(created)) {
      return Err(cancelled_created_input());
    }
    Ok(())
  }

  /// Recover native state before first input, without a resume/create call.
  pub async fn execute_created(
    &self,
    created: &CreatedNativeSession,
    request_id: String,
    command: Command,
  ) -> Result<Reply, InteractionError> {
    self.check_created(created)?;
    let message = matches!(command, Command::Message { .. });
    if message {
      self.check_created_input(created)?;
      let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      if let Some(preparation) = state.created_models.get_mut(&created_model_key(created)) {
        match &preparation.status {
          CreatedModelPreparationStatus::Pending => {
            return Err(InteractionError::uncertain(
              "The created session's model preparation is still pending",
            ));
          }
          CreatedModelPreparationStatus::Complete(Err(error)) => return Err(error.clone()),
          _ => preparation.input_attempted = true,
        }
      }
    }
    for command in [Command::State, Command::PendingInteractions] {
      self
        .0
        .interactions
        .execute(InteractionRequest {
          request_id: uuid::Uuid::new_v4().to_string(),
          target: created.target.clone(),
          command,
        })
        .await?;
      if message {
        self.check_created_input(created)?;
      }
    }
    self.check_created(created)?;
    if message {
      self.check_created_input(created)?;
    }
    self
      .0
      .interactions
      .execute(InteractionRequest {
        request_id,
        target: created.target.clone(),
        command,
      })
      .await
  }

  /// Publish an exact host-verified history key for an already created target.
  /// This preserves the runtime and never invokes `bind_native`.
  pub async fn adopt_created(
    &self,
    created: &CreatedNativeSession,
    key: String,
  ) -> Result<SessionBinding, InteractionError> {
    self
      .adopt_created_if_ready(created, key)
      .await?
      .ok_or_else(|| InteractionError::uncertain("The created session's model preparation is still pending"))
  }

  /// Publish an exact created target, deferring while its admitted model
  /// preparation is pending. Readiness and binding insertion are checked under
  /// one lock so callers cannot bypass preparation through a binding race.
  pub async fn adopt_created_if_ready(
    &self,
    created: &CreatedNativeSession,
    key: String,
  ) -> Result<Option<SessionBinding>, InteractionError> {
    if key.is_empty() || key.len() > 8192 {
      return Err(InteractionError::invalid("Invalid created-session key"));
    }
    self.ensure_open().map_err(InteractionError::unavailable)?;
    {
      let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      Self::check_created_state(&state, created)?;
      if model_preparation_pending(&state, &created.target) {
        return Ok(None);
      }
    }
    // First-input validation may have failed before recovering live state.
    // Adopt only after obtaining a fresh baseline; this never resumes native work.
    self
      .execute_created(created, uuid::Uuid::new_v4().to_string(), Command::State)
      .await?;
    let binding = SessionBinding {
      session_key: key.clone(),
      profile_id: created.profile_id.clone(),
      target: created.target.clone(),
      capabilities: created.capabilities.clone(),
    };
    let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
    Self::check_created_state(&state, created)?;
    if model_preparation_pending(&state, &created.target) {
      return Ok(None);
    }
    if let Some(existing) = state.bindings.get(&key) {
      if existing.target != binding.target {
        return Err(InteractionError::new(
          ErrorKind::Conflict,
          "This history key already has another runtime binding",
        ));
      }
      return Ok(Some(existing.clone()));
    }
    if state.bindings.len() >= 1024 {
      return Err(InteractionError::unavailable("Runtime session-binding limit reached"));
    }
    state.bindings.insert(key, binding.clone());
    drop(state);
    self.changed();
    Ok(Some(binding))
  }

  /// Reconcile a host-verified planned identity after native creation lost its
  /// acknowledgement. Existing ownership supplies the exact runtime instance;
  /// this never selects/starts a runtime, creates/resumes a session, or sends input.
  /// None permits history-only publication after that owner/runtime disappeared.
  /// A present owner whose state cannot be recovered remains an explicit error.
  pub async fn adopt_verified_creation(
    &self,
    provider: Provider,
    native_id: &str,
    working_directory: PathBuf,
    key: String,
  ) -> Result<Option<CreatedNativeSession>, InteractionError> {
    if !valid_native_id(native_id) || !working_directory.is_absolute() || key.is_empty() || key.len() > 8192 {
      return Err(InteractionError::invalid("Invalid verified creation identity"));
    }
    self.ensure_open().map_err(InteractionError::unavailable)?;
    let owner_key = format!("{provider:?}:{native_id}");
    let created = {
      let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      let Some(owner) = state.binding_owners.get(&owner_key) else {
        return Ok(None);
      };
      let Some(record) = state
        .runtimes
        .get(&owner.runtime_id)
        .filter(|record| record.info.instance_id == owner.instance_id)
      else {
        return Ok(None);
      };
      if record.info.provider != provider {
        return Err(InteractionError::new(
          ErrorKind::StaleTarget,
          "The verified session has a different runtime provider",
        ));
      }
      CreatedNativeSession {
        provider,
        profile_id: record.info.profile_id.clone(),
        target: SessionTarget {
          runtime_id: owner.runtime_id.clone(),
          instance_id: owner.instance_id.clone(),
          session_id: Some(native_id.to_owned()),
        },
        native: NativeSessionDescriptor {
          native_id: native_id.to_owned(),
          working_directory,
          history_path: None,
        },
        capabilities: record.info.capabilities.clone(),
      }
    };
    match self.adopt_created(&created, key).await {
      Ok(_) => Ok(Some(created)),
      Err(error) => {
        // Cleanup can complete during read-only recovery. Only absence permits
        // fallback; a replacement/current owner must still surface its failure.
        let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
        let present = state.binding_owners.get(&owner_key).is_some_and(|owner| {
          state
            .runtimes
            .get(&owner.runtime_id)
            .is_some_and(|record| record.info.instance_id == owner.instance_id)
        });
        if present { Err(error) } else { Ok(None) }
      }
    }
  }
}

fn created_model_key(created: &CreatedNativeSession) -> CreatedModelKey {
  (
    created.target.runtime_id.clone(),
    created.target.instance_id.clone(),
    created.native.native_id.clone(),
  )
}

fn cancelled_created_input() -> InteractionError {
  InteractionError::new(
    ErrorKind::Rejected,
    "The created session's first message was cancelled before admission",
  )
}

pub(super) fn check_model_preparation_not_pending(
  state: &State,
  target: &SessionTarget,
) -> Result<(), InteractionError> {
  if model_preparation_pending(state, target) {
    return Err(InteractionError::uncertain(
      "The created session's model preparation is still pending",
    ));
  }
  Ok(())
}

fn model_preparation_pending(state: &State, target: &SessionTarget) -> bool {
  let Some(native_id) = &target.session_id else {
    return false;
  };
  let key = (target.runtime_id.clone(), target.instance_id.clone(), native_id.clone());
  state
    .created_models
    .get(&key)
    .is_some_and(|preparation| matches!(preparation.status, CreatedModelPreparationStatus::Pending))
}

fn valid_native_id(id: &str) -> bool {
  !id.is_empty() && id.len() <= 512 && id.trim() == id && !id.chars().any(char::is_control)
}
