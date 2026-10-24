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

  pub(crate) async fn ensure_session(
    &self,
    request_id: &str,
    session_key: &str,
    provider: Provider,
    native_id: &str,
    cwd: &Path,
  ) -> Result<RpcSessionBinding, String> {
    self
      .0
      .runtimes
      .ensure_session(request_id, session_key, provider, native_id, cwd)
      .await
      .map(Into::into)
  }

  pub(crate) async fn execute_binding(
    &self,
    binding: &RpcSessionBinding,
    request_id: String,
    command: Command,
  ) -> Result<Reply, InteractionError> {
    self
      .0
      .runtimes
      .execute_binding(&binding.into(), request_id, command)
      .await
  }

  pub(crate) fn binding_snapshot(&self, key: &str) -> Option<RuntimeSnapshot> {
    self.0.runtimes.binding_snapshot(key)
  }

  pub(crate) async fn release_binding(&self, key: &str) -> Result<(), String> {
    self.0.runtimes.release_binding(key).await
  }
}

pub(super) fn select_profile<'a>(
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

pub(super) fn builtin_codex(cwd: &Path) -> Result<RpcResolvedProfile, String> {
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
