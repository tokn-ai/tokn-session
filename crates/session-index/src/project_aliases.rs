//! Provenance separates folder aliases from the former ancestor-Git heuristic.
use std::{
  collections::{HashMap, HashSet},
  path::Path,
};

use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use crate::{Result, projects, remember_project};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Provenance {
  DirectGit,
  Worktree,
  RegisteredFolder,
}

impl Provenance {
  fn as_str(self) -> &'static str {
    match self {
      Self::DirectGit => "direct_git",
      Self::Worktree => "worktree",
      Self::RegisteredFolder => "registered_folder",
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Alias {
  pub project_key: String,
  pub provenance: Provenance,
}

impl Alias {
  pub fn registered(project_key: String) -> Self {
    Self {
      project_key,
      provenance: Provenance::RegisteredFolder,
    }
  }
}

pub(super) fn load(connection: &Connection) -> Result<HashMap<String, Alias>> {
  let mut statement =
    connection.prepare("SELECT cwd, project_key, provenance FROM project_aliases WHERE provenance != 'legacy'")?;
  let rows = statement.query_map([], |row| {
    let provenance: String = row.get(2)?;
    let provenance = match provenance.as_str() {
      "direct_git" => Provenance::DirectGit,
      "worktree" => Provenance::Worktree,
      "registered_folder" => Provenance::RegisteredFolder,
      _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok((
      row.get(0)?,
      Alias {
        project_key: row.get(1)?,
        provenance,
      },
    ))
  })?;
  rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
}

pub(super) fn insert(transaction: &Transaction<'_>, cwd: &str, alias: &Alias) -> Result<()> {
  transaction.execute(
    "INSERT INTO project_aliases(cwd, project_key, provenance) VALUES(?1, ?2, ?3)
     ON CONFLICT(cwd) DO UPDATE SET project_key = excluded.project_key, provenance = excluded.provenance
       WHERE project_aliases.provenance = 'legacy'",
    params![cwd, alias.project_key, alias.provenance.as_str()],
  )?;
  Ok(())
}

/// A process opened before this upgrade can still write unmarked aliases.
/// The indexed existence check keeps the ordinary metadata read path cheap.
pub(super) fn repair_legacy(connection: &mut Connection) -> Result<()> {
  let pending: bool = connection.query_row(
    "SELECT EXISTS(SELECT 1 FROM project_aliases WHERE provenance = 'legacy')",
    [],
    |row| row.get(0),
  )?;
  if pending {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    upgrade_legacy(&transaction)?;
    transaction.commit()?;
  }
  Ok(())
}

/// A current marker can replace a cached checkout identity. An absent marker
/// cannot disprove a previously verified worktree after its checkout is removed.
pub(super) fn load_current(connection: &mut Connection, cwds: &[String]) -> Result<HashMap<String, Alias>> {
  repair_legacy(connection)?;
  let aliases = load(connection)?;
  let mut replacements = Vec::new();
  for cwd in cwds.iter().collect::<HashSet<_>>() {
    let Some(previous) = aliases.get(cwd) else { continue };
    if let Some(current) = projects::direct_repository(cwd) {
      if *previous == current {
        continue;
      }
      if previous.provenance == Provenance::RegisteredFolder
        && current.provenance == Provenance::DirectGit
        && Path::new(&previous.project_key).canonicalize().ok().as_deref() == Some(Path::new(&current.project_key))
      {
        continue;
      }
      replacements.push((cwd, previous, Some(current)));
    } else if previous.provenance == Provenance::Worktree && projects::marker_absent_in_existing_folder(Path::new(cwd))
    {
      replacements.push((cwd, previous, None));
    }
  }
  if replacements.is_empty() {
    return Ok(aliases);
  }
  let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
  let mut anchors: HashMap<&str, i64> = HashMap::new();
  for (cwd, _, current) in &replacements {
    let time: i64 = transaction.query_row(
      "SELECT sort_at_ms FROM projects WHERE project_key = ?1
       UNION ALL SELECT created_at_ms FROM registered_projects WHERE folder_path = ?1 LIMIT 1",
      [cwd],
      |row| row.get(0),
    )?;
    let group = current.as_ref().map(|alias| alias.project_key.as_str()).unwrap_or(cwd);
    anchors
      .entry(group)
      .and_modify(|anchor| *anchor = (*anchor).min(time))
      .or_insert(time);
  }
  for (root, time) in anchors {
    remember_project(&transaction, root, time)?;
  }
  for (cwd, previous, current) in replacements {
    let group = current.as_ref().map(|alias| alias.project_key.as_str()).unwrap_or(cwd);
    let changed = if let Some(current) = &current {
      transaction.execute(
        "UPDATE project_aliases SET project_key = ?1, provenance = ?2
       WHERE cwd = ?3 AND project_key = ?4 AND provenance = ?5",
        params![
          current.project_key,
          current.provenance.as_str(),
          cwd,
          previous.project_key,
          previous.provenance.as_str()
        ],
      )?
    } else {
      transaction.execute(
        "DELETE FROM project_aliases WHERE cwd = ?1 AND project_key = ?2 AND provenance = ?3",
        params![cwd, previous.project_key, previous.provenance.as_str()],
      )?
    };
    if changed != 0 {
      let registered = transaction.execute(
        "UPDATE registered_projects SET group_key = ?1 WHERE folder_path = ?2 AND group_key = ?3",
        params![group, cwd, previous.project_key],
      )?;
      if current.is_none() && registered != 0 {
        insert(&transaction, cwd, &Alias::registered(cwd.clone()))?;
      }
    }
  }
  transaction.commit()?;
  load(connection)
}

pub(super) fn refresh_registrations(connection: &mut Connection) -> Result<()> {
  let folders = {
    let mut statement = connection.prepare("SELECT folder_path FROM registered_projects")?;
    statement
      .query_map([], |row| row.get(0))?
      .collect::<rusqlite::Result<Vec<String>>>()?
  };
  load_current(connection, &folders)?;
  Ok(())
}

/// Runs once inside migration 12's transaction, before any public index reads.
/// Unknown legacy aliases are not evidence that a missing folder was a worktree.
/// Only direct markers, exact worktree registrations, and verified canonical
/// spellings may survive; no provider history or user message is opened here.
pub(super) fn upgrade_legacy(transaction: &Transaction<'_>) -> Result<()> {
  let trusted = load(transaction)?;
  let order = {
    let mut statement = transaction.prepare("SELECT project_key, sort_at_ms FROM projects")?;
    statement
      .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?
      .collect::<rusqlite::Result<HashMap<_, _>>>()?
  };
  let legacy = {
    let mut statement =
      transaction.prepare("SELECT cwd, project_key FROM project_aliases WHERE provenance = 'legacy'")?;
    statement
      .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
      .collect::<rusqlite::Result<HashMap<_, _>>>()?
  };
  let registrations = {
    let mut statement =
      transaction.prepare("SELECT project_id, group_key, folder_path, created_at_ms FROM registered_projects")?;
    statement
      .query_map([], |row| {
        Ok((
          row.get::<_, String>(0)?,
          row.get::<_, String>(1)?,
          row.get::<_, String>(2)?,
          row.get::<_, i64>(3)?,
        ))
      })?
      .collect::<rusqlite::Result<Vec<_>>>()?
  };
  let mut worktrees = HashMap::new();
  let roots = legacy
    .values()
    .chain(registrations.iter().map(|row| &row.1))
    .collect::<std::collections::HashSet<_>>();
  for root in roots {
    worktrees.extend(projects::registered_worktrees(Path::new(root)));
  }
  let mut verified = HashMap::new();
  for cwd in legacy.keys() {
    if let Some(alias) = projects::direct_repository(cwd).or_else(|| worktrees.get(cwd).cloned()) {
      verified.insert(cwd.clone(), alias);
    }
  }
  let mut updates = Vec::new();
  let mut anchors: HashMap<String, i64> = HashMap::new();
  for (project_id, old_group, folder, created_at_ms) in &registrations {
    // A new ordinary Git marker does not change an explicitly registered
    // spelling of the same physical folder. A different common root does.
    let spelling = Path::new(old_group)
      .canonicalize()
      .ok()
      .filter(|canonical| canonical == Path::new(folder))
      .map(|_| Alias::registered(old_group.clone()));
    let alias = trusted
      .get(folder)
      .cloned()
      .or_else(|| {
        projects::direct_repository(folder).map(|direct| {
          if direct.provenance == Provenance::DirectGit {
            spelling.clone().unwrap_or(direct)
          } else {
            direct
          }
        })
      })
      .or_else(|| worktrees.get(folder).cloned())
      // Never retain a parent merely because it was an old inferred group.
      .or(spelling)
      .unwrap_or_else(|| Alias::registered(folder.clone()));
    let time = order.get(folder).copied().unwrap_or(*created_at_ms);
    anchors
      .entry(alias.project_key.clone())
      .and_modify(|anchor| *anchor = (*anchor).min(time))
      .or_insert(time);
    updates.push((project_id, alias.project_key.clone()));
    verified.insert(folder.clone(), alias);
  }
  // Preserve each cwd's immutable anchor when the old shared group splits.
  // Existing target anchors win, exactly as in ordinary project discovery.
  for (cwd, alias) in &verified {
    if let Some(time) = order.get(cwd) {
      anchors
        .entry(alias.project_key.clone())
        .and_modify(|anchor| *anchor = (*anchor).min(*time))
        .or_insert(*time);
    }
  }
  for (root, time) in anchors {
    remember_project(transaction, &root, time)?;
  }
  for (project_id, group) in updates {
    transaction.execute(
      "UPDATE registered_projects SET group_key = ?1 WHERE project_id = ?2 AND group_key != ?1",
      params![group, project_id],
    )?;
  }
  transaction.execute("DELETE FROM project_aliases WHERE provenance = 'legacy'", [])?;
  for (cwd, alias) in verified {
    insert(transaction, &cwd, &alias)?;
  }
  Ok(())
}

#[cfg(test)]
mod tests;
