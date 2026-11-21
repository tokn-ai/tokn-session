use std::path::{Path, PathBuf};
use tokn_session_client::{AgentClient, Source};

fn root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/qoder")
}

#[test]
fn qoder_file_catalog_exact_path_and_subagent_tree_share_one_source() {
  let root = root();
  let headers = AgentClient::list_session_headers(Source::Qoder, Some(root.clone())).unwrap();
  assert_eq!(headers.len(), 2);
  assert_eq!(
    AgentClient::file_session_paths(Source::Qoder, Some(root.clone()))
      .unwrap()
      .len(),
    2
  );
  assert_eq!(
    AgentClient::session_catalog_roots(Source::Qoder, Some(root.clone())).unwrap(),
    vec![root.clone()]
  );
  let header =
    AgentClient::session_header_at_path(Source::Qoder, Some(root.clone()), &root.join("main.jsonl")).unwrap();
  assert_eq!(header.id, "main");
  let tree = AgentClient::load_session_tree(Source::Qoder, Some(root.clone()), "main").unwrap();
  assert_eq!(tree.children.len(), 1);
  assert_eq!(tree.children[0].session.reference.id, "main/agent/worker");
  let path = root.join("main.jsonl");
  assert_eq!(
    AgentClient::load_session(Source::Qoder, None, path.to_str().unwrap())
      .unwrap()
      .reference
      .id,
    "main"
  );
}
