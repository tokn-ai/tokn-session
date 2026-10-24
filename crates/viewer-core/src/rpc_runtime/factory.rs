use super::*;
use std::{collections::BTreeMap, ffi::OsString, time::Duration};

#[derive(Clone)]
pub(super) enum Handle {
  Codex(Arc<tokn_codex_adapter::CodexAdapter>),
  Pi(Arc<tokn_pi_adapter::PiAdapter>),
  OpenCode(Arc<tokn_opencode_adapter::OpenCodeAdapter>),
  Dsh(Arc<tokn_dsh_adapter::DshAdapter>),
}
impl Handle {
  pub(super) fn adapter(&self) -> Arc<dyn InteractionAdapter> {
    match self {
      Self::Codex(adapter) => adapter.clone(),
      Self::Pi(adapter) => adapter.clone(),
      Self::OpenCode(adapter) => adapter.clone(),
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

  fn bind_native<'a>(&'a self, id: &'a str) -> AdapterFuture<'a, Result<(), String>> {
    Box::pin(Handle::bind_native(self, id))
  }
}

impl core_runtime::RuntimeHost for Host {
  type Profile = RpcResolvedProfile;

  fn resolve_profile(&self, profile_id: &str, revision: u64) -> Result<Self::Profile, String> {
    if let Some(profile) = self
      .transient_profiles
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .get(profile_id)
      .cloned()
    {
      if profile.revision != revision {
        return Err("Runtime profile changed before connection".into());
      }
      return Ok(profile);
    }
    self.profiles.resolve_at_revision(profile_id, revision)
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

  fn select_profile<'a>(
    &'a self,
    provider: Provider,
    cwd: &'a std::path::Path,
  ) -> AdapterFuture<'a, Result<(String, u64), String>> {
    Box::pin(async move {
      // Pi history requires a verified file and the owned-session driver.
      if provider == Provider::Pi {
        return Err("Continue this Pi session through its verified owned-session runtime".into());
      }
      let profiles = self.profiles.clone();
      let cwd = cwd.to_path_buf();
      let selected = tokio::task::spawn_blocking(move || {
        let snapshot = profiles.snapshot()?;
        let profile = super::workflow::select_profile(&snapshot, provider, &cwd)?;
        let Some(profile) = profile else {
          return Ok((None, snapshot.revision, cwd));
        };
        Ok::<_, String>((Some(profile.profile_id.clone()), snapshot.revision, cwd))
      })
      .await
      .map_err(|_| "Could not select a runtime profile")??;
      if let Some(profile_id) = selected.0 {
        return Ok((profile_id, selected.1));
      }
      let profile = tokio::task::spawn_blocking(move || super::workflow::builtin_codex(&selected.2))
        .await
        .map_err(|_| "Could not discover the local Codex executable")??;
      let profile_id = profile.profile.profile_id.clone();
      self
        .transient_profiles
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .entry(profile_id.clone())
        .or_insert(profile);
      Ok((profile_id, 0))
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
