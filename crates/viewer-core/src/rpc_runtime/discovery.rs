//! Configuration-only local executable discovery. Tests inject a snapshot rather
//! than changing process-wide environment variables or launching agent processes.
use super::*;
use sha2::{Digest, Sha256};
use std::{
  ffi::{OsStr, OsString},
  path::Path,
};

#[derive(Clone, Debug, Default)]
pub(crate) struct DiscoveryEnvironment {
  pub path: Option<OsString>,
  pub executables: BTreeMap<String, OsString>,
}

impl DiscoveryEnvironment {
  pub(crate) fn current() -> Self {
    Self {
      path: std::env::var_os("PATH"),
      executables: [Provider::Codex, Provider::OpenCode, Provider::Dsh]
        .into_iter()
        .filter_map(|provider| {
          let (_, _, key) = provider_command(provider).expect("discovered provider");
          std::env::var_os(key).map(|value| (key.to_owned(), value))
        })
        .collect(),
    }
  }
}

/// Explicit configuration never falls back to another binary if it is invalid.
/// The returned canonical path has been checked as executable, without running it.
pub(crate) fn resolve_executable(
  configured: Option<&OsStr>,
  search_path: Option<&OsStr>,
  command_name: &str,
) -> Result<PathBuf, String> {
  let command = configured
    .map(PathBuf::from)
    .unwrap_or_else(|| PathBuf::from(command_name));
  let path = if command.is_absolute() {
    executable_candidates(&command).find(|candidate| executable_ready(candidate))
  } else {
    search_path.and_then(|paths| {
      std::env::split_paths(paths)
        .filter(|directory| directory.is_absolute())
        .flat_map(|directory| executable_candidates(&directory.join(&command)))
        .find(|candidate| executable_ready(candidate))
    })
  }
  .ok_or_else(|| {
    if configured.is_some() {
      format!("The configured {command_name} executable is unavailable; fix its path or runtime profile")
    } else {
      format!("{command_name} is unavailable; install it or configure a runtime profile on this machine")
    }
  })?;
  std::fs::canonicalize(path).map_err(|_| format!("Could not resolve the {command_name} executable"))
}

fn executable_candidates(command: &Path) -> std::vec::IntoIter<PathBuf> {
  let mut candidates = vec![command.to_path_buf()];
  // The Pi driver and explicit environment overrides commonly supply bare
  // command names. Windows installations also expose package-manager shims.
  // Preserve explicit extensions: a missing configured file must not select
  // another command or silently replace its requested extension.
  if cfg!(windows) && command.extension().is_none() {
    candidates.extend(["exe", "com", "cmd", "bat"].map(|extension| command.with_extension(extension)));
  }
  candidates.into_iter()
}

fn executable_ready(path: &Path) -> bool {
  let Ok(metadata) = path.metadata() else { return false };
  if !metadata.is_file() {
    return false;
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 == 0 {
      return false;
    }
  }
  true
}

fn provider_command(provider: Provider) -> Result<(&'static str, &'static str, &'static str), String> {
  match provider {
    Provider::Codex => Ok(("Codex", "codex", "TOKN_SESSION_CODEX_EXECUTABLE")),
    Provider::OpenCode => Ok(("OpenCode", "opencode", "TOKN_SESSION_OPENCODE_EXECUTABLE")),
    Provider::Dsh => Ok(("DSH", "dsh", "TOKN_SESSION_DSH_EXECUTABLE")),
    Provider::Pi => Err("Pi uses its verified managed-session driver".into()),
  }
}

pub(super) fn builtin_profile(
  provider: Provider,
  cwd: &Path,
  environment: &DiscoveryEnvironment,
  installation_root: Option<&Path>,
) -> Result<RpcResolvedProfile, String> {
  let (name, command, key) = provider_command(provider)?;
  let cwd = std::fs::canonicalize(cwd).map_err(|_| "The session working directory is unavailable")?;
  if !cwd.is_dir() {
    return Err("The session working directory is unavailable".into());
  }
  // An explicit override is authoritative. Installed packages are checked only
  // before ordinary PATH discovery, and never downloaded by resolution.
  let installed = if environment.executables.contains_key(key) {
    None
  } else {
    installation_root
      .map(|root| crate::runtime_install::launch_spec(root, provider))
      .transpose()?
      .flatten()
  };
  let (executable, arguments, installation) = if let Some(spec) = installed {
    (
      spec.executable,
      spec.arguments,
      Some(RpcInstallationPin {
        root: installation_root.expect("installed root").to_path_buf(),
        version: spec.version,
        generation: spec.generation,
      }),
    )
  } else {
    (
      resolve_executable(
        environment.executables.get(key).map(OsString::as_os_str),
        environment.path.as_deref(),
        command,
      )?,
      Vec::new(),
      None,
    )
  };
  let installation = installation.or(
    installation_root
      .map(|root| installation_pin(root, provider, &executable, &arguments))
      .transpose()?
      .flatten(),
  );
  let mut profile = RpcResolvedProfile {
    revision: 0,
    profile: RpcRuntimeProfile {
      profile_id: uuid::Uuid::nil().to_string(),
      name: format!("Local {name}"),
      provider,
      mode: RpcRuntimeMode::Managed,
      executable: Some(
        executable
          .to_str()
          .ok_or("Runtime executable paths must be valid Unicode")?
          .to_owned(),
      ),
      arguments,
      working_directory: Some(
        cwd
          .to_str()
          .ok_or("Runtime project paths must be valid Unicode")?
          .to_owned(),
      ),
      endpoint: None,
      environment_keys: Vec::new(),
      credential_kind: None,
      is_default: false,
    },
    environment: BTreeMap::new(),
    credentials: RpcCredentials::None,
    installation,
  };
  profile.profile.profile_id = configuration_id(&profile)?;
  profile.validate_launch_ready()?;
  Ok(profile)
}

/// Discovered profiles are immutable executable/configuration snapshots. A
/// replaced binary must not silently take over an already verified catalog.
pub(super) fn validate_builtin(profile: &RpcResolvedProfile) -> Result<(), String> {
  profile.validate_launch_ready()?;
  if let Some(pin) = &profile.installation {
    let current = crate::runtime_install::launch_spec(&pin.root, profile.profile.provider)?;
    if !current.is_some_and(|spec| spec.generation == pin.generation) {
      return Err("The managed agent installation changed; refresh the runtime or model selection".into());
    }
  }
  if configuration_id(profile)? != profile.profile.profile_id {
    return Err("The discovered executable changed; refresh the runtime or model selection".into());
  }
  Ok(())
}

/// Saved profiles and explicit overrides may also launch files inside an
/// installed package, including Node outside that tree with a script argument.
pub(crate) fn installation_pin(
  root: &Path,
  provider: Provider,
  executable: &Path,
  arguments: &[String],
) -> Result<Option<RpcInstallationPin>, String> {
  let directory = root.join(match provider {
    Provider::Pi => "pi",
    Provider::Codex => "codex",
    Provider::OpenCode => "opencode",
    Provider::Dsh => "dsh",
  });
  let canonical = std::fs::canonicalize(&directory).ok();
  let within = |path: &Path| {
    path.starts_with(&directory)
      || canonical
        .as_ref()
        .is_some_and(|directory| std::fs::canonicalize(path).is_ok_and(|path| path.starts_with(directory)))
  };
  if !within(executable) && !arguments.iter().any(|argument| within(Path::new(argument))) {
    return Ok(None);
  }
  let spec = crate::runtime_install::launch_spec(root, provider)?
    .ok_or("The configured managed agent installation is unavailable; download it or update the profile")?;
  Ok(Some(RpcInstallationPin {
    root: root.to_owned(),
    version: spec.version,
    generation: spec.generation,
  }))
}

fn configuration_id(profile: &RpcResolvedProfile) -> Result<String, String> {
  let executable = profile
    .profile
    .executable
    .as_deref()
    .ok_or("Discovered runtime executable is missing")?;
  let mut environment = profile.environment.clone();
  if let Some(pin) = &profile.installation {
    environment.insert("TOKN_INSTALLATION_GENERATION".into(), pin.generation.clone());
  }
  configuration_digest(
    profile.profile.provider,
    profile.profile.working_directory.as_deref(),
    Path::new(executable),
    &profile.profile.arguments,
    &environment,
  )
}

pub(crate) fn launch_configuration_id(
  provider: Provider,
  cwd: &Path,
  executable: &Path,
  arguments: &[String],
  generation: Option<&str>,
) -> Result<String, String> {
  let cwd = std::fs::canonicalize(cwd).map_err(|_| "The runtime project folder is unavailable")?;
  let environment = generation
    .map(|generation| BTreeMap::from([("TOKN_INSTALLATION_GENERATION".into(), generation.into())]))
    .unwrap_or_default();
  configuration_digest(
    provider,
    Some(cwd.to_str().ok_or("Runtime project paths must be valid Unicode")?),
    executable,
    arguments,
    &environment,
  )
}

fn configuration_digest(
  provider: Provider,
  cwd: Option<&str>,
  executable: &Path,
  arguments: &[String],
  environment: &BTreeMap<String, String>,
) -> Result<String, String> {
  let canonical = std::fs::canonicalize(executable).map_err(|_| "The discovered executable is unavailable")?;
  let metadata = canonical
    .metadata()
    .map_err(|_| "Could not inspect the discovered executable")?;
  let modified = metadata
    .modified()
    .ok()
    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
    .map(|time| time.as_nanos());
  let mut digest = Sha256::new();
  digest.update(b"tokn:local-managed-runtime:v2\0");
  digest.update(
    serde_json::to_vec(&(
      provider,
      cwd,
      canonical
        .to_str()
        .ok_or("Runtime executable paths must be valid Unicode")?,
      arguments,
      environment,
      metadata.len(),
      modified,
    ))
    .map_err(|_| "Could not fingerprint local runtime configuration")?,
  );
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    digest.update(metadata.dev().to_le_bytes());
    digest.update(metadata.ino().to_le_bytes());
  }
  let mut bytes: [u8; 16] = digest.finalize()[..16].try_into().expect("SHA-256 prefix");
  bytes[6] = (bytes[6] & 0x0f) | 0x80;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  Ok(uuid::Uuid::from_bytes(bytes).to_string())
}
