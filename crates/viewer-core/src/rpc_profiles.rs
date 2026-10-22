//! Host-owned runtime configuration. Public snapshots expose secret names and
//! kinds, while secret values are available only to the host runtime factory.
use serde::{Deserialize, Serialize};
use std::{
  collections::{BTreeMap, BTreeSet},
  path::{Path, PathBuf},
  sync::Mutex,
};
use tokn_interaction_core::Provider;
use tokn_session_storage::{read_private_json, update_private_json};

const VERSION: u32 = 1;
const MAX_BYTES: u64 = 256 * 1024;
const MAX_PROFILES: usize = 32;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RpcRuntimeMode {
  Managed,
  Attached,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RpcRuntimeProfile {
  pub profile_id: String,
  pub name: String,
  pub provider: Provider,
  pub mode: RpcRuntimeMode,
  pub executable: Option<String>,
  pub arguments: Vec<String>,
  pub working_directory: Option<String>,
  pub endpoint: Option<String>,
  pub environment_keys: Vec<String>,
  pub credential_kind: Option<String>,
  pub is_default: bool,
}

/// Write-only credentials. Deliberately has no Debug implementation, including
/// on all containing input and private storage records.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RpcCredentials {
  #[default]
  None,
  Bearer {
    token: String,
  },
  Basic {
    username: String,
    password: String,
  },
}

impl RpcCredentials {
  fn kind(&self) -> Option<String> {
    match self {
      Self::None => None,
      Self::Bearer { .. } => Some("bearer".into()),
      Self::Basic { .. } => Some("basic".into()),
    }
  }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcRuntimeProfileInput {
  pub profile_id: Option<String>,
  pub name: String,
  pub provider: Provider,
  pub mode: RpcRuntimeMode,
  pub executable: Option<String>,
  #[serde(default)]
  pub arguments: Vec<String>,
  pub working_directory: Option<String>,
  pub endpoint: Option<String>,
  /// None retains existing values; an empty map clears them.
  pub environment: Option<BTreeMap<String, String>>,
  /// None retains existing values; `type: none` clears them.
  pub credentials: Option<RpcCredentials>,
  #[serde(default)]
  pub is_default: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RpcRuntimeProfilesSnapshot {
  pub version: u32,
  pub revision: u64,
  pub profiles: Vec<RpcRuntimeProfile>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcSaveProfileRequest {
  pub expected_revision: u64,
  pub profile: RpcRuntimeProfileInput,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcRemoveProfileRequest {
  pub expected_revision: u64,
  pub profile_id: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RpcProfileValidationIssue {
  pub field: String,
  pub message: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RpcProfileValidation {
  pub valid: bool,
  pub issues: Vec<RpcProfileValidationIssue>,
  pub executable_exists: Option<bool>,
  pub working_directory_exists: Option<bool>,
}

/// Host-only launch input, deliberately neither serializable nor Debug.
#[derive(Clone)]
pub struct RpcResolvedProfile {
  pub revision: u64,
  pub profile: RpcRuntimeProfile,
  pub environment: BTreeMap<String, String>,
  pub credentials: RpcCredentials,
}

impl RpcResolvedProfile {
  /// A bounded filesystem preflight before managed launch. Attached endpoints
  /// are never probed here, and diagnostics never include configuration values.
  pub fn validate_launch_ready(&self) -> Result<(), String> {
    if self.profile.mode != RpcRuntimeMode::Managed {
      return Ok(());
    }
    if !self
      .profile
      .executable
      .as_ref()
      .is_some_and(|path| absolute_path(path) && executable_ready(Path::new(path)))
    {
      return Err("Runtime executable is unavailable; validate the profile on this machine".into());
    }
    if !self
      .profile
      .working_directory
      .as_ref()
      .is_some_and(|path| absolute_path(path) && Path::new(path).is_dir())
    {
      return Err("Runtime working directory is unavailable; validate the profile on this machine".into());
    }
    Ok(())
  }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredProfile {
  profile_id: String,
  name: String,
  provider: Provider,
  mode: RpcRuntimeMode,
  executable: Option<String>,
  arguments: Vec<String>,
  working_directory: Option<String>,
  endpoint: Option<String>,
  environment: BTreeMap<String, String>,
  credentials: RpcCredentials,
  is_default: bool,
}

impl StoredProfile {
  fn public(&self) -> RpcRuntimeProfile {
    RpcRuntimeProfile {
      profile_id: self.profile_id.clone(),
      name: self.name.clone(),
      provider: self.provider,
      mode: self.mode,
      executable: self.executable.clone(),
      arguments: self.arguments.clone(),
      working_directory: self.working_directory.clone(),
      endpoint: self.endpoint.clone(),
      environment_keys: self.environment.keys().cloned().collect(),
      credential_kind: self.credentials.kind(),
      is_default: self.is_default,
    }
  }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredProfiles {
  version: u32,
  revision: u64,
  profiles: Vec<StoredProfile>,
}

impl Default for StoredProfiles {
  fn default() -> Self {
    Self {
      version: VERSION,
      revision: 0,
      profiles: Vec::new(),
    }
  }
}

impl StoredProfiles {
  fn snapshot(&self) -> RpcRuntimeProfilesSnapshot {
    RpcRuntimeProfilesSnapshot {
      version: self.version,
      revision: self.revision,
      profiles: self.profiles.iter().map(StoredProfile::public).collect(),
    }
  }

  fn validate(&self) -> Result<(), String> {
    if self.version != VERSION || self.profiles.len() > MAX_PROFILES {
      return Err("Unsupported or oversized runtime profile storage; restore it before making changes".into());
    }
    let mut ids = BTreeSet::new();
    let mut defaults = Vec::new();
    for profile in &self.profiles {
      if !ids.insert(profile.profile_id.clone())
        || !schema_issues(profile).is_empty()
        || (profile.is_default && defaults.contains(&profile.provider))
      {
        return Err("Invalid runtime profile storage; restore it before making changes".into());
      }
      if profile.is_default {
        defaults.push(profile.provider);
      }
    }
    Ok(())
  }

  fn resolve_input(&self, input: RpcRuntimeProfileInput) -> Result<StoredProfile, String> {
    let previous = if let Some(id) = &input.profile_id {
      Some(
        self
          .profiles
          .iter()
          .find(|profile| profile.profile_id == *id)
          .ok_or("Runtime profile no longer exists; reload the configuration")?,
      )
    } else {
      None
    };
    Ok(StoredProfile {
      profile_id: input.profile_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
      name: input.name,
      provider: input.provider,
      mode: input.mode,
      executable: input.executable,
      arguments: input.arguments,
      working_directory: input.working_directory,
      endpoint: input.endpoint,
      environment: input
        .environment
        .unwrap_or_else(|| previous.map(|profile| profile.environment.clone()).unwrap_or_default()),
      credentials: input
        .credentials
        .unwrap_or_else(|| previous.map(|profile| profile.credentials.clone()).unwrap_or_default()),
      is_default: input.is_default,
    })
  }
}

/// None selects a private in-memory store, useful for isolated viewer instances.
/// Disk snapshots reread their owner file so other writers cannot remain hidden.
pub struct RpcProfileStore {
  path: Option<PathBuf>,
  memory: Mutex<StoredProfiles>,
}

impl RpcProfileStore {
  /// Bind a storage location without reading it. Operations retry inspection on
  /// every access, so an operator can repair invalid files without restarting.
  pub fn configured(path: Option<PathBuf>) -> Self {
    Self {
      path,
      memory: Mutex::new(StoredProfiles::default()),
    }
  }

  /// Pure inspection: never creates the file, its parent, or a lock file.
  pub fn new(path: Option<PathBuf>) -> Result<Self, String> {
    let store = Self::configured(path);
    store.read()?;
    Ok(store)
  }

  fn read(&self) -> Result<StoredProfiles, String> {
    let value: StoredProfiles = match &self.path {
      Some(path) => read_private_json(path, MAX_BYTES)?.unwrap_or_default(),
      None => self
        .memory
        .lock()
        .map_err(|_| "Runtime profile store is unavailable")?
        .clone(),
    };
    value.validate()?;
    Ok(value)
  }

  pub fn snapshot(&self) -> Result<RpcRuntimeProfilesSnapshot, String> {
    Ok(self.read()?.snapshot())
  }

  pub fn resolve(&self, profile_id: &str) -> Result<RpcResolvedProfile, String> {
    Self::resolve_stored(self.read()?, profile_id)
  }

  pub fn resolve_at_revision(&self, profile_id: &str, expected_revision: u64) -> Result<RpcResolvedProfile, String> {
    let stored = self.read()?;
    if stored.revision != expected_revision {
      return Err("Runtime profiles changed on this machine; reload before connecting".into());
    }
    Self::resolve_stored(stored, profile_id)
  }

  fn resolve_stored(stored: StoredProfiles, profile_id: &str) -> Result<RpcResolvedProfile, String> {
    let revision = stored.revision;
    let stored = stored
      .profiles
      .into_iter()
      .find(|profile| profile.profile_id == profile_id)
      .ok_or("Runtime profile no longer exists; reload the configuration")?;
    Ok(RpcResolvedProfile {
      revision,
      profile: stored.public(),
      environment: stored.environment,
      credentials: stored.credentials,
    })
  }

  /// Checks the selected host's files only; never launches a process or contacts
  /// an endpoint. Saved paths may be temporarily absent without corrupting storage.
  pub fn validate(&self, input: RpcRuntimeProfileInput) -> Result<RpcProfileValidation, String> {
    let profile = self.read()?.resolve_input(input)?;
    let mut issues = schema_issues(&profile);
    let executable_exists = profile.executable.as_ref().map(|executable| {
      let ready = executable_ready(Path::new(executable));
      if !ready {
        issue(
          &mut issues,
          "executable",
          "Choose an existing executable file on this machine",
        );
      }
      ready
    });
    let working_directory_exists = profile.working_directory.as_ref().map(|directory| {
      let ready = Path::new(directory).is_dir();
      if !ready {
        issue(
          &mut issues,
          "working_directory",
          "Choose an existing directory on this machine",
        );
      }
      ready
    });
    Ok(RpcProfileValidation {
      valid: issues.is_empty(),
      issues,
      executable_exists,
      working_directory_exists,
    })
  }

  pub fn save(&self, request: RpcSaveProfileRequest) -> Result<RpcRuntimeProfilesSnapshot, String> {
    self.update(request.expected_revision, |stored| {
      let profile = stored.resolve_input(request.profile)?;
      if !schema_issues(&profile).is_empty() {
        return Err("Runtime profile is invalid; validate it and correct the reported fields".into());
      }
      let existing = stored
        .profiles
        .iter()
        .position(|item| item.profile_id == profile.profile_id);
      if existing.is_none() && stored.profiles.len() >= MAX_PROFILES {
        return Err("A machine supports at most 32 runtime profiles".into());
      }
      if profile.is_default {
        for other in &mut stored.profiles {
          if other.provider == profile.provider {
            other.is_default = false;
          }
        }
      }
      if let Some(index) = existing {
        stored.profiles[index] = profile;
      } else {
        stored.profiles.push(profile);
      }
      Ok(())
    })
  }

  pub fn remove(&self, request: RpcRemoveProfileRequest) -> Result<RpcRuntimeProfilesSnapshot, String> {
    self.update(request.expected_revision, |stored| {
      let index = stored
        .profiles
        .iter()
        .position(|profile| profile.profile_id == request.profile_id)
        .ok_or("Runtime profile no longer exists; reload the configuration")?;
      stored.profiles.remove(index);
      Ok(())
    })
  }

  fn update(
    &self,
    expected_revision: u64,
    update: impl FnOnce(&mut StoredProfiles) -> Result<(), String>,
  ) -> Result<RpcRuntimeProfilesSnapshot, String> {
    let apply = |mut stored: StoredProfiles| {
      stored.validate()?;
      if stored.revision != expected_revision {
        return Err("Runtime profiles changed on this machine; reload before saving".into());
      }
      update(&mut stored)?;
      stored.revision = stored
        .revision
        .checked_add(1)
        .ok_or("Runtime profile revision limit reached")?;
      stored.validate()?;
      Ok(stored)
    };
    let stored = if let Some(path) = &self.path {
      update_private_json(path, MAX_BYTES, |previous| apply(previous.unwrap_or_default()))?
    } else {
      let mut memory = self.memory.lock().map_err(|_| "Runtime profile store is unavailable")?;
      let next = apply(memory.clone())?;
      if serde_json::to_vec_pretty(&next)
        .map_err(|_| "Could not encode runtime profiles")?
        .len() as u64
        > MAX_BYTES
      {
        return Err("Updated runtime profiles exceed their size limit".into());
      }
      *memory = next.clone();
      next
    };
    Ok(stored.snapshot())
  }
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

fn issue(issues: &mut Vec<RpcProfileValidationIssue>, field: &str, message: &str) {
  issues.push(RpcProfileValidationIssue {
    field: field.into(),
    message: message.into(),
  });
}

fn absolute_path(value: &str) -> bool {
  value.len() <= 4096 && !value.contains('\0') && Path::new(value).is_absolute()
}

fn schema_issues(profile: &StoredProfile) -> Vec<RpcProfileValidationIssue> {
  let mut issues = Vec::new();
  if uuid::Uuid::parse_str(&profile.profile_id).is_err() {
    issue(&mut issues, "profile_id", "Invalid runtime profile identifier");
  }
  if profile.name.trim().is_empty() || profile.name.len() > 128 || profile.name.chars().any(char::is_control) {
    issue(
      &mut issues,
      "name",
      "Enter a name with at most 128 bytes and no control characters",
    );
  }
  if profile.arguments.len() > 128
    || profile
      .arguments
      .iter()
      .any(|arg| arg.len() > 4096 || arg.contains('\0'))
  {
    issue(
      &mut issues,
      "arguments",
      "Arguments exceed their limits or contain a null character",
    );
  }
  if profile
    .working_directory
    .as_ref()
    .is_some_and(|path| !absolute_path(path))
  {
    issue(
      &mut issues,
      "working_directory",
      "Working directory must be an absolute path on this machine",
    );
  }
  match profile.mode {
    RpcRuntimeMode::Managed => {
      if profile.executable.as_ref().is_none_or(|path| !absolute_path(path)) {
        issue(
          &mut issues,
          "executable",
          "Managed runtimes require an absolute executable path on this machine",
        );
      }
      if profile.working_directory.is_none() {
        issue(
          &mut issues,
          "working_directory",
          "Managed runtimes require a working directory",
        );
      }
      if profile.endpoint.is_some() {
        issue(
          &mut issues,
          "endpoint",
          "Managed runtimes discover their endpoint from their owned process",
        );
      }
      if matches!(profile.provider, Provider::OpenCode | Provider::Dsh) && !profile.arguments.is_empty() {
        issue(
          &mut issues,
          "arguments",
          "This provider's managed launcher does not support extra arguments",
        );
      }
      if !matches!(profile.credentials, RpcCredentials::None) {
        issue(
          &mut issues,
          "credentials",
          "Managed runtime credentials must be supplied through its environment",
        );
      }
    }
    RpcRuntimeMode::Attached => {
      if profile.provider == Provider::Pi {
        issue(
          &mut issues,
          "mode",
          "Pi attachment requires supplied process pipes and cannot use a saved endpoint",
        );
      }
      if profile.executable.is_some() || !profile.arguments.is_empty() {
        issue(
          &mut issues,
          "executable",
          "Attached runtimes use an endpoint instead of an executable or arguments",
        );
      }
      if !profile.environment.is_empty() {
        issue(
          &mut issues,
          "environment",
          "Environment overrides apply only to managed processes",
        );
      }
      if profile.provider == Provider::OpenCode && profile.working_directory.is_none() {
        issue(
          &mut issues,
          "working_directory",
          "OpenCode attachment requires its project working directory",
        );
      }
      let endpoint_ok = profile.endpoint.as_ref().is_some_and(|endpoint| {
        if endpoint.len() > 4096 || endpoint.chars().any(char::is_control) || endpoint.trim() != endpoint {
          return false;
        }
        // Url normalizes empty userinfo away; native WebSocket transports also
        // reject that authority form, so validate the original authority too.
        if endpoint.split_once("://").is_some_and(|(_, rest)| {
          rest
            .split(['/', '?', '#'])
            .next()
            .is_some_and(|authority| authority.contains('@'))
        }) {
          return false;
        }
        let Ok(url) = url::Url::parse(endpoint) else {
          return false;
        };
        let scheme_ok = match profile.provider {
          Provider::Codex => matches!(url.scheme(), "ws" | "wss"),
          Provider::OpenCode | Provider::Dsh => matches!(url.scheme(), "http" | "https") && url.path() == "/",
          Provider::Pi => false,
        };
        scheme_ok
          && url.host_str().is_some()
          && url.username().is_empty()
          && url.password().is_none()
          && url.query().is_none()
          && url.fragment().is_none()
      });
      if !endpoint_ok {
        issue(
          &mut issues,
          "endpoint",
          "Use a Codex WS(S) URL or an OpenCode/DSH HTTP(S) origin without credentials, query, or fragment",
        );
      }
      if !matches!(profile.credentials, RpcCredentials::None)
        && !matches!(
          (&profile.credentials, profile.provider),
          (RpcCredentials::Basic { .. }, Provider::OpenCode)
            | (RpcCredentials::Bearer { .. }, Provider::Codex | Provider::Dsh)
        )
      {
        issue(
          &mut issues,
          "credentials",
          "Use basic credentials for OpenCode or a bearer token for Codex/DSH",
        );
      }
    }
  }
  let environment_ok = profile.environment.len() <= 64
    && profile.environment.iter().all(|(key, value)| {
      !key.is_empty()
        && key.len() <= 128
        && key
          .bytes()
          .enumerate()
          .all(|(index, byte)| byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit()))
        && value.len() <= 16 * 1024
        && !value.contains('\0')
    })
    && profile
      .environment
      .iter()
      .map(|(key, value)| key.len() + value.len())
      .sum::<usize>()
      <= 64 * 1024;
  if !environment_ok {
    issue(
      &mut issues,
      "environment",
      "Environment overrides exceed their limits or contain invalid keys or values",
    );
  }
  let credentials_ok = match &profile.credentials {
    RpcCredentials::None => true,
    RpcCredentials::Bearer { token } => {
      !token.is_empty() && token.len() <= 16 * 1024 && token.bytes().all(|byte| matches!(byte, 0x21..=0x7e))
    }
    RpcCredentials::Basic { username, password } => {
      !username.is_empty()
        && username.len() <= 1024
        && !username.contains(':')
        && !username.chars().any(char::is_control)
        && password.len() <= 16 * 1024
        && !password.chars().any(char::is_control)
    }
  };
  if !credentials_ok {
    issue(
      &mut issues,
      "credentials",
      "Credentials are empty, oversized, or contain unsupported characters",
    );
  }
  issues
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  fn managed(directory: &Path) -> RpcRuntimeProfileInput {
    RpcRuntimeProfileInput {
      profile_id: None,
      name: "Local Pi".into(),
      provider: Provider::Pi,
      mode: RpcRuntimeMode::Managed,
      executable: Some(std::env::current_exe().unwrap().to_string_lossy().into_owned()),
      arguments: Vec::new(),
      working_directory: Some(directory.to_string_lossy().into_owned()),
      endpoint: None,
      environment: None,
      credentials: None,
      is_default: true,
    }
  }

  fn attached() -> RpcRuntimeProfileInput {
    RpcRuntimeProfileInput {
      profile_id: None,
      name: "Codex remote".into(),
      provider: Provider::Codex,
      mode: RpcRuntimeMode::Attached,
      executable: None,
      arguments: Vec::new(),
      working_directory: None,
      endpoint: Some("wss://codex.example/rpc".into()),
      environment: None,
      credentials: Some(RpcCredentials::Bearer {
        token: "do-not-disclose-this-bearer".into(),
      }),
      is_default: true,
    }
  }

  fn save(store: &RpcProfileStore, profile: RpcRuntimeProfileInput) -> RpcRuntimeProfilesSnapshot {
    store
      .save(RpcSaveProfileRequest {
        expected_revision: store.snapshot().unwrap().revision,
        profile,
      })
      .unwrap()
  }

  #[test]
  fn inspection_and_validation_do_not_create_storage_or_launch_processes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("viewer/rpc-profiles.json");
    let store = RpcProfileStore::new(Some(path.clone())).unwrap();
    assert_eq!(store.snapshot().unwrap().revision, 0);
    assert!(store.snapshot().unwrap().profiles.is_empty());
    let checked = store.validate(managed(directory.path())).unwrap();
    assert!(checked.valid, "{:?}", checked.issues);
    assert_eq!(checked.executable_exists, Some(true));
    assert_eq!(checked.working_directory_exists, Some(true));
    assert!(!path.parent().unwrap().exists());
  }

  #[test]
  fn public_snapshots_and_validation_never_return_environment_or_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("profiles.json");
    let store = RpcProfileStore::new(Some(path.clone())).unwrap();
    let mut local = managed(directory.path());
    local.environment = Some(BTreeMap::from([("API_KEY".into(), "private-env-value".into())]));
    let first = save(&store, local);
    let local_id = first.profiles[0].profile_id.clone();
    let saved = save(&store, attached());
    let public = serde_json::to_string(&saved).unwrap();
    assert!(public.contains("API_KEY"));
    assert!(public.contains("bearer"));
    assert!(!public.contains("private-env-value"));
    assert!(!public.contains("do-not-disclose-this-bearer"));
    assert!(!format!("{saved:?}").contains("private-env-value"));
    assert_eq!(
      store.resolve(&local_id).unwrap().environment["API_KEY"],
      "private-env-value"
    );
    let disk = std::fs::read_to_string(path).unwrap();
    assert!(disk.contains("private-env-value"));
    assert!(disk.contains("do-not-disclose-this-bearer"));
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      assert_eq!(
        std::fs::metadata(store.path.as_ref().unwrap())
          .unwrap()
          .permissions()
          .mode()
          & 0o777,
        0o600
      );
    }
  }

  #[test]
  fn retained_secrets_are_resolved_host_side_and_can_be_explicitly_cleared() {
    let directory = tempfile::tempdir().unwrap();
    let store = RpcProfileStore::new(None).unwrap();
    let mut local = managed(directory.path());
    local.environment = Some(BTreeMap::from([("TOKEN".into(), "private".into())]));
    let saved = save(&store, local.clone());
    local.profile_id = Some(saved.profiles[0].profile_id.clone());
    local.environment = None;
    local.name = "Renamed".into();
    assert!(store.validate(local.clone()).unwrap().valid);
    assert_eq!(save(&store, local.clone()).profiles[0].environment_keys, vec!["TOKEN"]);
    local.environment = Some(BTreeMap::new());
    assert!(save(&store, local).profiles[0].environment_keys.is_empty());

    let mut remote = attached();
    let saved = save(&store, remote.clone());
    let id = saved.profiles.last().unwrap().profile_id.clone();
    remote.profile_id = Some(id.clone());
    remote.credentials = None;
    assert_eq!(
      save(&store, remote.clone())
        .profiles
        .last()
        .unwrap()
        .credential_kind
        .as_deref(),
      Some("bearer")
    );
    assert!(matches!(
      store.resolve(&id).unwrap().credentials,
      RpcCredentials::Bearer { .. }
    ));
    remote.credentials = Some(RpcCredentials::None);
    assert_eq!(save(&store, remote).profiles.last().unwrap().credential_kind, None);
  }

  #[test]
  fn revisions_detect_independent_writers_and_defaults_are_atomic_per_provider() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("profiles.json");
    let first = RpcProfileStore::new(Some(path.clone())).unwrap();
    let second = RpcProfileStore::new(Some(path)).unwrap();
    save(&first, managed(directory.path()));
    assert_eq!(second.snapshot().unwrap().revision, 1);
    assert!(
      second
        .save(RpcSaveProfileRequest {
          expected_revision: 0,
          profile: attached()
        })
        .is_err()
    );
    let mut another = managed(directory.path());
    another.name = "Second Pi".into();
    let saved = save(&second, another);
    assert!(!saved.profiles[0].is_default);
    assert!(saved.profiles[1].is_default);
    let saved = save(&first, attached());
    assert!(saved.profiles[1].is_default);
    assert!(saved.profiles[2].is_default);
    assert_eq!(first.snapshot().unwrap(), second.snapshot().unwrap());
    assert!(first.resolve_at_revision(&saved.profiles[0].profile_id, 1).is_err());
    assert_eq!(
      first
        .resolve_at_revision(&saved.profiles[0].profile_id, 3)
        .unwrap()
        .revision,
      3
    );
  }

  #[test]
  fn deletion_uses_revision_and_never_reassigns_a_default_implicitly() {
    let directory = tempfile::tempdir().unwrap();
    let store = RpcProfileStore::new(None).unwrap();
    let saved = save(&store, managed(directory.path()));
    let id = saved.profiles[0].profile_id.clone();
    assert!(
      store
        .remove(RpcRemoveProfileRequest {
          expected_revision: 0,
          profile_id: id.clone()
        })
        .is_err()
    );
    let removed = store
      .remove(RpcRemoveProfileRequest {
        expected_revision: 1,
        profile_id: id.clone(),
      })
      .unwrap();
    assert_eq!(removed.revision, 2);
    assert!(removed.profiles.is_empty());
    assert!(store.resolve(&id).is_err());
    assert!(
      store
        .remove(RpcRemoveProfileRequest {
          expected_revision: 2,
          profile_id: id
        })
        .is_err()
    );
    assert_eq!(store.snapshot().unwrap().revision, 2);
  }

  #[test]
  fn malformed_or_semantically_invalid_storage_is_not_replaced() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("profiles.json");
    let store = RpcProfileStore::new(Some(path.clone())).unwrap();
    save(&store, attached());
    let original = std::fs::read_to_string(&path).unwrap();
    let mut duplicate_defaults: serde_json::Value = serde_json::from_str(&original).unwrap();
    let mut duplicate = duplicate_defaults["profiles"][0].clone();
    duplicate["profile_id"] = json!(uuid::Uuid::new_v4().to_string());
    duplicate_defaults["profiles"].as_array_mut().unwrap().push(duplicate);
    for invalid in [
      "broken".to_owned(),
      original.replace("\"version\": 1", "\"version\": 999"),
      original.replace("wss://codex.example/rpc", "file:///private-secret"),
      serde_json::to_string(&duplicate_defaults).unwrap(),
    ] {
      std::fs::write(&path, &invalid).unwrap();
      assert!(RpcProfileStore::new(Some(path.clone())).is_err());
      let error = store
        .save(RpcSaveProfileRequest {
          expected_revision: 1,
          profile: attached(),
        })
        .unwrap_err();
      assert!(!error.contains("private-secret"));
      assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
    }
  }

  #[test]
  fn configured_store_recovers_after_operator_repairs_invalid_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("profiles.json");
    let valid = RpcProfileStore::new(Some(path.clone())).unwrap();
    save(&valid, attached());
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, "broken").unwrap();
    let store = RpcProfileStore::configured(Some(path.clone()));
    assert!(store.snapshot().is_err());
    assert!(store.validate(attached()).is_err());
    std::fs::write(&path, original).unwrap();
    assert_eq!(store.snapshot().unwrap().profiles.len(), 1);
    assert!(store.validate(attached()).unwrap().valid);
  }

  #[test]
  fn host_path_validation_does_not_make_saved_config_unreadable_when_binary_disappears() {
    let directory = tempfile::tempdir().unwrap();
    let store = RpcProfileStore::new(None).unwrap();
    let mut input = managed(directory.path());
    input.executable = Some(directory.path().join("not-installed").to_string_lossy().into_owned());
    input.working_directory = Some(directory.path().join("missing-folder").to_string_lossy().into_owned());
    let result = store.validate(input.clone()).unwrap();
    assert!(!result.valid);
    assert_eq!(result.executable_exists, Some(false));
    assert_eq!(result.working_directory_exists, Some(false));
    let saved = save(&store, input);
    assert_eq!(saved.profiles.len(), 1);
    assert!(
      store
        .resolve(&saved.profiles[0].profile_id)
        .unwrap()
        .validate_launch_ready()
        .is_err()
    );
    assert_eq!(store.snapshot().unwrap().profiles.len(), 1);
  }

  #[test]
  fn launch_preflight_checks_managed_files_and_does_not_probe_attached_endpoints() {
    let directory = tempfile::tempdir().unwrap();
    let store = RpcProfileStore::new(None).unwrap();
    let local = save(&store, managed(directory.path()));
    assert!(
      store
        .resolve(&local.profiles[0].profile_id)
        .unwrap()
        .validate_launch_ready()
        .is_ok()
    );
    let remote = save(&store, attached());
    assert!(
      store
        .resolve(&remote.profiles[1].profile_id)
        .unwrap()
        .validate_launch_ready()
        .is_ok()
    );
  }

  #[test]
  fn endpoint_validation_rejects_secret_bearing_urls_without_echoing_values() {
    let store = RpcProfileStore::new(None).unwrap();
    for endpoint in [
      "https://codex.example",
      "wss://@codex.example",
      "wss://user:secret-value@codex.example",
      "wss://codex.example/?token=secret-value",
      "wss://codex.example/#secret-value",
      "wss://codex.example/\n",
    ] {
      let mut input = attached();
      input.endpoint = Some(endpoint.into());
      let result = store.validate(input.clone()).unwrap();
      assert!(!result.valid, "accepted {endpoint}");
      assert!(!serde_json::to_string(&result).unwrap().contains("secret-value"));
      assert!(
        store
          .save(RpcSaveProfileRequest {
            expected_revision: 0,
            profile: input
          })
          .is_err()
      );
    }
    assert!(store.snapshot().unwrap().profiles.is_empty());
  }

  #[test]
  fn provider_transport_and_authentication_constraints_are_not_silently_ignored() {
    let directory = tempfile::tempdir().unwrap();
    let store = RpcProfileStore::new(None).unwrap();
    let mut input = attached();
    input.provider = Provider::Pi;
    assert!(!store.validate(input.clone()).unwrap().valid);
    input.provider = Provider::OpenCode;
    input.endpoint = Some("https://opencode.example".into());
    input.working_directory = Some(directory.path().to_string_lossy().into_owned());
    assert!(
      !store.validate(input.clone()).unwrap().valid,
      "bearer is not basic auth"
    );
    input.credentials = Some(RpcCredentials::Basic {
      username: "opencode".into(),
      password: "secret".into(),
    });
    assert!(store.validate(input.clone()).unwrap().valid);
    input.endpoint = Some("https://opencode.example/nested".into());
    assert!(!store.validate(input).unwrap().valid);
    for provider in [Provider::OpenCode, Provider::Dsh] {
      let mut input = managed(directory.path());
      input.provider = provider;
      input.arguments.push("unsupported".into());
      assert!(!store.validate(input).unwrap().valid);
    }
    let mut input = attached();
    input.environment = Some(BTreeMap::from([("TOKEN".into(), "secret".into())]));
    assert!(!store.validate(input).unwrap().valid);
  }

  #[test]
  fn invalid_secret_values_and_oversized_profiles_fail_without_advancing_revision() {
    let directory = tempfile::tempdir().unwrap();
    let store = RpcProfileStore::new(None).unwrap();
    let mut input = managed(directory.path());
    input.environment = Some(BTreeMap::from([("BAD=KEY".into(), "secret\0value".into())]));
    let checked = store.validate(input.clone()).unwrap();
    assert!(!checked.valid);
    assert!(!serde_json::to_string(&checked).unwrap().contains("secret"));
    assert!(
      store
        .save(RpcSaveProfileRequest {
          expected_revision: 0,
          profile: input
        })
        .is_err()
    );
    assert_eq!(store.snapshot().unwrap().revision, 0);
    let mut oversized = managed(directory.path());
    oversized.arguments = vec!["x".repeat(4096); 128];
    assert!(
      store
        .save(RpcSaveProfileRequest {
          expected_revision: 0,
          profile: oversized,
        })
        .is_err()
    );
    assert_eq!(store.snapshot().unwrap().revision, 0);
    for _ in 0..MAX_PROFILES {
      save(&store, managed(directory.path()));
    }
    assert!(
      store
        .save(RpcSaveProfileRequest {
          expected_revision: MAX_PROFILES as u64,
          profile: managed(directory.path())
        })
        .is_err()
    );
    assert_eq!(store.snapshot().unwrap().revision, MAX_PROFILES as u64);
  }

  #[test]
  fn retained_credentials_are_validated_after_changing_provider() {
    let directory = tempfile::tempdir().unwrap();
    let store = RpcProfileStore::new(None).unwrap();
    let mut input = attached();
    let saved = save(&store, input.clone());
    input.profile_id = Some(saved.profiles[0].profile_id.clone());
    input.credentials = None;
    input.provider = Provider::OpenCode;
    input.endpoint = Some("http://127.0.0.1:4096".into());
    input.working_directory = Some(directory.path().to_string_lossy().into_owned());
    assert!(!store.validate(input.clone()).unwrap().valid);
    input.credentials = Some(RpcCredentials::None);
    assert!(store.validate(input).unwrap().valid);
  }

  #[test]
  fn json_contract_has_snake_case_provider_and_explicit_credential_clear() {
    let input: RpcRuntimeProfileInput = serde_json::from_value(json!({
      "name": "OpenCode", "provider": "open_code", "mode": "attached",
      "endpoint": "https://opencode.example", "working_directory": "/tmp",
      "credentials": {"type": "none"}
    }))
    .unwrap();
    assert_eq!(input.provider, Provider::OpenCode);
    assert!(matches!(input.credentials, Some(RpcCredentials::None)));
    assert_eq!(serde_json::to_value(input).unwrap()["provider"], "open_code");
  }
}
