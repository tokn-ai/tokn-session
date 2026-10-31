//! Explicit-input activation, recovered bindings and safe ownership release.
use super::*;
use crate::runtime_plan::{RuntimePlan, VerifiedSessionDescriptor};

impl<H: RuntimeHost> RuntimeCoordinator<H> {
  /// Called only after the host has verified a historical session and an
  /// explicit Send. This prepares a target without delivering or replaying input.
  pub async fn ensure_session(
    &self,
    request_id: &str,
    session_key: &str,
    provider: Provider,
    native_id: &str,
    cwd: &Path,
  ) -> Result<SessionBinding, String> {
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
          .ensure_session_inner(key, provider, native_id, cwd, None)
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

  /// Activate only the host-resolved configuration and host-verified history.
  /// A changed default cannot redirect this explicit input to another runtime.
  pub async fn ensure_session_with_plan(
    &self,
    request_id: &str,
    session_key: &str,
    plan: RuntimePlan,
    descriptor: VerifiedSessionDescriptor,
  ) -> Result<SessionBinding, String> {
    validate_uuid(request_id)?;
    if session_key.is_empty() || session_key.len() > 8192 {
      return Err("Invalid verified session target".into());
    }
    plan.validate_resume(&descriptor)?;
    let selection = plan.shared_runtime_selection()?;
    let fingerprint = Sha256::digest(
      serde_json::to_vec(&("runtime-plan-v1", session_key, &plan, &descriptor))
        .map_err(|_| "Invalid verified session target")?,
    )
    .into();
    let manager = self.clone();
    let key = session_key.to_owned();
    let selected_plan = plan.clone();
    match self
      .action(request_id, "ensure_session", fingerprint, async move {
        manager.validate_resolved_plan(&selected_plan).await?;
        manager
          .ensure_session_inner(
            key,
            selected_plan.provider,
            descriptor.native().native_id.clone(),
            descriptor.native().working_directory.clone(),
            Some(selection),
          )
          .await
          .map(ActionReply::Bound)
      })
      .await?
    {
      ActionReply::Bound(binding) => {
        self
          .check_binding(&binding, Some(plan.provider))
          .map_err(|error| error.to_string())?;
        self.check_binding_plan(&binding, &plan)?;
        Ok(binding)
      }
      _ => Err("Request ID belongs to another runtime action".into()),
    }
  }

  async fn validate_resolved_plan(&self, plan: &RuntimePlan) -> Result<(), String> {
    let manager = self.clone();
    let plan = plan.clone();
    tokio::task::spawn_blocking(move || {
      let resolved = manager
        .0
        .host
        .resolve_profile(&plan.configuration.id, plan.configuration.revision)?;
      let descriptor = manager.0.host.describe(&resolved);
      if descriptor.profile_id != plan.configuration.id
        || descriptor.revision != plan.configuration.revision
        || descriptor.provider != plan.provider
        || descriptor.ownership != plan.ownership
      {
        return Err("Resolved runtime configuration no longer matches the selected plan".into());
      }
      Ok(())
    })
    .await
    .map_err(|_| "Could not verify the selected runtime plan")?
  }

  fn check_binding_plan(&self, binding: &SessionBinding, plan: &RuntimePlan) -> Result<(), String> {
    let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
    if binding.profile_id != plan.configuration.id
      || !state.runtimes.get(&binding.target.runtime_id).is_some_and(|runtime| {
        runtime.info.instance_id == binding.target.instance_id
          && runtime.info.profile_revision == plan.configuration.revision
          && runtime.info.ownership == plan.ownership
      })
    {
      return Err("The session is bound to a different runtime configuration; refresh before sending".into());
    }
    Ok(())
  }

  async fn ensure_session_inner(
    &self,
    key: String,
    provider: Provider,
    native_id: String,
    cwd: PathBuf,
    selection: Option<RuntimeSelection>,
  ) -> Result<SessionBinding, String> {
    self.ensure_open()?;
    if let Some(binding) = self.binding(&key) {
      if binding.target.session_id.as_deref() != Some(native_id.as_str()) {
        return Err("This session is bound to another native session".into());
      }
      self
        .check_binding(&binding, Some(provider))
        .map_err(|error| error.to_string())?;
      if let Some(selection) = &selection {
        let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
        if binding.profile_id != selection.profile_id
          || !state
            .runtimes
            .get(&binding.target.runtime_id)
            .is_some_and(|runtime| runtime.info.profile_revision == selection.expected_revision)
        {
          return Err("The session is bound to a different runtime configuration; refresh before sending".into());
        }
      }
      return Ok(binding);
    }
    let (profile_id, revision) = match selection {
      Some(selection) => (selection.profile_id, selection.expected_revision),
      None => self.0.host.select_profile(provider, &cwd).await?,
    };
    let info = self
      .start_inner(StartRuntimeRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        profile_id: profile_id.clone(),
        expected_revision: revision,
      })
      .await?;
    self
      .bind_inner(key, profile_id, info.instance_id, native_id, provider)
      .await
  }

  fn check_binding(&self, binding: &SessionBinding, provider: Option<Provider>) -> Result<(), InteractionError> {
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

  pub async fn execute_binding(
    &self,
    binding: &SessionBinding,
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
  pub fn binding_snapshot(&self, key: &str) -> Option<RuntimeSnapshot> {
    let binding = self.binding(key)?;
    self.check_binding(&binding, None).ok()?;
    self.0.interactions.snapshot(&binding.target.runtime_id)
  }

  /// Explicitly relinquish a verified idle binding without stopping a shared
  /// provider server. Failure or uncertain state preserves the ownership lease.
  pub async fn release_binding(&self, key: &str) -> Result<(), String> {
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
