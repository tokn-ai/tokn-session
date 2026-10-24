//! Explicit-input workflow over saved host runtime configuration. Inspection
//! never calls this activation path.
use super::*;
use std::path::Path;

impl RpcRuntimeManager {
  /// Host configuration and executable metadata only: no process, network, or
  /// native session is activated by checking whether Send can connect.
  pub(crate) async fn activation_ready(&self, provider: Provider, cwd: &Path) -> Result<(), String> {
    self.ensure_open()?;
    if provider == Provider::Pi {
      return Err("Pi continuation requires its verified owned-session runtime".into());
    }
    if !cwd.is_absolute() {
      return Err("This session has no verified working directory for RPC activation".into());
    }
    let profiles = self.profiles()?;
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || {
      let snapshot = profiles
        .snapshot()
        .map_err(|_| "Could not read runtime configuration on this machine")?;
      if let Some(profile) = select_profile(&snapshot, provider, &cwd)? {
        profiles
          .resolve_at_revision(&profile.profile_id, snapshot.revision)?
          .validate_launch_ready()
      } else {
        builtin_codex(&cwd).map(|_| ())
      }
    })
    .await
    .map_err(|_| "Could not check runtime readiness on this machine")?
  }

  /// Called only after the viewer has verified a historical session and an
  /// explicit Send. This prepares a target without delivering or replaying input.
  pub(crate) async fn ensure_session(
    &self,
    request_id: &str,
    session_key: &str,
    provider: Provider,
    native_id: &str,
    cwd: &Path,
  ) -> Result<RpcSessionBinding, String> {
    validate_uuid(request_id)?;
    if session_key.is_empty()
      || session_key.len() > 8192
      || native_id.is_empty()
      || native_id.len() > 512
      || native_id.trim() != native_id
      || native_id.chars().any(char::is_control)
      || !cwd.is_absolute()
    {
      return Err("Invalid verified session target".into());
    }
    let fingerprint = Sha256::digest(
      serde_json::to_vec(&(session_key, provider, native_id, cwd)).map_err(|_| "Invalid verified session target")?,
    )
    .into();
    let manager = self.clone();
    let key = session_key.to_owned();
    let native_id = native_id.to_owned();
    let cwd = cwd.to_path_buf();
    match self
      .action(request_id, "ensure_session", fingerprint, async move {
        manager
          .ensure_session_inner(key, provider, native_id, cwd)
          .await
          .map(ActionReply::Bound)
      })
      .await?
    {
      ActionReply::Bound(binding) => {
        self
          .check_binding(&binding, Some(provider))
          .map_err(|error| error.to_string())?;
        Ok(binding)
      }
      _ => Err("Request ID belongs to another runtime action".into()),
    }
  }

  async fn ensure_session_inner(
    &self,
    key: String,
    provider: Provider,
    native_id: String,
    cwd: PathBuf,
  ) -> Result<RpcSessionBinding, String> {
    self.ensure_open()?;
    if let Some(binding) = self.binding(&key) {
      if binding.target.session_id.as_deref() != Some(native_id.as_str()) {
        return Err("This viewer session is bound to another native session".into());
      }
      self
        .check_binding(&binding, Some(provider))
        .map_err(|error| error.to_string())?;
      return Ok(binding);
    }
    // Pi history resumes require its verified session-file path and existing
    // owned-session supervisor. An ID alone must never start a new Pi history.
    if provider == Provider::Pi {
      return Err("Continue this Pi session through its verified owned-session runtime".into());
    }
    let profiles = self.profiles()?;
    let selected = tokio::task::spawn_blocking(move || {
      let snapshot = profiles.snapshot()?;
      let profile = select_profile(&snapshot, provider, &cwd)?;
      let Some(profile) = profile else {
        return Ok((None, snapshot.revision, cwd));
      };
      Ok::<_, String>((Some(profile.profile_id.clone()), snapshot.revision, cwd))
    })
    .await
    .map_err(|_| "Could not select a runtime profile")??;
    let (profile_id, revision) = if let Some(profile_id) = selected.0 {
      (profile_id, selected.1)
    } else {
      let profile = tokio::task::spawn_blocking(move || builtin_codex(&selected.2))
        .await
        .map_err(|_| "Could not discover the local Codex executable")??;
      let profile_id = profile.profile.profile_id.clone();
      self
        .0
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .transient_profiles
        .entry(profile_id.clone())
        .or_insert(profile);
      (profile_id, 0)
    };
    let info = self
      .start_inner(RpcStartRuntimeRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        profile_id: profile_id.clone(),
        expected_revision: revision,
      })
      .await?;
    self
      .bind_inner(key, profile_id, info.instance_id, native_id, provider)
      .await
  }

  fn check_binding(&self, binding: &RpcSessionBinding, provider: Option<Provider>) -> Result<(), InteractionError> {
    self.ensure_open().map_err(InteractionError::unavailable)?;
    let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
    let current = state.bindings.get(&binding.session_key);
    let runtime = state.runtimes.get(&binding.target.runtime_id);
    if !current.is_some_and(|current| current.target == binding.target && current.profile_id == binding.profile_id)
      || !runtime.is_some_and(|runtime| {
        runtime.info.instance_id == binding.target.instance_id
          && runtime.info.status == "connected"
          && provider.is_none_or(|provider| provider == runtime.info.provider)
      })
    {
      return Err(InteractionError::new(
        ErrorKind::StaleTarget,
        "The session runtime changed; refresh its state before sending",
      ));
    }
    Ok(())
  }

  pub(crate) async fn execute_binding(
    &self,
    binding: &RpcSessionBinding,
    request_id: String,
    command: Command,
  ) -> Result<Reply, InteractionError> {
    self.check_binding(binding, None)?;
    self
      .0
      .interactions
      .execute(InteractionRequest {
        request_id,
        target: binding.target.clone(),
        command,
      })
      .await
      .map_err(|error| {
        InteractionError::new(
          error.kind,
          match error.kind {
            ErrorKind::Uncertain => "RPC delivery was not confirmed; inspect the session before retrying",
            ErrorKind::StaleTarget => "The native session or runtime changed; refresh before retrying",
            ErrorKind::Unsupported => "The selected runtime does not support this operation or delivery mode",
            ErrorKind::Conflict => "The native runtime is busy or this request conflicts with an earlier admission",
            ErrorKind::InvalidRequest => "The runtime request is invalid",
            ErrorKind::Rejected => "The native agent rejected this request; check its configuration on the host",
            ErrorKind::Unavailable => "The native runtime is unavailable; inspect its status on the host",
          },
        )
      })
  }

  /// Cached runtime state only. A missing binding never creates or resumes work.
  pub(crate) fn binding_snapshot(&self, key: &str) -> Option<RuntimeSnapshot> {
    let binding = self.binding(key)?;
    self.check_binding(&binding, None).ok()?;
    self.0.interactions.snapshot(&binding.target.runtime_id)
  }

  /// Explicitly relinquish a verified idle binding without stopping a shared
  /// provider server. Failure or uncertain state preserves the ownership lease.
  pub(crate) async fn release_binding(&self, key: &str) -> Result<(), String> {
    self.ensure_open()?;
    let Some(binding) = self.binding(key) else {
      return Ok(());
    };
    let provider = {
      let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      state
        .runtimes
        .get(&binding.target.runtime_id)
        .ok_or("Runtime is unavailable")?
        .info
        .provider
    };
    let native_id = binding
      .target
      .session_id
      .as_deref()
      .ok_or("Session identity is unavailable")?;
    let owner_key = format!("{provider:?}:{native_id}");
    let gate = self.gate(&format!("session:{owner_key}"));
    let _guard = gate.lock().await;
    self
      .check_binding(&binding, Some(provider))
      .map_err(|error| error.to_string())?;
    let state = self
      .execute_binding(&binding, uuid::Uuid::new_v4().to_string(), Command::State)
      .await
      .map_err(|_| "Could not verify that the session is idle")?;
    if !matches!(
      state,
      Reply::State(SessionState {
        status: ExecutionState::Idle,
        pending_inputs: None | Some(0),
        ..
      })
    ) {
      return Err("Wait for the session to finish before changing its runtime".into());
    }
    let pending = self
      .execute_binding(&binding, uuid::Uuid::new_v4().to_string(), Command::PendingInteractions)
      .await
      .map_err(|_| "Could not verify pending session input")?;
    if !matches!(pending,Reply::Interactions(ref interactions) if interactions.is_empty()) {
      return Err("Answer pending session input before changing its runtime".into());
    }
    let recovered = self.binding_snapshot(key).ok_or("Runtime state became unavailable")?;
    if recovered.recovery_required
      || recovered
        .pending_interactions
        .iter()
        .any(|interaction| interaction.session_id == binding.target.session_id)
      || !recovered.states.iter().any(|state| {
        state.session_id == binding.target.session_id
          && state.status == ExecutionState::Idle
          && state.pending_inputs.is_none_or(|count| count == 0)
      })
    {
      return Err("Session activity changed while releasing its runtime; refresh before retrying".into());
    }
    self
      .check_binding(&binding, Some(provider))
      .map_err(|error| error.to_string())?;
    let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
    // A disconnect can retire the binding between the previous read and this
    // lock. Never release its reserved lease after that loss of authority.
    if !state
      .bindings
      .get(key)
      .is_some_and(|current| current.target == binding.target)
      || !state.runtimes.get(&binding.target.runtime_id).is_some_and(|runtime| {
        runtime.info.instance_id == binding.target.instance_id && runtime.info.status == "connected"
      })
    {
      return Err("Runtime ownership changed while releasing the session; keep its lease until cleanup".into());
    }
    state.bindings.remove(key);
    if !state.bindings.values().any(|other| other.target == binding.target) {
      state.binding_owners.remove(&owner_key);
    }
    drop(state);
    self.changed();
    Ok(())
  }
}

fn select_profile<'a>(
  snapshot: &'a RpcRuntimeProfilesSnapshot,
  provider: Provider,
  cwd: &Path,
) -> Result<Option<&'a RpcRuntimeProfile>, String> {
  let suitable = snapshot
    .profiles
    .iter()
    .filter(|profile| {
      profile.provider == provider
        && (provider != Provider::OpenCode
          || profile
            .working_directory
            .as_ref()
            .is_some_and(|directory| Path::new(directory) == cwd))
    })
    .collect::<Vec<_>>();
  if let Some(profile) = suitable.iter().find(|profile| profile.is_default) {
    return Ok(Some(*profile));
  }
  if suitable.len() == 1 {
    return Ok(suitable.first().copied());
  }
  if !suitable.is_empty() {
    return Err("Choose a default RPC runtime profile for this provider before sending".into());
  }
  if provider == Provider::Codex {
    return Ok(None);
  }
  Err("No saved RPC runtime profile can continue this session; configure one on the selected machine".into())
}

fn builtin_codex(cwd: &Path) -> Result<RpcResolvedProfile, String> {
  let cwd = std::fs::canonicalize(cwd).map_err(|_| "The session working directory is unavailable")?;
  if !cwd.is_dir() {
    return Err("The session working directory is unavailable".into());
  }
  let configured = std::env::var_os("TOKN_SESSION_CODEX_EXECUTABLE");
  let executable = if let Some(executable) = configured {
    let path = PathBuf::from(executable);
    if path.is_absolute() {
      Some(path)
    } else {
      std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
          .map(|directory| directory.join(&path))
          .find(|candidate| candidate.is_file())
      })
    }
  } else {
    std::env::var_os("PATH").and_then(|paths| {
      std::env::split_paths(&paths)
        .map(|directory| directory.join(if cfg!(windows) { "codex.exe" } else { "codex" }))
        .find(|candidate| candidate.is_file())
    })
  }
  .ok_or("Codex is unavailable; install it or configure a runtime profile on this machine")?;
  let executable = std::fs::canonicalize(executable).map_err(|_| "The configured Codex executable is unavailable")?;
  let profile = RpcResolvedProfile {
    revision: 0,
    profile: RpcRuntimeProfile {
      profile_id: "ce427f65-1584-4b36-a1f1-194e0f45550a".into(),
      name: "Local Codex".into(),
      provider: Provider::Codex,
      mode: RpcRuntimeMode::Managed,
      executable: Some(executable.to_string_lossy().into_owned()),
      arguments: Vec::new(),
      working_directory: Some(cwd.to_string_lossy().into_owned()),
      endpoint: None,
      environment_keys: Vec::new(),
      credential_kind: None,
      is_default: false,
    },
    environment: BTreeMap::new(),
    credentials: RpcCredentials::None,
  };
  profile.validate_launch_ready()?;
  Ok(profile)
}
