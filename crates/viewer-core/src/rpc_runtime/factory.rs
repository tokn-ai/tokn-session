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
