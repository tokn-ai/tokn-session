//! Provider discovery only changes durable folder evidence. Directory checks
//! remain host-owned and are repeated before launch and snapshot publication.
use std::{collections::BTreeSet, path::Path, sync::Mutex};

use tokio::sync::broadcast;
use tokn_session_index::{ProjectFolderObservation, RegisteredProjectRecord, SessionIndexError};

use crate::{ViewerService, model::ViewerProvider, relay::RelayMode, workspace::ProjectWorkspaceSnapshot};

use super::{
  EXTERNAL_REGISTRY_REASON, MAX_FOLDER_PATH_BYTES, MAX_PROJECT_NAME_CHARS, current_time_ms, project_snapshot,
};

const MAX_RECONCILIATION_ATTEMPTS: usize = 3;

pub(crate) struct ProjectRegistryState {
  snapshot: Mutex<Option<CachedProjectRegistry>>,
  bootstrapped: Mutex<BTreeSet<ViewerProvider>>,
  sender: broadcast::Sender<()>,
}

struct CachedProjectRegistry {
  revision: i64,
  snapshot: ProjectWorkspaceSnapshot,
}

impl Default for ProjectRegistryState {
  fn default() -> Self {
    Self {
      snapshot: Mutex::new(None),
      bootstrapped: Mutex::new(BTreeSet::new()),
      sender: broadcast::channel(16).0,
    }
  }
}

impl ViewerService {
  pub fn subscribe_project_registry_changes(&self) -> broadcast::Receiver<()> {
    self.project_registry.sender.subscribe()
  }

  pub(crate) fn refresh_project_registry_snapshot(&self) -> Result<ProjectWorkspaceSnapshot, String> {
    self
      .refresh_project_registry_snapshot_with_change()
      .map(|(snapshot, _)| snapshot)
  }

  /// Source writes are frequent; only a folder revision change warrants
  /// rebuilding the registry and checking its directories in a follower.
  pub(crate) fn refresh_project_registry_if_changed(&self) -> Result<(), String> {
    if self.session_scope.is_some() {
      return Ok(());
    }
    let revision = self
      .session_index
      .project_folder_revision()
      .map_err(|error| format!("Could not read the project registry revision: {error}"))?;
    let local = self.relay.status().settings.mode != RelayMode::External;
    let cached = self
      .project_registry
      .snapshot
      .lock()
      .map_err(|_| "Project registry state is unavailable")?;
    if cached
      .as_ref()
      .is_some_and(|cached| cached.revision == revision && cached.snapshot.available == local)
    {
      return Ok(());
    }
    drop(cached);
    self.refresh_project_registry_snapshot().map(|_| ())
  }

  fn refresh_project_registry_snapshot_with_change(&self) -> Result<(ProjectWorkspaceSnapshot, bool), String> {
    self.check_project_share_scope()?;
    // Serialize the read and filesystem checks too, so an older snapshot can
    // never overwrite a newer one after another refresh publishes it.
    let mut cached = self
      .project_registry
      .snapshot
      .lock()
      .map_err(|_| "Project registry state is unavailable")?;
    // Capture before reading the rows. A concurrent commit after this read
    // remains newer than the cache and will be picked up by the next poll.
    let revision = self
      .session_index
      .project_folder_revision()
      .map_err(|error| format!("Could not read the project registry revision: {error}"))?;
    let snapshot = if self.relay.status().settings.mode == RelayMode::External {
      ProjectWorkspaceSnapshot {
        available: false,
        unavailable_reason: Some(EXTERNAL_REGISTRY_REASON.into()),
        projects: Vec::new(),
      }
    } else {
      let projects = self
        .session_index
        .list_project_folders()
        .map_err(|error| format!("Could not list project folders: {error}"))?
        .into_iter()
        .map(project_snapshot)
        .collect();
      ProjectWorkspaceSnapshot {
        available: true,
        unavailable_reason: None,
        projects,
      }
    };
    let changed = cached.as_ref().map(|cached| &cached.snapshot) != Some(&snapshot);
    *cached = Some(CachedProjectRegistry {
      revision,
      snapshot: snapshot.clone(),
    });
    if changed {
      let _ = self.project_registry.sender.send(());
    }
    Ok((snapshot, changed))
  }

  /// Called after fresh catalog metadata has committed. Preserved cached rows
  /// are deliberately not positive observations during incomplete discovery.
  pub(crate) fn reconcile_discovered_project_folders(
    &self,
    provider: ViewerProvider,
    cwds: &BTreeSet<String>,
    complete: bool,
  ) -> Result<bool, String> {
    // Uncovered providers can still use the local catalog as an External-mode
    // fallback. That scan must neither mutate this registry nor become a
    // provider error merely because project management is unavailable.
    if self.session_scope.is_some() || self.relay.status().settings.mode == RelayMode::External {
      return Ok(false);
    }
    let changed = self
      .reconcile_project_folder_evidence(provider, Some(cwds), complete)?
      .unwrap_or(false);
    self
      .project_registry
      .bootstrapped
      .lock()
      .map_err(|_| "Project registry state is unavailable")?
      .insert(provider);
    let (_, snapshot_changed) = self.refresh_project_registry_snapshot_with_change()?;
    Ok(changed || snapshot_changed)
  }

  pub(super) fn bootstrap_project_registry(&self) -> Result<(), String> {
    self.check_local_project_registry()?;
    let mut bootstrapped = self
      .project_registry
      .bootstrapped
      .lock()
      .map_err(|_| "Project registry state is unavailable")?;
    for provider in ViewerProvider::ALL {
      if bootstrapped.contains(&provider) {
        continue;
      }
      // A failure leaves the provider unmarked, so a later list retries rather
      // than silently treating an unreadable index as an empty catalog.
      if self.reconcile_project_folder_evidence(provider, None, false)?.is_some() {
        bootstrapped.insert(provider);
      }
    }
    Ok(())
  }

  fn reconcile_project_folder_evidence(
    &self,
    provider: ViewerProvider,
    freshly_observed: Option<&BTreeSet<String>>,
    complete: bool,
  ) -> Result<Option<bool>, String> {
    let provider = provider.as_str();
    for _ in 0..MAX_RECONCILIATION_ATTEMPTS {
      let checkpoint = self
        .session_index
        .project_discovery_checkpoint(provider)
        .map_err(|error| format!("Could not checkpoint project discovery: {error}"))?;
      // Older indexes formatted cwd as display text, collapsing spaces or
      // truncating paths. Check the native-reread marker inside the same
      // checkpoint that fences the eventual write, including every retry.
      if freshly_observed.is_none()
        && !checkpoint
          .sources
          .iter()
          .any(|source| source.key.source_key == "catalog.v1" && source.cursor == "headers.v3")
      {
        return Ok(None);
      }
      let indexed = self
        .session_index
        .project_discovery_cwds(provider)
        .map_err(|error| format!("Could not read indexed project folders: {error}"))?;
      let indexed_cwds_valid = indexed.iter().all(|cwd| valid_discovered_cwd(cwd));
      let current = indexed
        .into_iter()
        .filter(|cwd| valid_discovered_cwd(cwd))
        .collect::<BTreeSet<_>>();
      // A sibling process may have committed new metadata before this method
      // captured its checkpoint. It cannot be disproved by the caller's older
      // scan, even if the caller finished that scan successfully.
      let complete = complete
        && indexed_cwds_valid
        && freshly_observed
          .is_some_and(|fresh| fresh.iter().all(|cwd| valid_discovered_cwd(cwd)) && current.is_subset(fresh));
      let candidates = current
        .iter()
        .filter(|cwd| freshly_observed.is_none_or(|fresh| fresh.contains(*cwd)));
      let mut observations = Vec::new();
      let mut unresolved = Vec::new();
      for cwd in candidates {
        let known = self
          .session_index
          .project_folder_for_cwd(provider, cwd)
          .map_err(|error| format!("Could not read a discovered folder alias: {error}"))?;
        // Cached bootstrap can add previously unmapped folders but must never
        // change existing presence/absence or resurrect a retired membership.
        if freshly_observed.is_none() && known.is_some() {
          continue;
        }
        let canonical = super::canonical_folder(cwd).ok();
        let previously_resolved = known.is_some();
        if let Some(project) =
          known.filter(|known| canonical.as_ref().is_none_or(|folder| folder == &known.folder_path))
        {
          observations.push(ProjectFolderObservation {
            cwd: cwd.clone(),
            project,
          });
          continue;
        }
        // Parent traversal cannot be normalized safely without resolving every
        // symlink. Wait for that path to become readable rather than save an
        // identity that changes as soon as it can be canonicalized.
        if canonical.is_none()
          && Path::new(cwd)
            .components()
            .any(|component| component == std::path::Component::ParentDir)
        {
          continue;
        }
        let folder_path = canonical.unwrap_or_else(|| {
          Path::new(cwd)
            .components()
            .collect::<std::path::PathBuf>()
            .to_string_lossy()
            .into_owned()
        });
        unresolved.push((cwd.clone(), folder_path, previously_resolved));
      }
      observations.extend(self.new_discovered_projects(unresolved)?);
      if freshly_observed.is_none() && observations.is_empty() {
        return Ok(Some(false));
      }
      let result = if freshly_observed.is_none() {
        self
          .session_index
          .add_project_folder_observations(provider, &checkpoint, &observations)
      } else {
        self
          .session_index
          .reconcile_project_folders(provider, &checkpoint, &observations, complete)
      };
      match result {
        Ok(changed) => return Ok(Some(changed)),
        Err(SessionIndexError::ProjectDiscoveryConflict { .. }) => continue,
        Err(error) => return Err(format!("Could not reconcile discovered project folders: {error}")),
      }
    }
    Err("Project discovery changed while folders were being resolved; retrying on the next catalog update".into())
  }

  fn new_discovered_projects(
    &self,
    unresolved: Vec<(String, String, bool)>,
  ) -> Result<Vec<ProjectFolderObservation>, String> {
    if unresolved.is_empty() {
      return Ok(Vec::new());
    }
    let created_at_ms = current_time_ms();
    let known_groups = self
      .session_index
      .project_order(&[], created_at_ms)
      .map_err(|error| format!("Could not read existing project groups: {error}"))?;
    let candidates = unresolved
      .into_iter()
      .map(|(cwd, folder_path, previously_resolved)| {
        let group_folder =
          if !previously_resolved && !known_groups.contains_key(&folder_path) && known_groups.contains_key(&cwd) {
            cwd.clone()
          } else {
            folder_path.clone()
          };
        (cwd, folder_path, group_folder)
      })
      .collect::<Vec<_>>();
    let group_folders = candidates
      .iter()
      .map(|(_, _, group)| group.clone())
      .collect::<BTreeSet<_>>()
      .into_iter()
      .collect::<Vec<_>>();
    let groups = self
      .session_index
      .project_groups(&group_folders, created_at_ms)
      .map_err(|error| format!("Could not resolve discovered folder groups: {error}"))?;
    candidates
      .into_iter()
      .map(|(cwd, folder_path, group_folder)| {
        let group_key = groups
          .get(&group_folder)
          .ok_or("Could not resolve the discovered folder group")?
          .project_key
          .clone();
        let name = Path::new(&folder_path)
          .file_name()
          .and_then(|name| name.to_str())
          .filter(|name| !name.trim().is_empty())
          .unwrap_or("Project")
          .chars()
          .take(MAX_PROJECT_NAME_CHARS)
          .collect();
        Ok(ProjectFolderObservation {
          cwd,
          project: RegisteredProjectRecord {
            project_id: uuid::Uuid::new_v4().to_string(),
            group_key,
            folder_path,
            name,
            created_at_ms,
          },
        })
      })
      .collect()
  }
}

/// Validate a native path without trimming, collapsing whitespace, removing
/// controls, or truncating it into a different filesystem identity.
pub(crate) fn valid_discovered_cwd(cwd: &str) -> bool {
  !cwd.trim().is_empty()
    && cwd.len() <= MAX_FOLDER_PATH_BYTES
    && !cwd.chars().any(char::is_control)
    && Path::new(cwd).is_absolute()
}

#[cfg(test)]
mod tests;
