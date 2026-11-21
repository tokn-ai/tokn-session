use super::*;
use std::{collections::BTreeMap, ffi::OsString, path::Path, time::Duration};

#[derive(Clone)]
pub(super) enum Handle {
  Codex(Arc<tokn_codex_adapter::CodexAdapter>),
  Pi(Arc<tokn_pi_adapter::PiAdapter>),
  OpenCode(Arc<tokn_opencode_adapter::OpenCodeAdapter>),
  ZCode {
    adapter: Arc<tokn_zcode_adapter::ZCodeAdapter>,
    history_path: PathBuf,
  },
  Qoder {
    adapter: Arc<tokn_qoder_adapter::QoderAdapter>,
    history_root: PathBuf,
    directory: PathBuf,
  },
  Dsh(Arc<tokn_dsh_adapter::DshAdapter>),
}
impl Handle {
  pub(super) fn adapter(&self) -> Arc<dyn InteractionAdapter> {
    match self {
      Self::Codex(adapter) => adapter.clone(),
      Self::Pi(adapter) => adapter.clone(),
      Self::OpenCode(adapter) => adapter.clone(),
      Self::ZCode { adapter, .. } => adapter.clone(),
      Self::Qoder { adapter, .. } => adapter.clone(),
      Self::Dsh(adapter) => adapter.clone(),
    }
  }
  pub(super) async fn bind_native(&self, id: &str) -> Result<(), String> {
    if let Self::Codex(adapter) = self {
      adapter
        .resume_thread(id)
        .await
        .map_err(|_| "Could not resume the selected Codex thread")?;
    }
    Ok(())
  }
}
fn env(profile: &RpcResolvedProfile) -> BTreeMap<OsString, OsString> {
  profile
    .environment
    .iter()
    .map(|(key, value)| (key.into(), value.into()))
    .collect()
}
fn path(profile: &RpcResolvedProfile) -> Result<PathBuf, String> {
  profile
    .profile
    .executable
    .as_ref()
    .map(PathBuf::from)
    .ok_or_else(|| "Runtime executable is missing".into())
}
fn cwd(profile: &RpcResolvedProfile) -> Result<PathBuf, String> {
  profile
    .profile
    .working_directory
    .as_ref()
    .map(PathBuf::from)
    .ok_or_else(|| "Runtime working directory is missing".into())
}
fn endpoint(profile: &RpcResolvedProfile) -> Result<&str, String> {
  profile
    .profile
    .endpoint
    .as_deref()
    .ok_or_else(|| "Runtime endpoint is missing".into())
}
fn bearer(profile: &RpcResolvedProfile) -> Option<&str> {
  if let RpcCredentials::Bearer { token } = &profile.credentials {
    Some(token)
  } else {
    None
  }
}
pub(super) async fn open(id: &str, profile: &RpcResolvedProfile) -> Result<Handle, String> {
  let owned = profile.profile.mode == RpcRuntimeMode::Managed;
  let result = match profile.profile.provider {
    Provider::Codex => {
      let options = tokn_codex_adapter::EndpointOptions::new(id);
      let adapter = if owned {
        tokn_codex_adapter::CodexAdapter::spawn(
          options,
          tokn_codex_adapter::LaunchOptions {
            executable: path(profile)?,
            working_directory: cwd(profile)?,
            arguments: profile.profile.arguments.iter().map(OsString::from).collect(),
            environment: env(profile),
          },
        )
        .await
      } else {
        tokn_codex_adapter::CodexAdapter::connect(options, endpoint(profile)?, bearer(profile)).await
      };
      adapter.map(|adapter| Handle::Codex(Arc::new(adapter)))
    }
    Provider::Pi => tokn_pi_adapter::PiAdapter::spawn(
      tokn_pi_adapter::EndpointOptions::new(id),
      tokn_pi_adapter::LaunchOptions {
        executable: path(profile)?,
        working_directory: cwd(profile)?,
        arguments: profile.profile.arguments.iter().map(OsString::from).collect(),
        environment: env(profile),
        session_file: None,
      },
    )
    .await
    .map(|adapter| Handle::Pi(Arc::new(adapter))),
    Provider::OpenCode => {
      let adapter = if owned {
        tokn_opencode_adapter::OpenCodeAdapter::launch(tokn_opencode_adapter::LaunchOptions {
          runtime_id: id.into(),
          executable: path(profile)?,
          directory: cwd(profile)?.to_string_lossy().into(),
          port: 0,
          environment: profile.environment.clone(),
        })
        .await
      } else {
        tokn_opencode_adapter::OpenCodeAdapter::connect(tokn_opencode_adapter::ConnectionOptions {
          runtime_id: id.into(),
          endpoint: endpoint(profile)?.into(),
          directory: cwd(profile)?.to_string_lossy().into(),
          workspace_id: None,
          credentials: match &profile.credentials {
            RpcCredentials::Basic { username, password } => Some(tokn_opencode_adapter::Credentials {
              username: username.clone(),
              password: password.clone(),
            }),
            _ => None,
          },
        })
        .await
      };
      adapter.map(|adapter| Handle::OpenCode(Arc::new(adapter)))
    }
    Provider::ZCode => {
      if !owned {
        return Err("ZCode requires a managed stdio runtime".into());
      }
      let history_path = tokn_session_zcode::ZCodeSessionSource::new(None).database_path()?;
      tokn_zcode_adapter::ZCodeAdapter::launch(tokn_zcode_adapter::LaunchOptions {
        runtime_id: id.into(),
        executable: path(profile)?,
        arguments: profile.profile.arguments.clone(),
        directory: cwd(profile)?,
        environment: profile.environment.clone(),
        history_path: history_path.clone(),
      })
      .await
      .map(|adapter| Handle::ZCode {
        adapter: Arc::new(adapter),
        history_path,
      })
    }
    Provider::Qoder => {
      if !owned {
        return Err("Qoder requires a managed ACP runtime".into());
      }
      let history_root = tokn_session_qoder::QoderSessionSource::new(None).session_root()?;
      let environment = qoder_environment(profile, &history_root)?;
      let directory = cwd(profile)?
        .canonicalize()
        .map_err(|_| "Qoder project folder is unavailable")?;
      tokn_qoder_adapter::QoderAdapter::launch(tokn_qoder_adapter::LaunchOptions {
        runtime_id: id.into(),
        executable: path(profile)?,
        arguments: profile.profile.arguments.clone(),
        directory: directory.clone(),
        environment,
      })
      .await
      .map(|adapter| Handle::Qoder {
        adapter: Arc::new(adapter),
        history_root,
        directory,
      })
    }
    Provider::Dsh => {
      let adapter = if owned {
        let mut options = tokn_dsh_adapter::ManagedOptions::new(cwd(profile)?);
        options.program = path(profile)?;
        options.runtime_id = id.into();
        options.env = profile.environment.clone();
        tokn_dsh_adapter::DshAdapter::launch(options).await
      } else {
        let mut options = tokn_dsh_adapter::ApiProxyConfig::new(endpoint(profile)?);
        options.runtime_id = id.into();
        options.bearer_token = bearer(profile).map(str::to_owned);
        options.connect_timeout = Duration::from_secs(5);
        tokn_dsh_adapter::DshAdapter::attach(options).await
      };
      adapter.map(|adapter| Handle::Dsh(Arc::new(adapter)))
    }
  };
  result.map_err(|error| error.to_string())
}

fn qoder_environment(profile: &RpcResolvedProfile, history_root: &Path) -> Result<BTreeMap<String, String>, String> {
  if crate::rpc_profiles::qoder_storage_override(&profile.profile.arguments) {
    return Err("The host owns Qoder ACP, session, project and history options; --config-dir and session-control arguments are unavailable".into());
  }
  let config = history_root
    .parent()
    .filter(|path| path.is_absolute())
    .ok_or("Qoder history needs an absolute configuration directory")?;
  let expected = config.canonicalize().unwrap_or_else(|_| config.to_path_buf());
  if let Some(configured) = profile.environment.get("QODER_CONFIG_DIR") {
    let configured = PathBuf::from(configured);
    if !configured.is_absolute() || configured.canonicalize().unwrap_or(configured) != expected {
      return Err("This Qoder runtime configuration uses a different history directory".into());
    }
  }
  let mut environment = profile.environment.clone();
  environment.insert("QODER_CONFIG_DIR".into(), config.to_string_lossy().into_owned());
  Ok(environment)
}

fn qoder_history_path(root: &Path, directory: &Path, native_id: &str) -> Result<PathBuf, InteractionError> {
  if native_id.is_empty()
    || native_id.len() > 128
    || !native_id
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
  {
    return Err(InteractionError::uncertain(
      "Qoder returned an invalid native session identity",
    ));
  }
  let directory = directory
    .to_str()
    .ok_or_else(|| InteractionError::invalid("Qoder project path must be valid UTF-8"))?;
  Ok(
    root
      .join(tokn_session_qoder::project_identifier(directory))
      .join(format!("{native_id}.jsonl")),
  )
}

fn verify_qoder_history(path: &Path, root: &Path, directory: &Path, native_id: &str) -> Result<(), String> {
  let root = root.canonicalize().map_err(|_| "Qoder history source is unavailable")?;
  let actual = path.canonicalize().map_err(|_| "Qoder history file is unavailable")?;
  let expected = qoder_history_path(&root, directory, native_id).map_err(|error| error.message)?;
  if actual != expected || !actual.starts_with(&root) {
    return Err("The Qoder runtime uses a different history source or project".into());
  }
  let header = tokn_session_qoder::read_session_header_exact(&actual, native_id)?;
  if header.parent_session_id.is_some()
    || header
      .cwd
      .as_deref()
      .and_then(|cwd| Path::new(cwd).canonicalize().ok())
      .as_deref()
      != Some(directory)
  {
    return Err("The Qoder history does not match its native root session and project".into());
  }
  Ok(())
}
pub(super) fn redact_error(_error: &str, profile: &RpcResolvedProfile) -> String {
  // Native diagnostics can quote encoded credentials or inherited environment
  // values. A public error must not depend on matching literal secret strings.
  format!(
    "Could not start or connect {:?}. Validate this host's executable, folder, endpoint, authentication, and agent configuration.",
    profile.profile.provider
  )
}

impl core_runtime::RuntimeHandle for Handle {
  fn adapter(&self) -> Arc<dyn InteractionAdapter> {
    Handle::adapter(self)
  }

  fn create_native<'a>(
    &'a self,
    request: &'a core_runtime::CreateNativeSessionRequest,
  ) -> AdapterFuture<'a, Result<core_runtime::NativeSessionDescriptor, InteractionError>> {
    Box::pin(async move {
      let (native_id, working_directory, history_path) = match self {
        Self::Codex(adapter) => {
          let created = adapter
            .create_thread_with_descriptor(tokn_codex_adapter::ThreadOptions {
              cwd: Some(request.working_directory.clone()),
              model: request.model.as_ref().map(|model| model.model_id.clone()),
              model_provider: request.model.as_ref().and_then(|model| model.model_provider.clone()),
              ephemeral: Some(false),
              ..Default::default()
            })
            .await?;
          (
            created
              .target
              .session_id
              .ok_or_else(|| InteractionError::uncertain("Codex omitted the created thread ID"))?,
            created.working_directory,
            created.history_path,
          )
        }
        Self::OpenCode(adapter) => {
          let id = request
            .native_id
            .as_deref()
            .ok_or_else(|| InteractionError::invalid("OpenCode creation requires a reserved ID"))?;
          let created = adapter.create_session_with_id(id, None, request.model.clone()).await?;
          (
            created
              .target
              .session_id
              .ok_or_else(|| InteractionError::uncertain("OpenCode omitted the created session ID"))?,
            created.working_directory,
            None,
          )
        }
        Self::ZCode { adapter, .. } => {
          let id = request
            .native_id
            .as_deref()
            .ok_or_else(|| InteractionError::invalid("ZCode creation requires a reserved ID"))?;
          let created = adapter.create_session_with_id(id, request.model.clone()).await?;
          (
            created
              .target
              .session_id
              .ok_or_else(|| InteractionError::uncertain("ZCode omitted the created session ID"))?,
            created.working_directory,
            None,
          )
        }
        Self::Qoder {
          adapter, history_root, ..
        } => {
          let created = adapter.create_session(request.model.clone()).await?;
          let native_id = created
            .target
            .session_id
            .ok_or_else(|| InteractionError::uncertain("Qoder omitted the created session ID"))?;
          let history_path = qoder_history_path(history_root, &created.working_directory, &native_id)?;
          (native_id, created.working_directory, Some(history_path))
        }
        Self::Dsh(adapter) => {
          if request.model.is_some() {
            return Err(InteractionError::unsupported(
              "DSH uses its default model for new sessions",
            ));
          }
          let id = request
            .native_id
            .clone()
            .ok_or_else(|| InteractionError::invalid("DSH creation requires a reserved ID"))?;
          let created = adapter
            .create_session(tokn_dsh_adapter::CreateSessionOptions {
              session_id: id,
              working_directory: request.working_directory.clone(),
              agent_preset: None,
            })
            .await?;
          (
            created
              .target
              .session_id
              .ok_or_else(|| InteractionError::uncertain("DSH omitted the created session ID"))?,
            created.working_directory,
            None,
          )
        }
        Self::Pi(_) => {
          return Err(InteractionError::unsupported(
            "Create Pi sessions through its managed-session driver",
          ));
        }
      };
      Ok(core_runtime::NativeSessionDescriptor {
        native_id,
        working_directory,
        history_path,
      })
    })
  }

  fn bind_native<'a>(&'a self, id: &'a str) -> AdapterFuture<'a, Result<(), String>> {
    Box::pin(Handle::bind_native(self, id))
  }

  fn bind_history<'a>(&'a self, key: &'a str, id: &'a str) -> AdapterFuture<'a, Result<(), String>> {
    Box::pin(async move {
      if let Self::Qoder {
        adapter,
        history_root,
        directory,
      } = self
      {
        let locator = crate::model::decode_session_key(key)?;
        if locator.provider != crate::model::ViewerProvider::Qoder || locator.session_id != id {
          return Err("The selected history does not match the Qoder session".into());
        }
        let root = history_root.clone();
        let directory = directory.clone();
        let native_id = id.to_owned();
        tokio::task::spawn_blocking(move || verify_qoder_history(&locator.source_path, &root, &directory, &native_id))
          .await
          .map_err(|_| "Could not verify Qoder history")??;
        adapter.resume_session(id).await.map_err(|error| error.message)?;
        return Ok(());
      }
      let Self::ZCode { adapter, history_path } = self else {
        return Handle::bind_native(self, id).await;
      };
      let locator = crate::model::decode_session_key(key)?;
      if locator.provider != crate::model::ViewerProvider::ZCode || locator.session_id != id {
        return Err("The selected history does not match the ZCode session".into());
      }
      let history_path = history_path.clone();
      let same_source = tokio::task::spawn_blocking(move || {
        locator.source_path.canonicalize().is_ok_and(|source| {
          history_path
            .canonicalize()
            .is_ok_and(|runtime_source| source == runtime_source)
        })
      })
      .await
      .map_err(|_| "Could not verify the ZCode history source")?;
      if !same_source {
        return Err("The ZCode runtime uses a different history database".into());
      }
      adapter.resume_session(id).await.map_err(|error| error.message)?;
      Ok(())
    })
  }
}

impl core_runtime::RuntimeHost for Host {
  type Profile = RpcResolvedProfile;

  fn resolve_profile(&self, profile_id: &str, revision: u64) -> Result<Self::Profile, String> {
    self.resolver.resolve_profile(profile_id, revision)
  }

  fn describe(&self, profile: &Self::Profile) -> core_runtime::ProfileDescriptor {
    core_runtime::ProfileDescriptor {
      profile_id: profile.profile.profile_id.clone(),
      revision: profile.revision,
      name: profile.profile.name.clone(),
      provider: profile.profile.provider,
      ownership: match profile.profile.mode {
        RpcRuntimeMode::Managed => Ownership::Managed,
        RpcRuntimeMode::Attached => Ownership::Attached,
      },
    }
  }

  fn validate_launch_ready(&self, profile: &Self::Profile) -> Result<(), String> {
    profile.validate_launch_ready()
  }

  fn validate_creation_model(&self, profile: &Self::Profile, model: &ModelIdentity) -> Result<(), InteractionError> {
    match profile.profile.provider {
      Provider::Codex => Ok(()),
      Provider::OpenCode if model.model_provider.is_some() => Ok(()),
      Provider::OpenCode => Err(InteractionError::invalid(
        "OpenCode model selection requires its provider identity",
      )),
      Provider::ZCode if model.model_provider.is_some() => Ok(()),
      Provider::ZCode => Err(InteractionError::invalid(
        "ZCode model selection requires its provider identity",
      )),
      Provider::Qoder if model.model_provider.is_none() => Ok(()),
      Provider::Qoder => Err(InteractionError::invalid(
        "Qoder model selection uses its native model identity",
      )),
      _ => Err(InteractionError::unsupported(
        "This runtime uses its default model for new sessions",
      )),
    }
  }

  fn select_profile<'a>(
    &'a self,
    provider: Provider,
    cwd: &'a std::path::Path,
  ) -> AdapterFuture<'a, Result<(String, u64), String>> {
    Box::pin(async move {
      let resolver = self.resolver.clone();
      let cwd = cwd.to_path_buf();
      let plan = tokio::task::spawn_blocking(move || resolver.resolve_plan(provider, &cwd, None))
        .await
        .map_err(|_| "Could not resolve the local runtime")??;
      let selection = plan.shared_runtime_selection()?;
      Ok((selection.profile_id, selection.expected_revision))
    })
  }

  fn open<'a>(
    &'a self,
    runtime_id: &'a str,
    profile: &'a Self::Profile,
  ) -> AdapterFuture<'a, Result<Arc<dyn core_runtime::RuntimeHandle>, String>> {
    Box::pin(async move {
      open(runtime_id, profile)
        .await
        .map(|handle| Arc::new(handle) as Arc<dyn core_runtime::RuntimeHandle>)
    })
  }

  fn redact_error(&self, error: &str, profile: &Self::Profile) -> String {
    redact_error(error, profile)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn profile(directory: &Path) -> RpcResolvedProfile {
    RpcResolvedProfile {
      revision: 1,
      profile: RpcRuntimeProfile {
        profile_id: uuid::Uuid::new_v4().to_string(),
        name: "Qoder fixture".into(),
        provider: Provider::Qoder,
        mode: RpcRuntimeMode::Managed,
        executable: Some("/fixture/qodercli".into()),
        arguments: Vec::new(),
        working_directory: Some(directory.to_string_lossy().into_owned()),
        endpoint: None,
        environment_keys: Vec::new(),
        credential_kind: None,
        is_default: false,
      },
      environment: BTreeMap::new(),
      credentials: RpcCredentials::None,
    }
  }

  #[test]
  fn qoder_launch_cannot_redirect_native_history_through_environment_or_arguments() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("config/projects");
    std::fs::create_dir_all(&root).unwrap();
    let mut profile = profile(directory.path());
    let environment = qoder_environment(&profile, &root).unwrap();
    assert_eq!(
      environment["QODER_CONFIG_DIR"],
      root.parent().unwrap().to_string_lossy()
    );
    profile.environment = environment;
    assert!(qoder_environment(&profile, &root).is_ok());
    for arguments in [
      vec!["--config-dir".into(), "/another/config".into()],
      vec!["--config-dir=/another/config".into()],
    ] {
      profile.profile.arguments = arguments;
      assert!(qoder_environment(&profile, &root).unwrap_err().contains("--config-dir"));
    }
    profile.profile.arguments.clear();
    for argument in [
      "--resume=other",
      "--continue",
      "--worktree",
      "--remote-session=other",
      "--session-id=other",
      "--acp=false",
      "-rfixture",
      "-p",
    ] {
      profile.profile.arguments = vec![argument.into()];
      assert!(qoder_environment(&profile, &root).is_err(), "{argument}");
    }
    profile.profile.arguments.clear();
    profile.environment.insert(
      "QODER_CONFIG_DIR".into(),
      directory.path().join("other").to_string_lossy().into_owned(),
    );
    assert!(
      qoder_environment(&profile, &root)
        .unwrap_err()
        .contains("different history")
    );
  }

  #[test]
  fn qoder_binding_requires_exact_root_transcript_identity_project_and_storage() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let project = project.canonicalize().unwrap();
    let root = directory.path().join("config/projects");
    let id = "11111111-1111-4111-8111-111111111111";
    let history = qoder_history_path(&root, &project, id).unwrap();
    std::fs::create_dir_all(history.parent().unwrap()).unwrap();
    let rows = include_str!("../../tests/fixtures/qoder/root_session.jsonl")
      .lines()
      .map(|line| {
        let mut row: serde_json::Value = serde_json::from_str(line).unwrap();
        row["cwd"] = serde_json::json!(project);
        row.to_string()
      })
      .collect::<Vec<_>>()
      .join("\n")
      + "\n";
    std::fs::write(&history, &rows).unwrap();
    assert!(verify_qoder_history(&history, &root, &project, id).is_ok());
    assert!(verify_qoder_history(&history, &root, &project, "another-session").is_err());
    assert!(verify_qoder_history(&history, &root, directory.path(), id).is_err());
    let outside = directory.path().join(format!("{id}.jsonl"));
    std::fs::write(&outside, &rows).unwrap();
    assert!(verify_qoder_history(&outside, &root, &project, id).is_err());
    assert!(qoder_history_path(&root, &project, "../other").is_err());
    #[cfg(unix)]
    {
      std::fs::remove_file(&history).unwrap();
      std::os::unix::fs::symlink(&outside, &history).unwrap();
      assert!(verify_qoder_history(&history, &root, &project, id).is_err());
    }
  }
}
