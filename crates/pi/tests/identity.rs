use serde_json::{Value, json};
use std::path::Path;
use tokn_session_pi::{MAX_SESSION_HEADER_BYTES, verify_session_identity};

fn header(cwd: &Path) -> Value {
  // Preserve the provider-native fixture's schema and forward-compatible fields.
  let mut value: Value =
    serde_json::from_str(include_str!("../fixtures/basic_session.jsonl").lines().next().unwrap()).unwrap();
  value["id"] = json!("native-id");
  value["cwd"] = json!(cwd);
  value["futureHeaderField"] = json!({"preserved": true});
  value
}

#[test]
fn only_complete_matching_native_headers_establish_identity() {
  let directory = tempfile::tempdir().unwrap();
  let cwd = directory.path().canonicalize().unwrap();
  let path = directory.path().join("native.jsonl");
  assert_eq!(verify_session_identity(&path, "native-id", &cwd).unwrap(), None);
  for incomplete in ["", "{\"type\":\"session\"", &header(&cwd).to_string()] {
    std::fs::write(&path, incomplete).unwrap();
    assert_eq!(verify_session_identity(&path, "native-id", &cwd).unwrap(), None);
  }
  // A large malformed body cannot interfere with native header verification.
  std::fs::write(&path, format!("{}\r\n{}", header(&cwd), "x".repeat(128 * 1024))).unwrap();
  let identity = verify_session_identity(&path, "native-id", &cwd).unwrap().unwrap();
  assert_eq!(identity.session_file, path.canonicalize().unwrap());
  assert_eq!(identity.session_id, "native-id");
  assert_eq!(identity.cwd, cwd);
  assert!(verify_session_identity(&path, "wrong-id", &cwd).is_err());
  let other = tempfile::tempdir().unwrap();
  assert!(verify_session_identity(&path, "native-id", &other.path().canonicalize().unwrap()).is_err());
}

#[test]
fn optional_header_metadata_cannot_veto_matching_native_identity() {
  let directory = tempfile::tempdir().unwrap();
  let cwd = directory.path().canonicalize().unwrap();
  let path = directory.path().join("native.jsonl");
  for (field, value) in [
    ("version", json!("future-version")),
    ("timestamp", json!(1_790_000_000_000_u64)),
    ("parentSession", json!({"future": "shape"})),
  ] {
    let mut value_header = header(&cwd);
    value_header[field] = value;
    std::fs::write(&path, format!("{value_header}\n")).unwrap();
    let identity = verify_session_identity(&path, "native-id", &cwd).unwrap().unwrap();
    assert_eq!(identity.session_id, "native-id", "optional field {field}");
    assert_eq!(identity.session_file, path.canonicalize().unwrap());
    assert_eq!(identity.cwd, cwd);
  }
}

#[test]
fn rejects_malformed_native_headers_and_noncanonical_expected_cwd() {
  let directory = tempfile::tempdir().unwrap();
  let cwd = directory.path().canonicalize().unwrap();
  let path = directory.path().join("native.jsonl");
  for invalid in [
    json!(null),
    json!({"type":"message","id":"native-id","cwd":cwd}),
    json!({"type":"session","id":42,"cwd":cwd}),
    json!({"type":"session","id":"native-id"}),
    json!({"type":"session","id":"native-id","cwd":false}),
  ] {
    std::fs::write(&path, format!("{invalid}\n")).unwrap();
    assert!(verify_session_identity(&path, "native-id", &cwd).is_err());
  }
  std::fs::write(&path, b"not JSON\n").unwrap();
  assert!(verify_session_identity(&path, "native-id", &cwd).is_err());
  std::fs::write(&path, format!("{}\n", header(&cwd))).unwrap();
  std::fs::create_dir(cwd.join("child")).unwrap();
  assert!(verify_session_identity(&path, "native-id", &cwd.join("child/..")).is_err());
}

#[test]
fn bounds_header_reads_including_unterminated_rows() {
  let directory = tempfile::tempdir().unwrap();
  let cwd = directory.path().canonicalize().unwrap();
  let path = directory.path().join("native.jsonl");
  let header = header(&cwd).to_string();
  let padding = " ".repeat(MAX_SESSION_HEADER_BYTES - header.len() - 1);
  std::fs::write(&path, format!("{header}{padding}\n")).unwrap();
  assert!(verify_session_identity(&path, "native-id", &cwd).unwrap().is_some());
  for suffix in ["", "\n"] {
    std::fs::write(&path, format!("{}{suffix}", "x".repeat(MAX_SESSION_HEADER_BYTES + 1))).unwrap();
    assert!(
      verify_session_identity(&path, "native-id", &cwd)
        .unwrap_err()
        .contains("size limit")
    );
  }
}

#[cfg(unix)]
#[test]
fn resolves_native_cwd_and_file_aliases_to_canonical_paths() {
  use std::os::unix::fs::symlink;
  let directory = tempfile::tempdir().unwrap();
  let cwd = directory.path().canonicalize().unwrap();
  let alias = directory.path().join("alias");
  symlink(&cwd, &alias).unwrap();
  let path = directory.path().join("native.jsonl");
  std::fs::write(&path, format!("{}\n", header(&alias))).unwrap();
  let identity = verify_session_identity(&alias.join("native.jsonl"), "native-id", &cwd)
    .unwrap()
    .unwrap();
  assert_eq!(identity.session_file, path.canonicalize().unwrap());
  assert_eq!(identity.cwd, cwd);
  assert!(verify_session_identity(&path, "native-id", &alias).is_err());
}
