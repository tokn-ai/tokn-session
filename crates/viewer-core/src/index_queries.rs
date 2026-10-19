//! SQLite-only queries used to admit and locate on-demand snapshots.
use crate::{
  model::{SessionLocator, ViewerProvider},
  service::{index_catalog_source_key, index_source_key_for_path, indexed_session_header},
  service_protocol::CatalogEntry,
};
use tokn_session_index::{IndexedSession, SessionIndex, SessionKey};

pub(crate) fn snapshot_entries(index: &SessionIndex) -> Result<Vec<CatalogEntry>, String> {
  let mut ready = std::collections::BTreeSet::new();
  for provider in ViewerProvider::ALL {
    if index
      .source_state(&index_catalog_source_key(provider))
      .map_err(|e| e.to_string())?
      .is_some()
    {
      ready.insert(provider.as_str());
    }
  }
  let sessions = index.list_present_sessions().map_err(|e| e.to_string())?;
  let admitted = index
    .managed_session_admissions()
    .map_err(|error| error.to_string())?
    .into_iter()
    .collect::<std::collections::HashSet<_>>();
  Ok(
    sessions
      .iter()
      .filter(|session| ready.contains(session.key.provider.as_str()) || admitted.contains(&session.key))
      .filter_map(|session| snapshot_entry_from_row(session).ok())
      .collect(),
  )
}

pub(crate) fn snapshot_entry(index: &SessionIndex, locator: &SessionLocator) -> Result<Option<CatalogEntry>, String> {
  let source = index_source_key_for_path(locator.provider, &locator.source_path)?;
  let key = SessionKey::new(source.provider, source.source_key, &locator.session_id);
  // A partial first catalog stays closed except for an exact viewer-created
  // session whose native identity and registered project were verified.
  if index
    .source_state(&index_catalog_source_key(locator.provider))
    .map_err(|e| e.to_string())?
    .is_none()
    && !index
      .has_managed_session_admission(&key)
      .map_err(|error| error.to_string())?
  {
    return Ok(None);
  }
  index
    .session(&key)
    .map_err(|e| e.to_string())?
    .filter(|session| session.present)
    .map(|session| snapshot_entry_from_row(&session))
    .transpose()
}

pub(crate) fn snapshot_entry_for_key(index: &SessionIndex, key: &str) -> Result<Option<CatalogEntry>, String> {
  let (provider, source_path, session_id) =
    serde_json::from_str(key).map_err(|e| format!("Invalid snapshot key: {e}"))?;
  let entry = snapshot_entry(
    index,
    &SessionLocator {
      version: 1,
      provider,
      source_path,
      session_id,
    },
  )?;
  Ok(entry.filter(|entry| entry.key == key))
}

fn snapshot_entry_from_row(session: &IndexedSession) -> Result<CatalogEntry, String> {
  let header = indexed_session_header(session)?;
  let provider = serde_json::from_value::<tokn_session_core::Provider>(serde_json::json!(session.key.provider))
    .map_err(|e| e.to_string())?;
  Ok(CatalogEntry {
    key: serde_json::to_string(&(provider, &header.path, &header.id)).map_err(|e| e.to_string())?,
    provider,
    header,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use tokn_session_index::{RegisteredProjectRecord, SessionMetadata, SourceReplacement, SourceState};

  #[test]
  fn cold_catalog_admits_only_exact_owned_sessions_without_a_provider_sentinel() {
    let index = SessionIndex::open_in_memory().unwrap();
    index
      .register_project(RegisteredProjectRecord {
        project_id: "project".into(),
        group_key: "/project".into(),
        folder_path: "/project".into(),
        name: "Project".into(),
        created_at_ms: 0,
      })
      .unwrap();
    let owned = SessionLocator {
      version: 1,
      provider: ViewerProvider::Pi,
      source_path: "/owned.jsonl".into(),
      session_id: "owned".into(),
    };
    let unrelated = SessionLocator {
      version: 1,
      provider: ViewerProvider::Pi,
      source_path: "/unrelated.jsonl".into(),
      session_id: "unrelated".into(),
    };
    for locator in [&owned, &unrelated] {
      let source = index_source_key_for_path(locator.provider, &locator.source_path).unwrap();
      let key = SessionKey::new(source.provider.clone(), source.source_key.clone(), &locator.session_id);
      index
        .replace_source(SourceReplacement::baseline(
          SourceState::new(source, "one", 0),
          vec![SessionMetadata::new(key, locator.source_path.to_str().unwrap())],
        ))
        .unwrap();
    }
    assert!(snapshot_entry(&index, &owned).unwrap().is_none());
    let source = index_source_key_for_path(owned.provider, &owned.source_path).unwrap();
    let key = SessionKey::new(source.provider.clone(), source.source_key.clone(), &owned.session_id);
    index.admit_managed_session(&key, "project").unwrap();
    let entry = snapshot_entry(&index, &owned).unwrap().unwrap();
    assert_eq!(entry.header.id, owned.session_id);
    assert_eq!(
      snapshot_entry_for_key(&index, &entry.key).unwrap().unwrap().key,
      entry.key
    );
    assert!(snapshot_entry(&index, &unrelated).unwrap().is_none());
    assert!(
      snapshot_entry(
        &index,
        &SessionLocator {
          session_id: "other".into(),
          ..owned.clone()
        }
      )
      .unwrap()
      .is_none()
    );
    assert_eq!(snapshot_entries(&index).unwrap().len(), 1);
    assert!(
      index
        .source_state(&index_catalog_source_key(ViewerProvider::Pi))
        .unwrap()
        .is_none()
    );
    index
      .replace_source(SourceReplacement::baseline(SourceState::new(source, "two", 1), vec![]))
      .unwrap();
    assert!(snapshot_entry(&index, &owned).unwrap().is_none());
    assert!(snapshot_entries(&index).unwrap().is_empty());
  }
}
