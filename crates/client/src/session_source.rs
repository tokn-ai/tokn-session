use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tokn_session_codex::CodexSessionSource;
use tokn_session_core::{
  FileSessionCatalog, LoadedSession, LoadedSessionTree, SessionHeader, SessionHeaderCatalog, SessionRef,
  SessionRelationCatalog,
};
use tokn_session_dsh::DshSessionSource;
use tokn_session_opencode::OpenCodeSessionSource;
use tokn_session_pi::PiSessionSource;
use tokn_session_workbuddy::WorkBuddySessionSource;
use tokn_session_zcode::ZCodeSessionSource;

use crate::Source;

pub(crate) enum SessionSourceClient {
  Dsh(DshSessionSource),
  Codex(CodexSessionSource),
  OpenCode(OpenCodeSessionSource),
  Pi(PiSessionSource),
  WorkBuddy(WorkBuddySessionSource),
  ZCode(ZCodeSessionSource),
}

impl SessionSourceClient {
  pub(crate) fn list_session_headers(&self) -> Result<Vec<SessionHeader>, String> {
    let mut headers = match self {
      Self::Dsh(source) => file_headers(source.list_session_relations()?),
      Self::Codex(source) => file_headers(source.list_session_relations()?),
      Self::OpenCode(source) => source.list_session_headers()?,
      Self::Pi(source) => file_headers(source.list_session_relations()?),
      Self::WorkBuddy(source) => source.list_session_headers()?,
      Self::ZCode(source) => source.list_session_headers()?,
    };
    sort_headers(&mut headers);
    Ok(headers)
  }

  pub(crate) fn list_session_header_catalog(&self) -> Result<SessionHeaderCatalog, String> {
    let mut catalog = match self {
      Self::Dsh(source) => file_header_catalog(source.list_session_relation_catalog()?),
      Self::Codex(source) => file_header_catalog(source.list_session_relation_catalog()?),
      Self::OpenCode(source) => source.list_session_header_catalog()?,
      Self::Pi(source) => file_header_catalog(source.list_session_relation_catalog()?),
      Self::WorkBuddy(source) => source.list_session_header_catalog()?,
      Self::ZCode(source) => source.list_session_header_catalog()?,
    };
    sort_headers(&mut catalog.headers);
    Ok(catalog)
  }

  pub(crate) fn session_catalog_roots(&self) -> Result<Vec<PathBuf>, String> {
    match self {
      Self::Codex(source) => source.session_roots(),
      Self::Pi(source) => source.session_roots(),
      Self::Dsh(source) => Ok(vec![source.session_root()?]),
      Self::OpenCode(source) => Ok(vec![source.database_path()?]),
      Self::WorkBuddy(source) => Ok(vec![source.config_dir()?]),
      Self::ZCode(source) => Ok(vec![source.database_path()?]),
    }
  }

  pub(crate) fn file_session_roots(&self) -> Result<Vec<PathBuf>, String> {
    match self {
      Self::Codex(source) => source.session_roots(),
      Self::Pi(source) => source.session_roots(),
      Self::Dsh(_) => Err(path_targeting_unsupported("dsh")),
      Self::OpenCode(_) => Err(path_targeting_unsupported("opencode")),
      Self::WorkBuddy(_) => Err(path_targeting_unsupported("workbuddy")),
      Self::ZCode(_) => Err(path_targeting_unsupported("zcode")),
    }
  }

  pub(crate) fn file_session_paths(&self) -> Result<Vec<PathBuf>, String> {
    match self {
      Self::Codex(source) => source.session_paths(),
      Self::Pi(source) => source.session_paths(),
      Self::Dsh(_) => Err(path_targeting_unsupported("dsh")),
      Self::OpenCode(_) => Err(path_targeting_unsupported("opencode")),
      Self::WorkBuddy(_) => Err(path_targeting_unsupported("workbuddy")),
      Self::ZCode(_) => Err(path_targeting_unsupported("zcode")),
    }
  }

  pub(crate) fn file_session_catalog(&self) -> Result<FileSessionCatalog, String> {
    match self {
      Self::Codex(source) => source.session_path_catalog(),
      Self::Pi(source) => source.session_path_catalog(),
      Self::Dsh(_) => Err(path_targeting_unsupported("dsh")),
      Self::OpenCode(_) => Err(path_targeting_unsupported("opencode")),
      Self::WorkBuddy(_) => Err(path_targeting_unsupported("workbuddy")),
      Self::ZCode(_) => Err(path_targeting_unsupported("zcode")),
    }
  }

  pub(crate) fn apply_catalog_metadata(&self, headers: &mut [SessionHeader]) {
    if let Self::Codex(source) = self {
      source.apply_catalog_metadata(headers);
    }
  }

  pub(crate) fn session_header_at_path(&self, path: &Path) -> Result<SessionHeader, String> {
    let reference = match self {
      // A watcher can receive one notification per appended rollout record.
      // Keep its Codex read confined to the JSONL header; the complete catalog
      // pass owns private Desktop state and legacy-index presentation metadata.
      Self::Codex(source) => source.session_relation_at_path_for_incremental_catalog(path),
      Self::Pi(source) => source.session_relation_at_path(path),
      Self::Dsh(_) => Err(path_targeting_unsupported("dsh")),
      Self::OpenCode(_) => Err(path_targeting_unsupported("opencode")),
      Self::WorkBuddy(_) => Err(path_targeting_unsupported("workbuddy")),
      Self::ZCode(_) => Err(path_targeting_unsupported("zcode")),
    }?;
    Ok(file_header(reference))
  }

  pub(crate) fn list_sessions(&self) -> Result<Vec<SessionRef>, String> {
    match self {
      Self::Dsh(source) => source.list_sessions(),
      Self::Codex(source) => source.list_sessions(),
      Self::OpenCode(source) => source.list_sessions(),
      Self::Pi(source) => source.list_sessions(),
      Self::WorkBuddy(source) => source.list_sessions(),
      Self::ZCode(source) => source.list_sessions(),
    }
  }

  pub(crate) fn hydrate_session_header(&self, header: SessionHeader) -> Result<SessionHeader, String> {
    match self {
      Self::Dsh(source) => source.hydrate_session_header(header),
      Self::Codex(source) => source.hydrate_session_header(header),
      Self::OpenCode(source) => source.hydrate_session_header(header),
      Self::Pi(source) => source.hydrate_session_header(header),
      Self::WorkBuddy(source) => source.hydrate_session_header(header),
      Self::ZCode(source) => source.hydrate_session_header(header),
    }
  }

  pub(crate) fn list_session_relations(&self) -> Result<Vec<SessionRef>, String> {
    match self {
      Self::Dsh(source) => source.list_session_relations(),
      Self::Codex(source) => source.list_session_relations(),
      Self::OpenCode(source) => source.list_session_relations(),
      Self::Pi(source) => source.list_session_relations(),
      Self::WorkBuddy(source) => source.list_session_relations(),
      Self::ZCode(source) => source.list_session_relations(),
    }
  }

  pub(crate) fn load_session(&self, session: &str) -> Result<LoadedSession, String> {
    match self {
      Self::Dsh(source) => source.load_session(session),
      Self::Codex(source) => source.load_session(session),
      Self::OpenCode(source) => source.load_session(session),
      Self::Pi(source) => source.load_session(session),
      Self::WorkBuddy(source) => source.load_session(session),
      Self::ZCode(source) => source.load_session(session),
    }
  }

  pub(crate) fn load_session_path(&self, path: &Path) -> Result<LoadedSession, String> {
    match self {
      Self::Dsh(source) => source.load_session_path(path),
      Self::Codex(source) => source.load_session_path(path),
      Self::OpenCode(_) => {
        Err("opencode sessions are stored in sqlite; pass a session id and use --session-dir for the database".into())
      }
      Self::Pi(source) => source.load_session_path(path),
      Self::WorkBuddy(source) => {
        let path = path
          .to_str()
          .ok_or_else(|| "workbuddy session path is not valid UTF-8".to_string())?;
        source.load_session(path)
      }
      Self::ZCode(_) => {
        Err("zcode sessions are stored in sqlite; pass a session id and use --session-dir for the database".into())
      }
    }
  }

  pub(crate) fn load_session_tree(&self, session: &str) -> Result<LoadedSessionTree, String> {
    let root = self.load_session(session)?;
    if matches!(self, Self::WorkBuddy(_)) {
      // The observed WorkBuddy schema has message ancestry, but no
      // session-level parent relationship. Keep tree output useful for both
      // cataloged and orphan JSONL histories by treating each as a leaf.
      return Ok(LoadedSessionTree {
        session: root,
        children: Vec::new(),
      });
    }
    self.load_session_tree_from(root, self.list_session_relations()?)
  }

  pub(crate) fn load_session_tree_from(
    &self,
    root: LoadedSession,
    mut references: Vec<SessionRef>,
  ) -> Result<LoadedSessionTree, String> {
    references.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then_with(|| b.path.cmp(&a.path)));
    let mut canonical_ids = HashSet::new();
    references.retain(|reference| canonical_ids.insert(reference.id.clone()));
    references.sort_by(|a, b| a.timestamp.cmp(&b.timestamp).then_with(|| a.path.cmp(&b.path)));

    let mut children_by_parent = HashMap::<String, Vec<SessionRef>>::new();
    for reference in references {
      if let Some(parent_session_id) = reference.parent_session_id.clone() {
        children_by_parent.entry(parent_session_id).or_default().push(reference);
      }
    }

    let mut visited = HashSet::new();
    visited.insert(root.reference.id.clone());
    self.load_descendants(root, &mut children_by_parent, &mut visited)
  }

  fn load_descendants(
    &self,
    session: LoadedSession,
    children_by_parent: &mut HashMap<String, Vec<SessionRef>>,
    visited: &mut HashSet<String>,
  ) -> Result<LoadedSessionTree, String> {
    let mut children = Vec::new();
    for child in children_by_parent.remove(&session.reference.id).unwrap_or_default() {
      if !visited.insert(child.id.clone()) {
        continue;
      }
      let loaded = self
        .load_reference(&child)
        .map_err(|err| format!("failed to load child session `{}`: {err}", child.id))?;
      children.push(self.load_descendants(loaded, children_by_parent, visited)?);
    }

    Ok(LoadedSessionTree { session, children })
  }

  fn load_reference(&self, reference: &SessionRef) -> Result<LoadedSession, String> {
    match self {
      Self::Dsh(source) => source.load_session_path(&reference.path),
      Self::Codex(source) => source.load_session_path(&reference.path),
      Self::OpenCode(source) => source.load_session_exact(&reference.id),
      Self::Pi(source) => source.load_session_path(&reference.path),
      Self::WorkBuddy(source) => source.load_session_exact(&reference.id),
      Self::ZCode(source) => source.load_session_exact(&reference.id),
    }
  }
}

fn file_headers(references: Vec<SessionRef>) -> Vec<SessionHeader> {
  references.into_iter().map(file_header).collect()
}

fn file_header_catalog(relations: SessionRelationCatalog) -> SessionHeaderCatalog {
  let headers = file_headers(relations.references);
  let complete = relations.complete && headers.iter().all(|header| header.updated_at_ms.is_some());
  SessionHeaderCatalog { headers, complete }
}

fn sort_headers(headers: &mut [SessionHeader]) {
  headers.sort_by(|left, right| {
    right
      .updated_at_ms
      .cmp(&left.updated_at_ms)
      .then_with(|| right.updated_at.cmp(&left.updated_at))
      .then_with(|| right.timestamp.cmp(&left.timestamp))
      .then_with(|| right.path.cmp(&left.path))
  });
}

fn path_targeting_unsupported(source: &str) -> String {
  format!("{source} does not support path-targeted session cataloging")
}

fn file_header(reference: SessionRef) -> SessionHeader {
  let updated_at_ms = std::fs::metadata(&reference.path)
    .ok()
    .and_then(|metadata| metadata.modified().ok())
    .and_then(system_time_ms);
  SessionHeader {
    id: reference.id,
    parent_session_id: reference.parent_session_id,
    agent_path: reference.agent_path,
    agent_nickname: reference.agent_nickname,
    agent_role: reference.agent_role,
    title: reference.title,
    preview: reference.preview,
    path: reference.path,
    cwd: reference.cwd,
    timestamp: reference.timestamp,
    updated_at: updated_at_ms.map(|value| value.to_string()),
    updated_at_ms,
  }
}

fn system_time_ms(value: SystemTime) -> Option<i64> {
  match value.duration_since(UNIX_EPOCH) {
    Ok(duration) => i64::try_from(duration.as_millis()).ok(),
    Err(error) => i64::try_from(error.duration().as_millis()).ok().map(|value| -value),
  }
}

pub(crate) fn session_source(source: Source, session_dir: Option<PathBuf>) -> Result<SessionSourceClient, String> {
  match source {
    Source::Dsh => Ok(SessionSourceClient::Dsh(DshSessionSource::new(session_dir))),
    Source::Pi => Ok(SessionSourceClient::Pi(PiSessionSource::new(session_dir))),
    Source::Codex => Ok(SessionSourceClient::Codex(CodexSessionSource::new(session_dir))),
    Source::OpenCode => Ok(SessionSourceClient::OpenCode(OpenCodeSessionSource::new(session_dir))),
    Source::WorkBuddy => Ok(SessionSourceClient::WorkBuddy(WorkBuddySessionSource::new(session_dir))),
    Source::ZCode => Ok(SessionSourceClient::ZCode(ZCodeSessionSource::new(session_dir))),
  }
}
