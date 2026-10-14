use super::*;
use crate::rollout_path::rollout_thread_id;

/// Exported/custom roots may have no Desktop database. Collapse duplicates
/// only when one verified lineage contains every other file for the same ID.
/// Independent copies stay ambiguous instead of choosing a branch by date.
pub(super) fn retain_lineage_heads(source: &CodexSessionSource, references: &mut Vec<SessionRef>) {
  let mut groups = HashMap::<&str, Vec<&Path>>::new();
  for reference in references.iter() {
    groups.entry(&reference.id).or_default().push(&reference.path);
  }
  let mut heads = HashMap::new();
  for (id, paths) in groups.into_iter().filter(|(_, paths)| paths.len() > 1) {
    let candidates = paths
      .iter()
      .filter_map(|path| {
        let segments = source.history_segments(path).ok()?;
        (segments.len() > 1
          && paths.iter().all(|candidate| {
            segments
              .iter()
              .any(|segment| paths_refer_to_same_file(candidate, &segment.path))
          }))
        .then_some((*path).to_path_buf())
      })
      .collect::<Vec<_>>();
    if let [head] = candidates.as_slice() {
      heads.insert(id.to_string(), head.clone());
    }
  }
  references.retain(|reference| {
    heads
      .get(&reference.id)
      .is_none_or(|head| paths_refer_to_same_file(head, &reference.path))
  });
}

/// A paginated revert leaves the old physical rollout in place as a bounded
/// history source. Only the current file in Desktop's catalog is a session
/// head. Select it without reopening every unchanged rollout during indexing.
pub(super) fn retain_current_rollouts(home: &Path, paths: &mut Vec<PathBuf>) {
  let mut paths_by_id = HashMap::<&str, Vec<&Path>>::new();
  for path in paths.iter() {
    if let Some(id) = rollout_thread_id(path) {
      paths_by_id.entry(id).or_default().push(path);
    }
  }
  paths_by_id.retain(|_, paths| paths.len() > 1);
  if paths_by_id.is_empty() {
    return;
  }
  let Ok(metadata) = read_state_metadata(home, &paths_by_id) else {
    return;
  };
  // Filename matching only narrows candidates. Verify the indexed head's
  // owning header before hiding any other file for that logical thread.
  let heads = metadata
    .into_iter()
    .filter_map(|(id, metadata)| {
      let path = crate::storage::resolve_path(&metadata.rollout_path?);
      let reference = inspect_session_header(&path).ok()?;
      (reference.id == id).then_some((id, path))
    })
    .collect::<HashMap<_, _>>();
  paths.retain(|path| {
    rollout_thread_id(path)
      .and_then(|id| heads.get(id))
      .is_none_or(|head| paths_refer_to_same_file(path, head))
  });
}

#[cfg(test)]
mod tests {
  use super::*;

  const ID: &str = "019ff730-9112-7cc0-92c5-1287c3241472";

  #[test]
  fn catalog_selects_a_compressed_head_when_desktop_retains_its_plain_path() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path();
    let original = home.join(format!("rollout-2026-08-13T02-16-23-{ID}.jsonl.zst"));
    let current_plain = home.join(format!("rollout-2026-09-18T18-32-33-{ID}_continuation.jsonl"));
    let current = current_plain.with_extension("jsonl.zst");
    let header = format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{ID}\"}}}}\n");
    for path in [&original, &current] {
      std::fs::write(path, zstd::stream::encode_all(header.as_bytes(), 0).unwrap()).unwrap();
    }
    let connection = Connection::open(home.join(STATE_DB_FILENAME)).unwrap();
    connection
      .execute_batch("create table threads (id text, rollout_path text, title text);")
      .unwrap();
    connection
      .execute(
        "insert into threads values (?1, ?2, 'Compressed title')",
        rusqlite::params![ID, current_plain.to_str()],
      )
      .unwrap();
    let mut paths = vec![original, current.clone()];
    retain_current_rollouts(home, &mut paths);
    assert_eq!(paths, [current]);
    assert!(!current_plain.exists());
  }

  #[test]
  fn catalogs_the_current_segment_without_hiding_unrelated_or_unindexed_files() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path();
    let sessions = home.join("sessions");
    std::fs::create_dir(&sessions).unwrap();
    let original = sessions.join(format!("rollout-2026-08-13T02-16-23-{ID}.jsonl"));
    let current = sessions.join(format!("rollout-2026-09-18T18-32-33-{ID}_continuation.jsonl"));
    let unrelated = sessions.join("custom.jsonl");
    for path in [&original, &current, &unrelated] {
      std::fs::write(
        path,
        format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{ID}\"}}}}\n"),
      )
      .unwrap();
    }
    let connection = Connection::open(home.join(STATE_DB_FILENAME)).unwrap();
    connection
      .execute_batch("create table threads (id text, rollout_path text, title text);")
      .unwrap();
    connection
      .execute(
        "insert into threads values (?1, ?2, 'Example')",
        rusqlite::params![ID, current.to_str()],
      )
      .unwrap();
    let mut paths = vec![original.clone(), current.clone(), unrelated.clone()];
    retain_current_rollouts(home, &mut paths);
    assert_eq!(paths, [current.clone(), unrelated]);

    // A stale native catalog cannot hide all the available history.
    std::fs::remove_file(&current).unwrap();
    let mut paths = vec![original.clone()];
    retain_current_rollouts(home, &mut paths);
    assert_eq!(paths, [original]);
  }

  #[test]
  fn refuses_to_hide_files_when_the_indexed_head_has_another_owner() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path();
    let original = home.join(format!("rollout-2026-08-13T02-16-23-{ID}.jsonl"));
    let current = home.join(format!("rollout-2026-09-18T18-32-33-{ID}_continuation.jsonl"));
    std::fs::write(
      &original,
      format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{ID}\"}}}}\n"),
    )
    .unwrap();
    std::fs::write(
      &current,
      "{\"type\":\"session_meta\",\"payload\":{\"id\":\"different\"}}\n",
    )
    .unwrap();
    let connection = Connection::open(home.join(STATE_DB_FILENAME)).unwrap();
    connection
      .execute_batch("create table threads (id text, rollout_path text, title text);")
      .unwrap();
    connection
      .execute(
        "insert into threads values (?1, ?2, 'Example')",
        rusqlite::params![ID, current.to_str()],
      )
      .unwrap();
    let mut paths = vec![original, current];
    let before = paths.clone();
    retain_current_rollouts(home, &mut paths);
    assert_eq!(paths, before);
  }

  #[test]
  fn custom_catalog_selects_a_verified_continuation_but_keeps_independent_copies() {
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path().join("original.jsonl");
    let head = directory.path().join("continued.jsonl");
    let header =
      serde_json::json!({"ordinal":0,"type":"session_meta","payload":{"id":"thread","history_mode":"paginated"}})
        .to_string()
        + "\n";
    std::fs::write(&base, &header).unwrap();
    std::fs::write(&head, serde_json::json!({"ordinal":1,"type":"session_meta","payload":{"id":"thread","history_mode":"paginated","history_base":{"thread_id":"thread","end_ordinal_exclusive":1,"end_byte_offset":header.len()}}}).to_string() + "\n").unwrap();
    let source = CodexSessionSource::new(Some(directory.path().to_path_buf()));
    let catalog = source.list_session_relations().unwrap();
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].path, head);

    // An unrelated duplicate cannot be discarded merely because it is older.
    let copy = directory.path().join("independent.jsonl");
    std::fs::write(copy, &header).unwrap();
    assert_eq!(source.list_session_relations().unwrap().len(), 3);
  }
}
