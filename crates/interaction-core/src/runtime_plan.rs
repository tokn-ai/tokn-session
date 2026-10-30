//! Pure host-resolved runtime intent. Resolving or validating a plan never
//! starts a process, takes ownership, creates a session, or resumes history.
use crate::{
  Ownership, Provider,
  runtime::{NativeSessionDescriptor, RuntimeSelection},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const MAX_IDENTITY_BYTES: usize = 1024;

/// An opaque host configuration identity and its revision fence. The host
/// must change one of these when launch-affecting configuration changes;
/// executable paths, endpoints, environment values and credentials stay there.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfigurationIdentity {
  pub id: String,
  pub revision: u64,
}

impl RuntimeConfigurationIdentity {
  pub fn validate(&self) -> Result<(), String> {
    if !valid_identity(&self.id) {
      return Err("Invalid runtime configuration identity".into());
    }
    // Revisions are host-owned fences. Zero is a valid initial revision.
    Ok(())
  }
}

/// Maximum scope within which the selected factory can reuse a runtime.
/// Reuse still requires matching configuration and explicit native ownership.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeReuseScope {
  Session,
  Project,
  Runtime,
}

/// Generic lifecycle strategies, selected by factory capabilities rather than
/// inferred from a provider name. This first contract does not migrate either
/// strategy's existing admission receipts or worker ownership.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeStrategy {
  /// One host-owned worker for one native session.
  SessionWorker,
  /// A configured runtime coordinator that can bind native sessions.
  SharedRuntime,
}

/// Resolution result containing presentation-safe identities and policy only.
/// Deserializing this value is not host verification: every consumer must
/// validate and re-resolve its configuration fence before activation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimePlan {
  pub provider: Provider,
  pub configuration: RuntimeConfigurationIdentity,
  pub ownership: Ownership,
  pub reuse_scope: RuntimeReuseScope,
  pub strategy: RuntimeStrategy,
  pub working_directory: PathBuf,
}

impl RuntimePlan {
  pub fn validate(&self) -> Result<(), String> {
    self.configuration.validate()?;
    validate_path(&self.working_directory, "Runtime working directory")?;
    if self.strategy == RuntimeStrategy::SessionWorker
      && (self.ownership != Ownership::Managed || self.reuse_scope != RuntimeReuseScope::Session)
    {
      return Err("A session worker requires managed ownership and session reuse scope".into());
    }
    Ok(())
  }

  /// Bridge to the existing shared coordinator without guessing a profile.
  /// Session-worker identities belong to their factory, not that coordinator.
  pub fn shared_runtime_selection(&self) -> Result<RuntimeSelection, String> {
    self.validate()?;
    if self.strategy != RuntimeStrategy::SharedRuntime {
      return Err("A session-worker plan cannot select a shared runtime profile".into());
    }
    uuid::Uuid::parse_str(&self.configuration.id)
      .map_err(|_| "Shared runtime configuration requires a valid profile ID")?;
    Ok(RuntimeSelection {
      profile_id: self.configuration.id.clone(),
      expected_revision: self.configuration.revision,
    })
  }

  /// Scope identity for configuration-sensitive metadata and draft selections.
  /// This is not a runtime instance, native session ID, ownership lease or an
  /// authorization token. Session-scoped reuse must additionally match the
  /// verified native session identity.
  pub fn fingerprint(&self) -> Result<String, String> {
    self.validate()?;
    let bytes = serde_json::to_vec(&("runtime-plan-v1", self)).map_err(|error| error.to_string())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
  }

  pub fn plan_id(&self) -> Result<String, String> {
    self.fingerprint()
  }

  /// Check host-verified history against this requested plan without accessing
  /// the source or activating a runtime. Concrete factories can require extra
  /// resume data, such as a history path, after this common identity check.
  pub fn validate_resume(&self, session: &VerifiedSessionDescriptor) -> Result<(), String> {
    self.validate()?;
    if self.provider != session.provider {
      return Err("Verified session belongs to another provider".into());
    }
    if self.working_directory != session.native.working_directory {
      return Err("Verified session belongs to another working directory".into());
    }
    Ok(())
  }
}

/// Evidence supplied by a host after native history identity verification.
///
/// Core can check structure, not provider-native history bytes. The explicit
/// constructor is therefore a host trust boundary, not an IO verifier. Private
/// fields and the absence of Deserialize prevent wire data from silently
/// becoming verified evidence. No native activation occurs here.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct VerifiedSessionDescriptor {
  provider: Provider,
  native: NativeSessionDescriptor,
}

impl VerifiedSessionDescriptor {
  /// Call only after the host has verified the provider, native identity and
  /// working directory against its trusted history source. File-based sources
  /// should also supply their verified history path; ID-based sources need not.
  pub fn from_verified_history(provider: Provider, native: NativeSessionDescriptor) -> Result<Self, String> {
    if !valid_identity(&native.native_id) {
      return Err("Invalid verified native session identity".into());
    }
    validate_path(&native.working_directory, "Verified session working directory")?;
    if let Some(path) = &native.history_path {
      validate_path(path, "Verified session history path")?;
    }
    Ok(Self { provider, native })
  }

  pub fn provider(&self) -> Provider {
    self.provider
  }

  pub fn native(&self) -> &NativeSessionDescriptor {
    &self.native
  }

  pub fn history_path(&self) -> Option<&Path> {
    self.native.history_path.as_deref()
  }
}

fn valid_identity(value: &str) -> bool {
  !value.is_empty()
    && value.len() <= MAX_IDENTITY_BYTES
    && value.trim() == value
    && !value.chars().any(char::is_control)
}

/// Structural validation only; the host must compare this opaque fence with
/// the current resolved plan before admitting new creation input.
pub fn validate_plan_id(value: &str) -> Result<(), String> {
  if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
    return Err("Invalid runtime plan identity".into());
  }
  Ok(())
}

fn validate_path(path: &Path, label: &str) -> Result<(), String> {
  if !path.is_absolute() || path.to_str().is_none_or(|path| path.contains('\0')) {
    return Err(format!("{label} must be an absolute JSON-serializable path"));
  }
  // Do not canonicalize here: verification belongs to the host, and resolving
  // a plan must not depend on disk access or mutate how a path is represented.
  Ok(())
}

#[cfg(test)]
mod tests;
