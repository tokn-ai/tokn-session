use super::*;
use std::{fs, io::Write, path::Path};
use tempfile::TempDir;
use tokn_session_client::Source;

const HEADER: &str = "{\"type\":\"session\",\"id\":\"pi-session\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}\n";
const MESSAGE: &str = "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"first prompt\"}}\n";

fn named(name: &str) -> String {
  format!("{}\n", serde_json::json!({"type": "session_info", "name": name}))
}

fn fixture(root: &Path) -> CatalogEntry {
  CatalogEntry {
    key: root.display().to_string(),
    provider: Provider::Pi,
    header: AgentClient::list_session_headers(Source::Pi, Some(root.into()))
      .unwrap()
      .remove(0),
  }
}

fn append(path: &Path, text: &str) {
  fs::OpenOptions::new()
    .append(true)
    .open(path)
    .unwrap()
    .write_all(text.as_bytes())
    .unwrap();
}

fn update(cache: &PresentationCache, entry: &CatalogEntry) -> SessionHeader {
  cache.reconcile(std::slice::from_ref(entry));
  cache.step();
  cache.decorate(std::slice::from_ref(entry)).remove(0).header
}

#[test]
fn pi_decodes_history_once_then_only_complete_appended_lines() {
  let root = TempDir::new().unwrap();
  let path = root.path().join("session.jsonl");
  fs::write(
    &path,
    format!(
      "{HEADER}{}{MESSAGE}{}",
      MESSAGE.replace("user", "assistant"),
      named("Task")
    ),
  )
  .unwrap();
  let entry = fixture(root.path());
  let cache = PresentationCache::default();
  let header = update(&cache, &entry);
  assert_eq!(header.title.as_deref(), Some("Task"));
  assert_eq!(header.preview.as_deref(), Some("first prompt"));
  assert_eq!(cache.state.lock().unwrap().entries[&entry.key].pi.decoded, 4);
  for _ in 0..3 {
    update(&cache, &entry);
  }
  assert_eq!(cache.state.lock().unwrap().entries[&entry.key].pi.decoded, 4);

  append(&path, named("Renamed").trim_end());
  assert_eq!(update(&cache, &entry).title.as_deref(), Some("Task"));
  assert_eq!(cache.state.lock().unwrap().entries[&entry.key].pi.decoded, 4);
  append(&path, "\n");
  assert_eq!(update(&cache, &entry).title.as_deref(), Some("Renamed"));
  assert_eq!(cache.state.lock().unwrap().entries[&entry.key].pi.decoded, 5);
  append(&path, &named("  "));
  let cleared = update(&cache, &entry);
  assert_eq!(cleared.title, None);
  assert_eq!(cleared.preview.as_deref(), Some("first prompt"));
  assert_eq!(cache.state.lock().unwrap().entries[&entry.key].pi.decoded, 6);
}

#[test]
fn pi_metadata_uses_a_bounded_batch_even_when_the_source_exceeds_the_old_snapshot_limit() {
  let root = TempDir::new().unwrap();
  let path = root.path().join("session.jsonl");
  fs::write(&path, format!("{HEADER}{MESSAGE}{}", named("Large session"))).unwrap();
  let entry = fixture(root.path());
  let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
  // Only the initial batch is relevant. A sparse, unread suffix makes it
  // possible to check admission independently of materializing 128 MiB.
  let mut padding = vec![b' '; 8192];
  *padding.last_mut().unwrap() = b'\n';
  for _ in 0..=SCAN_BUDGET / padding.len() as u64 {
    file.write_all(&padding).unwrap();
  }
  file.set_len(129 * 1024 * 1024).unwrap();
  let mut cursor = PiCursor::default();
  assert!(
    !cursor
      .read(&entry.header, &revision(&entry.header, Provider::Pi))
      .unwrap()
  );
  assert_eq!(cursor.summary.title.as_deref(), Some("Large session"));
  assert_eq!(cursor.summary.preview.as_deref(), Some("first prompt"));
  assert!(cursor.offset <= SCAN_BUDGET + padding.len() as u64);
  assert_eq!(cursor.decoded, 3);
}

#[test]
fn pi_replacement_truncation_and_same_length_rewrites_reset_metadata() {
  let root = TempDir::new().unwrap();
  let path = root.path().join("session.jsonl");
  fs::write(&path, format!("{HEADER}{MESSAGE}{}", named("Old"))).unwrap();
  let entry = fixture(root.path());
  let cache = PresentationCache::default();
  update(&cache, &entry);
  let replacement = root.path().join("replacement");
  fs::write(
    &replacement,
    format!(
      "{HEADER}{}{}",
      MESSAGE.replace("first prompt", "other prompt"),
      named("New")
    ),
  )
  .unwrap();
  fs::rename(replacement, &path).unwrap();
  let replaced = update(&cache, &entry);
  assert_eq!(replaced.title.as_deref(), Some("New"));
  assert_eq!(replaced.preview.as_deref(), Some("other prompt"));
  fs::write(&path, HEADER).unwrap();
  let truncated = update(&cache, &entry);
  assert_eq!(truncated.title, None);
  assert_eq!(truncated.preview, None);
  fs::write(&path, format!("{HEADER}{}", named("Old"))).unwrap();
  update(&cache, &entry);
  // File timestamps have finite resolution; ensure the same-length rewrite
  // carries a distinct revision without relying on elapsed wall time.
  fs::write(&path, format!("{HEADER}{}", named("New"))).unwrap();
  fs::File::options()
    .write(true)
    .open(&path)
    .unwrap()
    .set_modified(std::time::SystemTime::UNIX_EPOCH)
    .unwrap();
  assert_eq!(update(&cache, &entry).title.as_deref(), Some("New"));
}

#[test]
fn metadata_is_bounded_isolated_and_last_good_survives_bad_appends() {
  let root = TempDir::new().unwrap();
  let path = root.path().join("session.jsonl");
  fs::write(
    &path,
    format!(
      "{HEADER}{}{}",
      MESSAGE.replace("first prompt", &"界".repeat(900)),
      named(&"名".repeat(900))
    ),
  )
  .unwrap();
  let entry = fixture(root.path());
  let cache = PresentationCache::default();
  let header = update(&cache, &entry);
  assert_eq!(header.title.as_ref().unwrap().chars().count(), TEXT_LIMIT);
  assert_eq!(header.preview.as_ref().unwrap().chars().count(), TEXT_LIMIT);
  append(&path, "invalid-json\n");
  assert_eq!(update(&cache, &entry), header);
  assert_eq!(cache.state.lock().unwrap().entries[&entry.key].pi.decoded, 3);
  let mut unrelated = entry.clone();
  unrelated.key = "different identity".into();
  assert_eq!(cache.decorate(&[unrelated]).remove(0).header.title, None);
  cache.reconcile(&[]);
  assert!(cache.state.lock().unwrap().entries.is_empty());
}

#[test]
fn pi_backfill_yields_between_bounded_batches_without_publishing_partial_names() {
  let root = TempDir::new().unwrap();
  let path = root.path().join("session.jsonl");
  let padding = format!(
    "{}\n",
    serde_json::json!({"type": "extension", "data": "x".repeat(SCAN_BUDGET as usize)})
  );
  fs::write(&path, format!("{HEADER}{}{padding}{}", named("Old"), named("Latest"))).unwrap();
  let entry = fixture(root.path());
  let mut cursor = PiCursor::default();
  let revision = revision(&entry.header, Provider::Pi);
  assert!(!cursor.read(&entry.header, &revision).unwrap());
  let decoded = cursor.decoded;
  assert!(cursor.read(&entry.header, &revision).unwrap());
  assert_eq!(cursor.summary.title.as_deref(), Some("Latest"));
  assert_eq!(cursor.decoded, decoded + 1);
}
