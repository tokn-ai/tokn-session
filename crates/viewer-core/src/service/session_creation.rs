//! Host policy and history publication for the shared creation coordinator.
use super::*;
use crate::rpc_profiles::RpcRuntimeMode;
use crate::rpc_runtime::RpcRuntimeManager;
use crate::workspace::{
  SessionCreationOptions, SessionCreationOptionsRequest, SessionCreationProvider, SessionCreationRuntime,
};
use tokn_interaction_core::creation::{
  CreationHost, CreationManager, CreationPhase, CreationRequest, CreationSnapshot,
};
use tokn_interaction_core::runtime::{CreatedNativeSession, RuntimeSelection};
use tokn_interaction_core::{
  AdapterFuture, Command, DeliveryMode, ErrorKind, InteractionError, MessageInput, Provider as LiveProvider, Reply,
};

#[derive(Clone)]
struct HistoryPublisher {
  repository: Arc<dyn ViewerRepository>,
  index: Arc<SessionIndex>,
  gate: Arc<Mutex<()>>,
  retry: Arc<Mutex<Option<mpsc::UnboundedSender<SessionIndexWake>>>>,
}

struct Host {
  runtimes: RpcRuntimeManager,
  history: HistoryPublisher,
}

impl ViewerService {
  pub(crate) fn creation_manager(&self) -> Result<CreationManager, String> {
    self
      .sessions
      .creations
      .get_or_init(|| {
        CreationManager::new(
          self.sessions.creation_path.clone(),
          Arc::new(Host {
            runtimes: self.sessions.runtimes.clone(),
            history: HistoryPublisher {
              repository: self.repository.clone(),
              index: self.session_index.clone(),
              gate: self.index_refresh_gate.clone(),
              retry: self.index_retry_sender.clone(),
            },
          }),
        )
      })
      .clone()
  }

  pub async fn get_session_creation_options(
    &self,
    request: SessionCreationOptionsRequest,
  ) -> Result<SessionCreationOptions, String> {
    self.check_managed_workspace()?;
    let service = self.clone();
    tokio::task::spawn_blocking(move || {
      let project = service.validated_project(&request.project_id)?;
      service.creation_options(&project)
    })
    .await
    .map_err(|_| "Could not check session creation options".to_owned())?
  }

  fn creation_options(&self, project: &crate::workspace::RegisteredProject) -> Result<SessionCreationOptions, String> {
    self.sessions.creation_routes.as_ref().map_err(Clone::clone)?;
    let profiles = self.sessions.runtimes.profiles()?;
    let saved = profiles.snapshot()?;
    let cwd = Path::new(&project.folder_path);
    let mut providers = Vec::new();
    for provider in [
      LiveProvider::Pi,
      LiveProvider::Codex,
      LiveProvider::OpenCode,
      LiveProvider::Dsh,
    ] {
      if provider == LiveProvider::Pi {
        let ready = self.sessions.managed.available();
        providers.push(SessionCreationProvider {
          provider,
          available: ready.is_ok(),
          reason: ready.err(),
          supports_images: true,
          default_profile_id: None,
          runtimes: Vec::new(),
        });
        continue;
      }
      let mut runtimes = Vec::new();
      for profile in saved.profiles.iter().filter(|profile| profile.provider == provider) {
        let ready = profiles
          .resolve_at_revision(&profile.profile_id, saved.revision)
          .and_then(|resolved| {
            resolved.validate_launch_ready()?;
            validate_creation_profile(&resolved, cwd)
          });
        runtimes.push(SessionCreationRuntime {
          profile_id: profile.profile_id.clone(),
          revision: saved.revision,
          name: profile.name.clone(),
          available: ready.is_ok(),
          reason: ready.err(),
        });
      }
      let selection = self.sessions.runtimes.creation_selection(provider, cwd, None);
      let default_profile_id = selection
        .as_ref()
        .ok()
        .and_then(|value| value.as_ref())
        .map(|selection| selection.profile_id.clone());
      let available = selection.is_ok();
      let reason = selection.err();
      providers.push(SessionCreationProvider {
        provider,
        available,
        reason,
        supports_images: provider != LiveProvider::Dsh,
        default_profile_id,
        runtimes,
      });
    }
    Ok(SessionCreationOptions {
      project_id: project.project_id.clone(),
      providers,
    })
  }

  pub(crate) fn creation_snapshot(
    &self,
    receipt: CreationSnapshot,
  ) -> Result<crate::workspace::ManagedSessionSnapshot, String> {
    let session = receipt
      .session_key
      .as_deref()
      .map(|key| self.created_summary(key))
      .transpose()?
      .flatten();
    let live = receipt
      .session_key
      .as_deref()
      .and_then(|key| self.sessions.runtimes.binding_snapshot(key));
    let running = live.as_ref().is_some_and(|snapshot| {
      snapshot.states.iter().any(|state| {
        state.session_id.as_deref()
          == receipt
            .created
            .as_ref()
            .map(|created| created.native.native_id.as_str())
          && matches!(
            state.status,
            tokn_interaction_core::ExecutionState::Running | tokn_interaction_core::ExecutionState::Stopping
          )
      })
    });
    Ok(crate::workspace::ManagedSessionSnapshot {
      revision: receipt.revision,
      request_id: receipt.request_id,
      project_id: receipt.project_id,
      provider: viewer_provider(receipt.provider).as_str().into(),
      status: match receipt.phase {
        CreationPhase::Failed => crate::workspace::ManagedSessionStatus::Failed,
        CreationPhase::Ready if running => crate::workspace::ManagedSessionStatus::Running,
        CreationPhase::Ready => crate::workspace::ManagedSessionStatus::Idle,
        _ => crate::workspace::ManagedSessionStatus::Starting,
      },
      session_id: receipt
        .created
        .as_ref()
        .map(|created| created.native.native_id.clone())
        .or(receipt.planned_native_id),
      session_key: receipt.session_key,
      session,
      message: receipt.message.or_else(|| match receipt.phase {
        CreationPhase::Admitted => Some("Preparing the selected agent runtime…".into()),
        CreationPhase::NativeKnown => Some("Session created. Sending the first message…".into()),
        CreationPhase::HistoryPending => {
          Some("Waiting for the saved conversation. Check this session again to refresh its history.".into())
        }
        _ => None,
      }),
      last_turn_error: None,
      pending_dialog: None,
      creation_phase: Some(receipt.phase),
      creation_input_state: Some(receipt.first_input),
    })
  }

  fn created_summary(&self, key: &str) -> Result<Option<SessionSummary>, String> {
    let locator = decode_session_key(key)?;
    let Some(indexed) = self
      .session_index
      .session(&index_session_key(&locator)?)
      .map_err(|error| error.to_string())?
    else {
      return Ok(None);
    };
    if !indexed.present {
      return Ok(None);
    }
    let mut summary = session_summary_with_child_count(
      locator.provider,
      indexed_session_header(&indexed)?,
      0,
      false,
      SessionAttention::from_index(&indexed),
    )?;
    let groups = self
      .session_index
      .project_groups(&indexed.cwd.into_iter().collect::<Vec<_>>(), current_time_ms())
      .map_err(|error| error.to_string())?;
    apply_project_group(&mut summary, &groups);
    Ok(Some(summary))
  }

  pub(super) fn rpc_creation(
    &self,
    request: &crate::workspace::ManagedSessionRequest,
  ) -> Result<Option<CreationSnapshot>, String> {
    if request.request_id.is_some() == request.session_key.is_some() {
      return Err("Supply exactly one creation request ID or session key".into());
    }
    if self.sessions.managed.get(request)?.is_some() {
      return Ok(None);
    }
    let manager = self.creation_manager()?;
    if let Some(id) = &request.request_id {
      return manager.get(id);
    }
    let key = request.session_key.as_deref().unwrap();
    let locator = decode_session_key(key)?;
    let canonical = encode_session_key(&locator)?;
    Ok(
      manager
        .list()
        .into_iter()
        .find(|receipt| receipt.session_key.as_deref() == Some(&canonical)),
    )
  }

  pub(super) async fn claim_creation_route(
    &self,
    request: &crate::workspace::CreateProjectSessionRequest,
  ) -> Result<(), String> {
    let routes = self.sessions.creation_routes.as_ref().map_err(Clone::clone)?.clone();
    let id = request.request_id.clone();
    let provider = request.provider;
    tokio::task::spawn_blocking(move || routes.claim(&id, provider))
      .await
      .map_err(|_| "Could not verify creation request ownership".to_owned())?
  }

  pub(super) async fn existing_rpc_creation(
    &self,
    request: &crate::workspace::CreateProjectSessionRequest,
  ) -> Result<Option<crate::workspace::ManagedSessionSnapshot>, crate::workspace::SessionCreationError> {
    use crate::workspace::SessionCreationError;
    let manager = self.creation_manager().map_err(SessionCreationError::not_started)?;
    if self
      .sessions
      .managed
      .get(&crate::workspace::ManagedSessionRequest {
        request_id: Some(request.request_id.clone()),
        session_key: None,
      })
      .map_err(SessionCreationError::not_started)?
      .is_some()
    {
      return Err(SessionCreationError::not_started(
        "This creation request ID belongs to a Pi session",
      ));
    }
    let Some(existing) = manager
      .get(&request.request_id)
      .map_err(SessionCreationError::not_started)?
    else {
      return Ok(None);
    };
    self
      .claim_creation_route(request)
      .await
      .map_err(SessionCreationError::not_started)?;
    let runtime = creation_runtime(request).map_err(SessionCreationError::not_started)?;
    let intent = CreationRequest {
      request_id: request.request_id.clone(),
      provider: request.provider,
      project_id: request.project_id.clone(),
      working_directory: existing.working_directory.clone(),
      input: MessageInput {
        text: request.prompt.clone(),
        ..Default::default()
      },
      image_ids: request.image_ids.clone(),
      runtime,
      model: request
        .model
        .as_ref()
        .map(crate::workspace::SessionCreationModelSelection::identity),
    };
    manager
      .receipt(&intent)
      .map_err(|error| SessionCreationError::uncertain(error.message))?;
    let snapshot = manager
      .refresh(&request.request_id)
      .await
      .map_err(SessionCreationError::uncertain)?
      .unwrap_or(existing);
    self
      .creation_snapshot(snapshot)
      .map(Some)
      .map_err(SessionCreationError::uncertain)
  }

  pub(super) async fn preflight_rpc_creation(
    &self,
    request: &crate::workspace::CreateProjectSessionRequest,
    project: &crate::workspace::RegisteredProject,
  ) -> Result<(), String> {
    if request.provider == LiveProvider::Dsh && !request.image_ids.is_empty() {
      return Err("DSH does not advertise image support for new sessions. Remove the images before starting.".into());
    }
    let manager = self.sessions.runtimes.clone();
    let provider = request.provider;
    let cwd = PathBuf::from(&project.folder_path);
    let selection = creation_runtime(request)?;
    tokio::task::spawn_blocking(move || {
      manager
        .creation_selection(provider, &cwd, selection.as_ref())
        .map(|_| ())
    })
    .await
    .map_err(|_| "Could not validate the creation runtime".to_owned())?
  }

  pub(super) async fn start_rpc_creation(
    &self,
    request: crate::workspace::CreateProjectSessionRequest,
    project: crate::workspace::RegisteredProject,
    images: Vec<crate::images::ResolvedImage>,
  ) -> Result<crate::workspace::ManagedSessionSnapshot, crate::workspace::SessionCreationError> {
    use crate::workspace::SessionCreationError;
    let manager = self.creation_manager().map_err(SessionCreationError::not_started)?;
    let runtime = creation_runtime(&request).map_err(SessionCreationError::not_started)?;
    use base64::Engine;
    let input = MessageInput {
      text: request.prompt,
      images: images
        .into_iter()
        .map(|image| tokn_interaction_core::ImageInput {
          mime_type: image.mime_type,
          data: base64::engine::general_purpose::STANDARD.encode(image.bytes.as_slice()),
        })
        .collect(),
      ..Default::default()
    };
    let intent = CreationRequest {
      request_id: request.request_id,
      provider: request.provider,
      project_id: request.project_id,
      working_directory: project.folder_path.into(),
      input,
      image_ids: request.image_ids,
      runtime,
      model: request
        .model
        .as_ref()
        .map(crate::workspace::SessionCreationModelSelection::identity),
    };
    let snapshot = manager.create(intent).await.map_err(|error| {
      if error.kind == ErrorKind::Uncertain {
        SessionCreationError::uncertain(error.message)
      } else {
        SessionCreationError::not_started(error.message)
      }
    })?;
    self
      .creation_snapshot(snapshot)
      .map_err(SessionCreationError::uncertain)
  }
}

fn creation_runtime(
  request: &crate::workspace::CreateProjectSessionRequest,
) -> Result<Option<RuntimeSelection>, String> {
  let selected = selected_creation_runtime(request.runtime_profile_id.as_deref(), request.runtime_revision)?;
  let Some(model) = &request.model else {
    return Ok(selected);
  };
  let pinned = selected_creation_runtime(model.runtime_profile_id.as_deref(), model.runtime_revision)?;
  if selected.is_some() && selected != pinned {
    return Err("The model belongs to a different runtime. Choose a model again.".into());
  }
  Ok(pinned)
}

pub(super) fn selected_creation_runtime(
  profile_id: Option<&str>,
  revision: Option<u64>,
) -> Result<Option<RuntimeSelection>, String> {
  match (profile_id, revision) {
    (Some(id), Some(revision)) => Ok(Some(RuntimeSelection {
      profile_id: id.to_owned(),
      expected_revision: revision,
    })),
    (None, None) => Ok(None),
    _ => Err("Choose a runtime again; its revision is missing".into()),
  }
}

fn viewer_provider(provider: LiveProvider) -> ViewerProvider {
  match provider {
    LiveProvider::Pi => ViewerProvider::Pi,
    LiveProvider::Codex => ViewerProvider::Codex,
    LiveProvider::OpenCode => ViewerProvider::OpenCode,
    LiveProvider::Dsh => ViewerProvider::Dsh,
  }
}

pub(crate) fn validate_creation_profile(
  profile: &crate::rpc_profiles::RpcResolvedProfile,
  cwd: &Path,
) -> Result<(), String> {
  if profile.profile.provider == LiveProvider::OpenCode
    && profile.profile.working_directory.as_deref().map(Path::new) != Some(cwd)
  {
    return Err("This OpenCode runtime uses another project folder. Configure a runtime for this folder.".into());
  }
  if profile.profile.mode == RpcRuntimeMode::Attached {
    let endpoint = profile
      .profile
      .endpoint
      .as_deref()
      .ok_or("The runtime endpoint is missing")?;
    let url = url::Url::parse(endpoint).map_err(|_| "The runtime endpoint is invalid")?;
    if !matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]" | "::1")) {
      return Err(
        "Creating sessions currently requires a local runtime with history readable on this computer.".into(),
      );
    }
  }
  Ok(())
}

impl CreationHost for Host {
  fn plan_native_id(&self, provider: LiveProvider, _: &str) -> Result<Option<String>, String> {
    match provider {
      LiveProvider::OpenCode => Ok(Some(tokn_opencode_adapter::new_session_id())),
      LiveProvider::Dsh => Ok(Some(tokn_dsh_adapter::new_session_id())),
      LiveProvider::Codex => Ok(None),
      LiveProvider::Pi => Err("Pi creation uses its managed-session driver".into()),
    }
  }
  fn create_native<'a>(
    &'a self,
    request: &'a CreationRequest,
    planned_id: Option<&'a str>,
  ) -> AdapterFuture<'a, Result<CreatedNativeSession, InteractionError>> {
    Box::pin(async move {
      let runtimes = self.runtimes.clone();
      let selection = request.runtime.clone();
      let cwd = request.working_directory.clone();
      let provider = request.provider;
      let selection =
        tokio::task::spawn_blocking(move || runtimes.creation_selection(provider, &cwd, selection.as_ref()))
          .await
          .map_err(|_| InteractionError::unavailable("Could not validate the creation runtime"))?
          .map_err(InteractionError::unavailable)?;
      self
        .runtimes
        .create_native_session(
          request.provider,
          request.working_directory.clone(),
          selection,
          planned_id.map(str::to_owned),
          request.model.clone(),
        )
        .await
    })
  }
  fn send_initial<'a>(
    &'a self,
    created: &'a CreatedNativeSession,
    request_id: &'a str,
    input: &'a MessageInput,
    model: Option<&'a tokn_interaction_core::ModelIdentity>,
  ) -> AdapterFuture<'a, Result<(), InteractionError>> {
    Box::pin(async move {
      let state = if model.is_some() || !input.images.is_empty() {
        let state = self
          .runtimes
          .execute_created(created, uuid::Uuid::new_v4().to_string(), Command::State)
          .await?;
        let Reply::State(state) = state else {
          return Err(InteractionError::unavailable(
            "Could not verify the new session's model before its first message",
          ));
        };
        if let Some(expected) = model
          && !state
            .model
            .as_ref()
            .is_some_and(|current| creation_model_matches(expected, current))
        {
          return Err(InteractionError::invalid(
            "The new session did not confirm the selected model. Its first message was not sent.",
          ));
        }
        Some(state)
      } else {
        None
      };
      if !input.images.is_empty() {
        let catalog = self
          .runtimes
          .execute_created(created, uuid::Uuid::new_v4().to_string(), Command::Models)
          .await?;
        let supported = match catalog {
          Reply::Models(models) => state
            .as_ref()
            .and_then(|state| state.model.as_ref())
            .is_some_and(|current| {
              models
                .iter()
                .any(|model| model.supports_images && creation_model_matches(&model.model, current))
            }),
          _ => false,
        };
        if !supported {
          return Err(InteractionError::unsupported(
            "The new session's current model does not advertise image support",
          ));
        }
      }
      match self
        .runtimes
        .execute_created(
          created,
          request_id.to_owned(),
          Command::Message {
            input: input.clone(),
            delivery: DeliveryMode::StartOrSteer,
          },
        )
        .await?
      {
        Reply::Admission(_) | Reply::Acknowledged => Ok(()),
        _ => Err(InteractionError::uncertain(
          "The first message's admission was not confirmed. Check the existing session before sending again.",
        )),
      }
    })
  }
  fn cancel_initial<'a>(
    &'a self,
    created: &'a CreatedNativeSession,
  ) -> AdapterFuture<'a, Result<(), InteractionError>> {
    Box::pin(async move {
      match self
        .runtimes
        .execute_created(created, uuid::Uuid::new_v4().to_string(), Command::CancelTurn)
        .await?
      {
        Reply::Acknowledged => Ok(()),
        _ => Err(InteractionError::uncertain("Turn cancellation was not confirmed")),
      }
    })
  }
  fn publish_history<'a>(&'a self, receipt: &'a CreationSnapshot) -> AdapterFuture<'a, Result<Option<String>, String>> {
    Box::pin(async move {
      let history = self.history.clone();
      let receipt = receipt.clone();
      let created = receipt.created.clone();
      let planned = receipt.planned_native_id.clone();
      let provider = receipt.provider;
      let cwd = receipt.working_directory.clone();
      let key = tokio::task::spawn_blocking(move || history.publish(&receipt))
        .await
        .map_err(|_| "Could not verify the new session's history".to_owned())??;
      if let (Some(created), Some(key)) = (&created, &key) {
        // A restarted host may publish durable history without its old runtime;
        // the next explicit Send will recover the verified session normally.
        if self.runtimes.adopt_created(created, key.clone()).await.is_err()
          && self.runtimes.inventory().runtimes.iter().any(|runtime| {
            runtime.runtime_id == created.target.runtime_id && runtime.instance_id == created.target.instance_id
          })
        {
          return Err("The created runtime is still present, but its session binding could not be recovered. Check Agent runtimes and retry this creation.".into());
        }
      } else if let (Some(id), Some(key)) = (planned, &key) {
        // A lost create acknowledgement still leaves a host-planned identity.
        // Adopt only an existing exact owner after local history verification;
        // reconciliation never creates, resumes, or sends the first prompt.
        self
          .runtimes
          .adopt_verified_creation(provider, &id, cwd, key.clone())
          .await
          .map_err(|error| {
            format!(
              "The saved session was found, but its live binding could not be recovered: {}",
              error.message
            )
          })?;
      }
      Ok(key)
    })
  }
}

fn creation_model_matches(
  expected: &tokn_interaction_core::ModelIdentity,
  actual: &tokn_interaction_core::ModelIdentity,
) -> bool {
  expected.model_id == actual.model_id
    && expected
      .model_provider
      .as_ref()
      .is_none_or(|provider| actual.model_provider.as_ref() == Some(provider))
}

impl HistoryPublisher {
  fn publish(&self, receipt: &CreationSnapshot) -> Result<Option<String>, String> {
    let Some(id) = receipt
      .created
      .as_ref()
      .map(|created| &created.native.native_id)
      .or_else(|| {
        matches!(receipt.provider, LiveProvider::OpenCode | LiveProvider::Dsh)
          .then_some(receipt.planned_native_id.as_ref())
          .flatten()
      })
    else {
      return Ok(None);
    };
    let provider = viewer_provider(receipt.provider);
    let expected_cwd = receipt
      .working_directory
      .canonicalize()
      .map_err(|_| "The new session's project folder is unavailable")?;
    let header = if let Some(path) = receipt
      .created
      .as_ref()
      .and_then(|created| created.native.history_path.as_ref())
    {
      let roots = self.repository.file_session_roots(provider)?;
      let candidate = path.canonicalize().unwrap_or_else(|_| path.clone());
      if !roots
        .iter()
        .any(|root| candidate.starts_with(root.canonicalize().unwrap_or_else(|_| root.clone())))
      {
        return Err("The new history is outside this machine's configured session sources. Add its history root in Settings, then check this creation again.".into());
      }
      if !path.exists() {
        return Ok(None);
      }
      if provider == ViewerProvider::Codex {
        let native = tokn_session_codex::history_header(path)?;
        let payload = &native.native()["payload"];
        if payload["id"].as_str() != Some(id)
          || payload["cwd"]
            .as_str()
            .map(Path::new)
            .and_then(|path| path.canonicalize().ok())
            .as_ref()
            != Some(&expected_cwd)
        {
          return Err("The created Codex history does not match its native identity and project folder".into());
        }
      }
      let canonical = path.canonicalize().map_err(|error| error.to_string())?;
      self.repository.session_header_at_path(provider, &canonical)?
    } else {
      let matching = self
        .repository
        .list_session_headers(provider)?
        .into_iter()
        .filter(|header| &header.id == id)
        .collect::<Vec<_>>();
      if matching.len() > 1 {
        return Err("More than one local history claims the new session's native identity".into());
      }
      let Some(header) = matching.into_iter().next() else {
        return Ok(None);
      };
      header
    };
    if header.id != *id
      || header
        .cwd
        .as_deref()
        .map(Path::new)
        .and_then(|path| path.canonicalize().ok())
        .as_ref()
        != Some(&expected_cwd)
      || header.parent_session_id.is_some()
      || header
        .agent_path
        .as_deref()
        .is_some_and(|path| !path.is_empty() && path != "/root")
    {
      return Err("The new session's local history does not match its identity and project folder".into());
    }
    self.index_header(receipt, provider, header)
  }

  fn index_header(
    &self,
    receipt: &CreationSnapshot,
    provider: ViewerProvider,
    header: SessionHeader,
  ) -> Result<Option<String>, String> {
    let project = self
      .index
      .get_registered_project(&receipt.project_id)
      .map_err(|error| error.to_string())?
      .ok_or("The registered project is unavailable")?;
    if Path::new(&project.folder_path) != receipt.working_directory {
      return Err("The registered project changed during creation".into());
    }
    let _gate = self.gate.lock().map_err(|_| "Session index is unavailable")?;
    let path = header.path.clone();
    let before = source_cursor(provider, &path)?;
    let source = index_source_key_for_path(provider, &path)?;
    // Capture the optimistic index checkpoint before provider I/O. A competing
    // body/catalog writer must retain its newer commit if it wins this read.
    let previous = self.index.source_state(&source).map_err(|error| error.to_string())?;
    let fresh = self.repository.session_header_exact(provider, &path, &header.id)?;
    if fresh
      .cwd
      .as_deref()
      .map(Path::new)
      .and_then(|path| path.canonicalize().ok())
      .as_ref()
      != Some(&receipt.working_directory)
      || fresh.parent_session_id.is_some()
      || fresh
        .agent_path
        .as_deref()
        .is_some_and(|path| !path.is_empty() && path != "/root")
    {
      return Err("The new history changed its identity or project folder before indexing".into());
    }
    if provider == ViewerProvider::Codex {
      let owning = tokn_session_codex::history_header(&path)?;
      if owning.native()["payload"]["id"].as_str() != Some(&header.id) {
        return Err("Codex's owning history identity changed before indexing".into());
      }
    }
    let header = fresh;
    let locator = locator_for_header(provider, &header);
    let key = index_session_key(&locator)?;
    if self
      .index
      .has_other_present_session_source(&key)
      .map_err(|error| error.to_string())?
    {
      return Err("Another local source already claims the created session's native identity".into());
    }
    if provider != ViewerProvider::OpenCode
      && previous
        .as_ref()
        .is_some_and(|state| state.cursor != INDEX_MISSING_SOURCE_CURSOR)
    {
      if self
        .index
        .unique_present_session_for_source(&source)
        .map_err(|error| error.to_string())?
        .as_ref()
        .is_none_or(|row| row.key != key)
      {
        return Err("The new history path already belongs to another indexed session identity".into());
      }
    }
    let headers = if provider == ViewerProvider::OpenCode {
      // SQLite is shared by many sessions. Replace one complete source catalog,
      // never a one-row inventory that would retire unrelated conversations.
      tokn_session_opencode::OpenCodeSessionSource::new(Some(path.clone())).list_session_headers()?
    } else {
      vec![header]
    };
    let ready = self
      .index
      .source_state(&index_catalog_source_key(provider))
      .map_err(|error| error.to_string())?
      .is_some();
    let changed = previous
      .as_ref()
      .and_then(|state| indexed_source_raw_cursor(&state.cursor))
      != Some(before.as_str());
    let mut metadata = Vec::with_capacity(headers.len());
    for header in headers {
      let session_key = IndexedSessionKey::new(source.provider.clone(), source.source_key.clone(), header.id.clone());
      let existing = self.index.session(&session_key).map_err(|error| error.to_string())?;
      metadata.push(catalog_session_metadata(
        &source,
        header,
        existing.as_ref(),
        ready,
        changed,
      )?);
    }
    if source_cursor(provider, &path)? != before {
      return Ok(None);
    }
    let replacement = SourceReplacement::new(
      SourceState::new(source, pending_body_cursor(&before), current_time_ms()),
      metadata,
    )
    .with_source_cursor_precondition(source_cursor_precondition(previous.as_ref()));
    match self.index.replace_sources(&[replacement]) {
      Ok(_) => {}
      Err(SessionIndexError::SourceCursorConflict { .. }) => return Ok(None),
      Err(error) => return Err(error.to_string()),
    }
    self
      .index
      .admit_managed_session(&key, &receipt.project_id)
      .map_err(|error| error.to_string())?;
    if let Some(sender) = self
      .retry
      .lock()
      .map_err(|_| "Session scheduler is unavailable")?
      .as_ref()
    {
      let wake = if provider == ViewerProvider::OpenCode {
        SessionIndexWake::ProviderCatalog(BTreeSet::from([provider]))
      } else {
        SessionIndexWake::ChangedFiles(BTreeMap::from([(provider, BTreeSet::from([path]))]))
      };
      let _ = sender.send(wake);
    }
    encode_session_key(&locator).map(Some)
  }
}

#[cfg(test)]
mod tests;
