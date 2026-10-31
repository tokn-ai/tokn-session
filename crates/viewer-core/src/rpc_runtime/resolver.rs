//! One host policy for availability, model catalogs and first input. Resolution
//! reads configuration/metadata only; runtime startup stays in the coordinator.
use super::*;
use std::path::Path;
use tokn_interaction_core::runtime_plan::{
  RuntimeConfigurationIdentity, RuntimePlan, RuntimeReuseScope, RuntimeStrategy,
};

#[derive(Clone)]
pub(super) struct RuntimeResolver {
  pub profiles: Arc<RpcProfileStore>,
  pub transient_profiles: Arc<Mutex<BTreeMap<String, RpcResolvedProfile>>>,
  pub discovery_environment: Arc<Mutex<Option<discovery::DiscoveryEnvironment>>>,
}

impl RuntimeResolver {
  pub fn resolve_plan(
    &self,
    provider: Provider,
    cwd: &Path,
    selected: Option<&core_runtime::RuntimeSelection>,
  ) -> Result<RuntimePlan, String> {
    if provider == Provider::Pi {
      return Err("Pi uses its verified managed-session driver".into());
    }
    if !cwd.is_absolute() || !cwd.is_dir() {
      return Err("This session needs an existing absolute working directory".into());
    }
    let canonical_cwd = std::fs::canonicalize(cwd).map_err(|_| "The session working directory is unavailable")?;
    let profile = if let Some(selection) = selected {
      self.resolve_profile(&selection.profile_id, selection.expected_revision)?
    } else {
      let saved = self.profiles.snapshot()?;
      if let Some(profile) = select_profile(&saved, provider, &canonical_cwd)? {
        self.profiles.resolve_at_revision(&profile.profile_id, saved.revision)?
      } else {
        let environment = self
          .discovery_environment
          .lock()
          .unwrap_or_else(|error| error.into_inner())
          .clone()
          .unwrap_or_else(discovery::DiscoveryEnvironment::current);
        let profile = discovery::builtin_profile(provider, &canonical_cwd, &environment)?;
        self.pin_builtin_profile(profile.clone())?;
        profile
      }
    };
    if profile.profile.provider != provider {
      return Err("The chosen runtime belongs to another agent".into());
    }
    profile.validate_launch_ready()?;
    let discovered = self.is_discovered(&profile.profile.profile_id);
    if discovered || provider == Provider::OpenCode {
      let configured = profile
        .profile
        .working_directory
        .as_deref()
        .ok_or("This runtime needs a project folder")?;
      let configured = std::fs::canonicalize(configured).map_err(|_| "The runtime project folder is unavailable")?;
      if configured != canonical_cwd {
        return Err("The selected runtime belongs to another project folder".into());
      }
    }
    let plan = RuntimePlan {
      provider,
      configuration: RuntimeConfigurationIdentity {
        id: profile.profile.profile_id,
        revision: profile.revision,
      },
      ownership: match profile.profile.mode {
        RpcRuntimeMode::Managed => Ownership::Managed,
        RpcRuntimeMode::Attached => Ownership::Attached,
      },
      reuse_scope: if discovered || provider == Provider::OpenCode {
        RuntimeReuseScope::Project
      } else {
        RuntimeReuseScope::Runtime
      },
      strategy: RuntimeStrategy::SharedRuntime,
      working_directory: cwd.to_path_buf(),
    };
    plan.validate()?;
    Ok(plan)
  }

  pub fn resolve_profile(&self, id: &str, revision: u64) -> Result<RpcResolvedProfile, String> {
    if let Some(profile) = self
      .transient_profiles
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .get(id)
      .cloned()
    {
      if profile.revision != revision {
        return Err("Runtime configuration changed before connection".into());
      }
      discovery::validate_builtin(&profile)?;
      return Ok(profile);
    }
    self.profiles.resolve_at_revision(id, revision)
  }

  pub fn is_discovered(&self, id: &str) -> bool {
    self
      .transient_profiles
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .contains_key(id)
  }

  pub fn pin_builtin_profile(&self, profile: RpcResolvedProfile) -> Result<core_runtime::RuntimeSelection, String> {
    discovery::validate_builtin(&profile)?;
    let id = profile.profile.profile_id.clone();
    let mut transient = self
      .transient_profiles
      .lock()
      .unwrap_or_else(|error| error.into_inner());
    if !transient.contains_key(&id) && transient.len() >= 256 {
      return Err(
        "Project runtime configuration limit reached; reopen the app to clear inactive configurations".into(),
      );
    }
    transient.entry(id.clone()).or_insert(profile);
    Ok(core_runtime::RuntimeSelection {
      profile_id: id,
      expected_revision: 0,
    })
  }
}

fn select_profile<'a>(
  snapshot: &'a RpcRuntimeProfilesSnapshot,
  provider: Provider,
  canonical_cwd: &Path,
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
            .is_some_and(|directory| std::fs::canonicalize(directory).is_ok_and(|cwd| cwd == canonical_cwd)))
    })
    .collect::<Vec<_>>();
  if let Some(profile) = suitable.iter().find(|profile| profile.is_default) {
    return Ok(Some(*profile));
  }
  match suitable.as_slice() {
    [] => Ok(None),
    [profile] => Ok(Some(*profile)),
    _ => Err("Choose a default RPC runtime profile for this provider before sending".into()),
  }
}
