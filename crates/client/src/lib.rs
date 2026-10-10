mod print;
mod session_source;
mod source;

use std::path::{Path, PathBuf};

use tokn_session_core::{LoadedSession, LoadedSessionTree, SessionRef};

pub use print::{AppendAction, AppendSessionRequest, CreateSessionRequest};
pub use source::Source;
pub use tokn_session_core::SessionHeader;

pub struct AgentClient;

impl AgentClient {
  /// Lists session headers without computing conversation counts.
  ///
  /// File-backed providers read only their session header, while catalog-backed
  /// providers read only their session rows. This is the appropriate discovery
  /// API for latency-sensitive viewers and other metadata-only consumers.
  pub fn list_session_headers(source: Source, session_dir: Option<PathBuf>) -> Result<Vec<SessionHeader>, String> {
    session_source::session_source(source, session_dir)?.list_session_headers()
  }

  /// Returns the effective session-file roots for a provider that supports
  /// path-targeted cataloging.
  ///
  /// The returned paths are the provider's own resolved discovery roots, so
  /// callers do not need to duplicate environment-variable or platform-home
  /// handling when registering filesystem watchers.
  pub fn file_session_roots(source: Source, session_dir: Option<PathBuf>) -> Result<Vec<PathBuf>, String> {
    session_source::session_source(source, session_dir)?.file_session_roots()
  }

  /// Enumerates file-backed sessions without opening their contents.
  pub fn file_session_paths(source: Source, session_dir: Option<PathBuf>) -> Result<Vec<PathBuf>, String> {
    session_source::session_source(source, session_dir)?.file_session_paths()
  }

  /// Applies provider catalog metadata that is stored outside session files.
  pub fn apply_catalog_metadata(
    source: Source,
    session_dir: Option<PathBuf>,
    headers: &mut [SessionHeader],
  ) -> Result<(), String> {
    session_source::session_source(source, session_dir)?.apply_catalog_metadata(headers);
    Ok(())
  }

  /// Reads one file-backed session header at a known path.
  ///
  /// This avoids enumerating a provider's whole catalog and does not inspect
  /// the conversation body. It is currently available for Codex and Pi,
  /// whose persisted histories are one session per JSONL file. Codex reads
  /// only the rollout header here; its optional Desktop state and legacy-index
  /// presentation metadata can be applied in one batch with
  /// [`AgentClient::apply_catalog_metadata`].
  pub fn session_header_at_path(
    source: Source,
    session_dir: Option<PathBuf>,
    path: &Path,
  ) -> Result<SessionHeader, String> {
    session_source::session_source(source, session_dir)?.session_header_at_path(path)
  }

  /// Resolves presentation metadata that requires inspecting one session body.
  ///
  /// Call this only for visible or search-relevant headers. Bulk discovery is
  /// intentionally kept body-free for latency-sensitive clients.
  pub fn hydrate_session_header(source: Source, header: SessionHeader) -> Result<SessionHeader, String> {
    session_source::session_source(source, None)?.hydrate_session_header(header)
  }

  /// Lists sessions with provider-specific message counts.
  ///
  /// This preserves the CLI's counted-list behavior and may scan every session
  /// body. Prefer [`AgentClient::list_session_headers`] when counts are not
  /// required.
  pub fn list_sessions(source: Source, session_dir: Option<PathBuf>) -> Result<Vec<SessionRef>, String> {
    session_source::session_source(source, session_dir)?.list_sessions()
  }

  pub fn load_session(source: Source, session_dir: Option<PathBuf>, session: &str) -> Result<LoadedSession, String> {
    session_source::session_source(source, session_dir)?.load_session(session)
  }

  pub fn load_session_tree(
    source: Source,
    session_dir: Option<PathBuf>,
    session: &str,
  ) -> Result<LoadedSessionTree, String> {
    let plan = tree_discovery_plan(source, session_dir, session);
    let client = session_source::session_source(source, plan.session_dir)?;
    let Some(path) = plan.explicit_path else {
      return client.load_session_tree(session);
    };

    let root = client.load_session_path(&path)?;
    let mut references = client.list_session_relations()?;
    if let Some(supplemental_dir) = plan.supplemental_dir {
      references.extend(session_source::session_source(source, Some(supplemental_dir))?.list_session_relations()?);
    }
    client.load_session_tree_from(root, references)
  }

  pub fn create_session(request: CreateSessionRequest) -> Result<(), String> {
    print::create_session(request)
  }

  pub fn append_session(request: AppendSessionRequest) -> Result<(), String> {
    print::append_session(request)
  }
}

#[derive(Debug, Eq, PartialEq)]
struct TreeDiscoveryPlan {
  session_dir: Option<PathBuf>,
  supplemental_dir: Option<PathBuf>,
  explicit_path: Option<PathBuf>,
}

fn tree_discovery_plan(source: Source, session_dir: Option<PathBuf>, session: &str) -> TreeDiscoveryPlan {
  if matches!(source, Source::OpenCode | Source::WorkBuddy | Source::ZCode) {
    return TreeDiscoveryPlan {
      session_dir,
      supplemental_dir: None,
      explicit_path: None,
    };
  }

  let session_path = Path::new(session);
  if !session_path.is_file() {
    return TreeDiscoveryPlan {
      session_dir,
      supplemental_dir: None,
      explicit_path: None,
    };
  }

  let supplemental_dir = session_dir.is_none().then(|| {
    session_path
      .parent()
      .filter(|path| !path.as_os_str().is_empty())
      .unwrap_or_else(|| Path::new("."))
      .to_path_buf()
  });
  TreeDiscoveryPlan {
    session_dir,
    supplemental_dir,
    explicit_path: Some(session_path.to_path_buf()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fixture_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("tests/fixtures")
      .join(relative)
  }

  #[test]
  fn header_listing_stops_before_an_invalid_conversation_body() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("header-only.jsonl");
    std::fs::write(
      &path,
      concat!(
        r#"{"timestamp":"2026-08-31T00:00:00Z","type":"session_meta","payload":{"id":"header-only","timestamp":"2026-08-31T00:00:00Z","cwd":"/tmp/project"}}"#,
        "\n",
        "{invalid conversation record\n",
      ),
    )
    .unwrap();

    let headers = AgentClient::list_session_headers(Source::Codex, Some(directory.path().to_path_buf())).unwrap();

    assert_eq!(headers.len(), 1);
    assert_eq!(headers[0].id, "header-only");
    assert_eq!(headers[0].path, path);
    assert_eq!(headers[0].cwd.as_deref(), Some("/tmp/project"));
    assert_eq!(headers[0].timestamp.as_deref(), Some("2026-08-31T00:00:00Z"));
    let expected_updated_at_ms = i64::try_from(
      std::fs::metadata(&headers[0].path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis(),
    )
    .unwrap();
    assert_eq!(headers[0].updated_at_ms, Some(expected_updated_at_ms));
    assert_eq!(
      headers[0].updated_at.as_deref(),
      Some(expected_updated_at_ms.to_string().as_str())
    );

    let header = AgentClient::session_header_at_path(Source::Codex, Some(directory.path().to_path_buf()), &path)
      .expect("targeted header should stop before the invalid conversation body");
    assert_eq!(header.id, "header-only");
    assert_eq!(header.path, path);
    assert_eq!(header.cwd.as_deref(), Some("/tmp/project"));
    assert_eq!(header.timestamp.as_deref(), Some("2026-08-31T00:00:00Z"));
    assert_eq!(header.updated_at_ms, Some(expected_updated_at_ms));
  }

  #[test]
  fn exposes_effective_file_roots_for_targeted_file_providers() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let root = directory.path().to_path_buf();

    assert_eq!(
      AgentClient::file_session_roots(Source::Codex, Some(root.clone()))
        .expect("Codex should expose its explicit root"),
      vec![root.clone()]
    );
    assert_eq!(
      AgentClient::file_session_roots(Source::Pi, Some(root.clone())).expect("Pi should expose its explicit root"),
      vec![root]
    );
  }

  #[test]
  fn targeted_pi_header_stops_before_an_invalid_conversation_body() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let path = directory.path().join("pi-targeted.jsonl");
    std::fs::write(
      &path,
      concat!(
        r#"{"type":"session","id":"pi-targeted","cwd":"/tmp/pi-project","timestamp":"2026-09-02T00:00:00Z"}"#,
        "\n",
        "{invalid conversation record\n",
      ),
    )
    .expect("Pi fixture should be written");

    let header = AgentClient::session_header_at_path(Source::Pi, Some(directory.path().to_path_buf()), &path)
      .expect("targeted Pi header should stop before the invalid conversation body");

    assert_eq!(header.id, "pi-targeted");
    assert_eq!(header.path, path);
    assert_eq!(header.cwd.as_deref(), Some("/tmp/pi-project"));
    assert_eq!(header.timestamp.as_deref(), Some("2026-09-02T00:00:00Z"));
    assert!(header.updated_at_ms.is_some());
  }

  #[test]
  fn rejects_path_targeting_for_catalog_backed_providers() {
    let error = AgentClient::file_session_roots(Source::OpenCode, None)
      .expect_err("OpenCode sessions are not one-file-per-session catalogs");

    assert!(error.contains("opencode"));
    assert!(error.contains("path-targeted"));
  }

  #[test]
  fn loads_dsh_tree_without_forks_or_inherited_parent_messages() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = include_str!("../tests/fixtures/dsh/basic/session.jsonl");
    for (id, metadata) in [
      ("root", ""),
      (
        "child",
        ",\"parentSession\":\"root\",\"origin\":\"subagent\",\"seedLength\":2",
      ),
      ("fork", ",\"parentSession\":\"root\",\"seedLength\":2"),
    ] {
      let folder = dir.path().join(id);
      std::fs::create_dir(&folder).unwrap();
      let content = fixture
        .replace("dsh-fixture", id)
        .replace("\"delegationDepth\":0", &format!("\"delegationDepth\":0{metadata}"));
      std::fs::write(folder.join("session.jsonl"), content).unwrap();
    }
    let path = dir.path().join("root/session.jsonl");
    let tree = AgentClient::load_session_tree(Source::Dsh, Some(dir.path().into()), path.to_str().unwrap()).unwrap();
    assert_eq!(tree.session.reference.id, "root");
    assert_eq!(tree.children.len(), 1);
    assert_eq!(tree.children[0].session.reference.id, "child");
    assert_eq!(tree.children[0].session.reference.message_count, 3);
  }

  #[test]
  fn loads_codex_session_tree_recursively() {
    let fixtures = fixture_path("codex");
    let tree =
      AgentClient::load_session_tree(Source::Codex, Some(fixtures), "tree-root").expect("fixture tree should load");

    assert_eq!(tree.session.reference.id, "tree-root");
    assert_eq!(tree.children.len(), 1);
    assert_eq!(tree.children[0].session.reference.id, "tree-child");
    assert_eq!(
      tree.children[0]
        .session
        .reference
        .path
        .file_name()
        .and_then(|name| name.to_str()),
      Some("tree_child.jsonl")
    );
    assert_eq!(tree.children[0].children.len(), 1);
    assert_eq!(tree.children[0].children[0].session.reference.id, "tree-grandchild");
  }

  #[test]
  fn discovers_codex_siblings_for_an_explicit_session_path() {
    let fixtures = fixture_path("codex");
    let root = fixtures.join("tree_root.jsonl");
    let tree = AgentClient::load_session_tree(
      Source::Codex,
      Some(fixtures),
      root.to_str().expect("fixture path should be utf-8"),
    )
    .expect("fixture tree should load");

    assert_eq!(tree.session.reference.id, "tree-root");
    assert_eq!(tree.children.len(), 1);
    assert_eq!(tree.children[0].session.reference.id, "tree-child");
  }

  #[test]
  fn loads_pi_session_tree_from_parent_paths() {
    let fixtures = fixture_path("pi");
    let tree =
      AgentClient::load_session_tree(Source::Pi, Some(fixtures), "pi-tree-root").expect("fixture tree should load");

    assert_eq!(tree.session.reference.id, "pi-tree-root");
    assert_eq!(tree.children.len(), 1);
    assert_eq!(tree.children[0].session.reference.id, "pi-tree-child");
  }

  #[test]
  fn discovers_pi_siblings_for_an_explicit_session_path() {
    let fixtures = fixture_path("pi");
    let root = fixtures.join("tree_parent.jsonl");
    let tree = AgentClient::load_session_tree(
      Source::Pi,
      Some(fixtures),
      root.to_str().expect("fixture path should be utf-8"),
    )
    .expect("fixture tree should load");

    assert_eq!(tree.session.reference.id, "pi-tree-root");
    assert_eq!(tree.children.len(), 1);
    assert_eq!(tree.children[0].session.reference.id, "pi-tree-child");
  }

  #[test]
  fn explicit_file_keeps_default_discovery_and_adds_its_directory() {
    let root = fixture_path("codex/tree_root.jsonl");
    let plan = tree_discovery_plan(
      Source::Codex,
      None,
      root.to_str().expect("fixture path should be utf-8"),
    );

    assert_eq!(plan.session_dir, None);
    assert_eq!(plan.supplemental_dir, root.parent().map(Path::to_path_buf));
    assert_eq!(plan.explicit_path.as_deref(), Some(root.as_path()));
  }

  #[test]
  fn explicit_codex_path_finds_descendants_in_other_discovery_subdirectories() {
    let fixtures = fixture_path("codex/tree_cross_day");
    let root = fixtures.join("day_one/root.jsonl");
    let tree = AgentClient::load_session_tree(
      Source::Codex,
      Some(fixtures),
      root.to_str().expect("fixture path should be utf-8"),
    )
    .expect("cross-directory fixture tree should load");

    assert_eq!(tree.session.reference.id, "cross-day-root");
    assert_eq!(tree.children.len(), 1);
    assert_eq!(tree.children[0].session.reference.id, "cross-day-child");
  }
}
