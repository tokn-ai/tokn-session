//! Durable folder membership derived from complete provider history catalogs.
use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::{RegisteredProjectRecord, Result, SessionIndex, SessionIndexError, SourceKey, SourceState};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectOrigin {
  Manual,
  Discovered,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectDiscoveryState {
  Present,
  Absent,
  Unknown,
}

/// Membership and origin are independent of current filesystem availability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectFolderRecord {
  pub project: RegisteredProjectRecord,
  pub origin: ProjectOrigin,
  pub discovery_state: ProjectDiscoveryState,
  /// Incomplete scans retain the last confirmed answer, including absence.
  pub has_agent_sessions: bool,
  /// Providers with current or last-confirmed session references.
  pub providers: Vec<String>,
}

/// A native session cwd and the host-verified launch folder it currently names.
/// Multiple cwd aliases may refer to one canonical project folder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectFolderObservation {
  pub cwd: String,
  pub project: RegisteredProjectRecord,
}

/// Capture before reading indexed folders and resolving their filesystem paths.
/// The index rechecks the entire inventory and revision in the write transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectDiscoveryCheckpoint {
  pub provider: String,
  pub sources: Vec<SourceState>,
  pub revision: i64,
}

impl SessionIndex {
  /// Changes only with saved folder metadata or provider membership, never
  /// ordinary session/source updates. Suitable for cheap cross-process polls.
  pub fn project_folder_revision(&self) -> Result<i64> {
    self
      .connection()?
      .query_row(
        "SELECT revision FROM project_folder_revision WHERE singleton = 1",
        [],
        |row| row.get(0),
      )
      .map_err(Into::into)
  }

  /// Only indexed cwd metadata; does not materialize session previews or bodies.
  /// Read after the checkpoint so reconciliation detects intervening mutations.
  pub fn project_discovery_cwds(&self, provider: &str) -> Result<BTreeSet<String>> {
    validate_provider(provider)?;
    let connection = self.connection()?;
    let mut statement = connection.prepare(
      "SELECT DISTINCT s.cwd FROM sessions AS s JOIN sources AS src ON src.id = s.source_id
       WHERE src.provider = ?1 AND s.present = 1 AND s.cwd IS NOT NULL ORDER BY s.cwd",
    )?;
    statement
      .query_map([provider], |row| row.get(0))?
      .collect::<rusqlite::Result<BTreeSet<_>>>()
      .map_err(Into::into)
  }

  pub fn project_discovery_checkpoint(&self, provider: &str) -> Result<ProjectDiscoveryCheckpoint> {
    validate_provider(provider)?;
    let mut connection = self.connection()?;
    let transaction = connection.transaction()?;
    let checkpoint = checkpoint(&transaction, provider)?;
    transaction.commit()?;
    Ok(checkpoint)
  }

  /// Replace one provider's folder evidence after its source changes commit.
  /// An incomplete pass may add observations, but cannot prove disappearance.
  /// Returns whether any folder's externally visible state changed.
  pub fn reconcile_project_folders(
    &self,
    provider: &str,
    expected: &ProjectDiscoveryCheckpoint,
    observations: &[ProjectFolderObservation],
    complete: bool,
  ) -> Result<bool> {
    self.update_project_folder_observations(provider, expected, observations, Some(complete))
  }

  /// Adds previously unknown cached evidence without changing any omitted
  /// membership. Startup uses this to bootstrap pre-discovery index databases.
  pub fn add_project_folder_observations(
    &self,
    provider: &str,
    expected: &ProjectDiscoveryCheckpoint,
    observations: &[ProjectFolderObservation],
  ) -> Result<bool> {
    self.update_project_folder_observations(provider, expected, observations, None)
  }

  fn update_project_folder_observations(
    &self,
    provider: &str,
    expected: &ProjectDiscoveryCheckpoint,
    observations: &[ProjectFolderObservation],
    complete: Option<bool>,
  ) -> Result<bool> {
    validate_provider(provider)?;
    if expected.provider != provider {
      return Err(SessionIndexError::InvalidProject(
        "discovery checkpoint belongs to another provider".into(),
      ));
    }
    let mut by_cwd = BTreeMap::new();
    for observation in observations {
      validate_cwd(&observation.cwd)?;
      super::registered_projects::validate_project(&observation.project)?;
      if let Some(previous) = by_cwd.insert(observation.cwd.as_str(), &observation.project) {
        if previous.folder_path != observation.project.folder_path {
          return Err(SessionIndexError::InvalidProject(
            "one cwd resolved to different launch folders".into(),
          ));
        }
      }
    }
    let mut connection = self.connection()?;
    super::project_aliases::refresh_registrations(&mut connection)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if checkpoint(&transaction, provider)? != *expected {
      return Err(SessionIndexError::ProjectDiscoveryConflict {
        provider: provider.into(),
      });
    }
    let before = folder_records(&transaction, None)?;
    let mut seen = BTreeSet::new();
    let mut mutated = false;
    for (cwd, candidate) in by_cwd {
      let project = super::registered_projects::ensure_project(&transaction, candidate, false)?;
      seen.insert(project.project_id.clone());
      mutated |= transaction.execute(
        "INSERT INTO project_folder_cwds(provider, cwd, project_id) VALUES(?1, ?2, ?3)
         ON CONFLICT(provider, cwd) DO UPDATE SET project_id = excluded.project_id
           WHERE project_folder_cwds.project_id != excluded.project_id",
        params![provider, cwd, project.project_id],
      )? > 0;
      mutated |= transaction.execute(
        "INSERT INTO project_folder_providers(project_id, provider, state, last_known_present)
         VALUES(?1, ?2, 'present', 1)
         ON CONFLICT(project_id, provider) DO UPDATE SET state = 'present', last_known_present = 1
           WHERE project_folder_providers.state != 'present' OR project_folder_providers.last_known_present != 1",
        params![project.project_id, provider],
      )? > 0;
    }
    let known = {
      let mut statement = transaction.prepare("SELECT project_id FROM project_folder_providers WHERE provider = ?1")?;
      statement
        .query_map([provider], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for project_id in known.into_iter().filter(|id| !seen.contains(id) && complete.is_some()) {
      if complete == Some(true) {
        mutated |= transaction.execute(
          "UPDATE project_folder_providers SET state = 'absent', last_known_present = 0
           WHERE project_id = ?1 AND provider = ?2 AND (state != 'absent' OR last_known_present != 0)",
          params![project_id, provider],
        )? > 0;
      } else {
        mutated |= transaction.execute(
          "UPDATE project_folder_providers SET state = 'unknown'
           WHERE project_id = ?1 AND provider = ?2 AND state != 'unknown'",
          params![project_id, provider],
        )? > 0;
      }
    }
    if mutated {
      let revision = expected
        .revision
        .checked_add(1)
        .ok_or_else(|| SessionIndexError::InvalidProject("discovery revision exhausted".into()))?;
      transaction.execute(
        "INSERT INTO project_discovery_revisions(provider, revision) VALUES(?1, ?2)
         ON CONFLICT(provider) DO UPDATE SET revision = excluded.revision",
        params![provider, revision],
      )?;
    }
    let changed = before != folder_records(&transaction, None)?;
    transaction.commit()?;
    Ok(changed)
  }

  /// Resolve an already observed spelling without following a now-missing
  /// symlink. A later confirmed observation can associate that cwd with a new
  /// folder, without changing the former folder's identity or saved receipts.
  pub fn project_folder_for_cwd(&self, provider: &str, cwd: &str) -> Result<Option<RegisteredProjectRecord>> {
    validate_provider(provider)?;
    validate_cwd(cwd)?;
    let connection = self.connection()?;
    connection
      .query_row(
        "SELECT p.project_id, p.group_key, p.folder_path, p.name, p.created_at_ms
       FROM project_folder_cwds AS c JOIN registered_projects AS p ON p.project_id = c.project_id
       WHERE c.provider = ?1 AND c.cwd = ?2",
        params![provider, cwd],
        super::registered_projects::project_from_row,
      )
      .optional()
      .map_err(Into::into)
  }

  pub fn list_project_folders(&self) -> Result<Vec<ProjectFolderRecord>> {
    let mut connection = self.connection()?;
    super::project_aliases::refresh_registrations(&mut connection)?;
    let transaction = connection.transaction()?;
    let records = folder_records(&transaction, None)?;
    transaction.commit()?;
    Ok(records)
  }

  pub fn get_project_folder(&self, project_id: &str) -> Result<Option<ProjectFolderRecord>> {
    let mut connection = self.connection()?;
    super::project_aliases::refresh_registrations(&mut connection)?;
    let transaction = connection.transaction()?;
    let record = folder_records(&transaction, Some(project_id))?.into_iter().next();
    transaction.commit()?;
    Ok(record)
  }
}

fn checkpoint(connection: &Connection, provider: &str) -> Result<ProjectDiscoveryCheckpoint> {
  let revision = connection
    .query_row(
      "SELECT revision FROM project_discovery_revisions WHERE provider = ?1",
      [provider],
      |row| row.get(0),
    )
    .optional()?
    .unwrap_or(0);
  let mut statement = connection.prepare(
    "SELECT source_key, cursor, scanned_at_ms, generation FROM sources WHERE provider = ?1 ORDER BY source_key ASC",
  )?;
  let sources = statement
    .query_map([provider], |row| {
      Ok(SourceState {
        key: SourceKey {
          provider: provider.into(),
          source_key: row.get(0)?,
        },
        cursor: row.get(1)?,
        scanned_at_ms: row.get(2)?,
        generation: row.get(3)?,
      })
    })?
    .collect::<rusqlite::Result<Vec<_>>>()?;
  Ok(ProjectDiscoveryCheckpoint {
    provider: provider.into(),
    sources,
    revision,
  })
}

fn folder_records(connection: &Connection, project_id: Option<&str>) -> Result<Vec<ProjectFolderRecord>> {
  let mut statement = connection.prepare(
    "SELECT p.project_id, p.group_key, p.folder_path, p.name, p.created_at_ms, COALESCE(o.manual, 1)
     FROM registered_projects AS p LEFT JOIN project_folder_origins AS o ON o.project_id = p.project_id
     WHERE ?1 IS NULL OR p.project_id = ?1 ORDER BY p.created_at_ms DESC, p.project_id ASC",
  )?;
  let projects = statement
    .query_map([project_id], |row| {
      Ok((
        super::registered_projects::project_from_row(row)?,
        row.get::<_, bool>(5)?,
      ))
    })?
    .collect::<rusqlite::Result<Vec<_>>>()?;
  let mut memberships = connection.prepare(
    "SELECT project_id, provider, state, last_known_present FROM project_folder_providers
     WHERE ?1 IS NULL OR project_id = ?1 ORDER BY project_id, provider",
  )?;
  let mut by_project = BTreeMap::<String, Vec<(String, String, bool)>>::new();
  for row in memberships.query_map([project_id], |row| {
    Ok((
      row.get::<_, String>(0)?,
      row.get::<_, String>(1)?,
      row.get::<_, String>(2)?,
      row.get::<_, bool>(3)?,
    ))
  })? {
    let (id, provider, state, present) = row?;
    by_project.entry(id).or_default().push((provider, state, present));
  }
  projects
    .into_iter()
    .map(|(project, manual)| {
      let sources = by_project.remove(&project.project_id).unwrap_or_default();
      let discovery_state = if sources.iter().any(|(_, state, _)| state == "present") {
        ProjectDiscoveryState::Present
      } else if sources.iter().any(|(_, state, _)| state == "unknown") {
        ProjectDiscoveryState::Unknown
      } else {
        ProjectDiscoveryState::Absent
      };
      let providers = sources
        .into_iter()
        .filter_map(|(provider, _, present)| present.then_some(provider))
        .collect::<Vec<_>>();
      Ok(ProjectFolderRecord {
        project,
        origin: if manual {
          ProjectOrigin::Manual
        } else {
          ProjectOrigin::Discovered
        },
        discovery_state,
        has_agent_sessions: !providers.is_empty(),
        providers,
      })
    })
    .collect()
}

fn validate_provider(provider: &str) -> Result<()> {
  if provider.is_empty()
    || provider.len() > 128
    || provider.trim() != provider
    || provider.chars().any(char::is_control)
  {
    return Err(SessionIndexError::InvalidProject("invalid discovery provider".into()));
  }
  Ok(())
}

fn validate_cwd(cwd: &str) -> Result<()> {
  if cwd.trim().is_empty() || cwd.len() > 4096 || cwd.chars().any(char::is_control) {
    return Err(SessionIndexError::InvalidProject(
      "invalid discovered folder path".into(),
    ));
  }
  Ok(())
}

#[cfg(test)]
mod tests;
