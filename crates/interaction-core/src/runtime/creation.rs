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
  ) -> Result<CreatedNativeSession, InteractionError> {
    if !working_directory.is_absolute() || native_id.as_deref().is_some_and(|id| !valid_native_id(id)) {
      return Err(InteractionError::invalid("Invalid native creation target"));
    }
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
        .create_native_inner(provider, working_directory, selection, native_id)
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
    let descriptor = tokio::task::spawn_blocking(move || {
      let resolved = manager.0.host.resolve_profile(&selected_id, revision)?;
      manager.0.host.validate_launch_ready(&resolved)?;
      Ok::<_, String>(manager.0.host.describe(&resolved))
    })
    .await
    .map_err(|_| InteractionError::unavailable("Could not read the creation runtime profile"))?
    .map_err(InteractionError::unavailable)?;
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
    let native = handle
      .create_native(&CreateNativeSessionRequest {
        working_directory: working_directory.clone(),
        native_id: native_id.clone(),
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
    Ok(CreatedNativeSession {
      provider,
      profile_id,
      target: SessionTarget {
        runtime_id: info.runtime_id,
        instance_id: info.instance_id,
        session_id: Some(native.native_id.clone()),
      },
      native,
      capabilities: info.capabilities,
    })
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

  /// Recover native state before first input, without a resume/create call.
  pub async fn execute_created(
    &self,
    created: &CreatedNativeSession,
    request_id: String,
    command: Command,
  ) -> Result<Reply, InteractionError> {
    self.check_created(created)?;
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
    }
    self.check_created(created)?;
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
    if key.is_empty() || key.len() > 8192 {
      return Err(InteractionError::invalid("Invalid created-session key"));
    }
    self.check_created(created)?;
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
    if let Some(existing) = state.bindings.get(&key) {
      if existing.target != binding.target {
        return Err(InteractionError::new(
          ErrorKind::Conflict,
          "This history key already has another runtime binding",
        ));
      }
      return Ok(existing.clone());
    }
    if state.bindings.len() >= 1024 {
      return Err(InteractionError::unavailable("Runtime session-binding limit reached"));
    }
    state.bindings.insert(key, binding.clone());
    drop(state);
    self.changed();
    Ok(binding)
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

fn valid_native_id(id: &str) -> bool {
  !id.is_empty() && id.len() <= 512 && id.trim() == id && !id.chars().any(char::is_control)
}
