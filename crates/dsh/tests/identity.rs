use std::{fs, path::PathBuf};
use tokn_session_dsh::{DshSessionSource, read_session_header_exact};

const FIXTURE: &str = include_str!("../fixtures/basic/session.jsonl");

#[test]
fn exact_header_does_not_parse_body_or_discover_unrelated_sessions() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("session.jsonl");
  let first = FIXTURE.lines().next().unwrap();
  // Neither a corrupt body nor a later session header can replace owning
  // identity. An unrelated invalid history must not be discovered at all.
  fs::write(
    &path,
    format!("{first}\nnot-json\n{{\"type\":\"session\",\"id\":\"different\"}}\n"),
  )
  .unwrap();
  fs::create_dir(directory.path().join("unrelated")).unwrap();
  fs::write(directory.path().join("unrelated/session.jsonl"), "not-json").unwrap();
  let source = DshSessionSource::new(Some(directory.path().to_owned()));
  let header = source.session_header_exact(&path, "dsh-fixture").unwrap();
  assert_eq!(header.path, path);
  assert_eq!(header.id, "dsh-fixture");
  assert_eq!(header.cwd.as_deref(), Some("/project/demo"));
  assert_eq!(header.timestamp.as_deref(), Some("1787157590000"));
  assert!(header.updated_at_ms.is_some());
  assert_eq!(header.title, None);
  assert_eq!(header.preview, None);
  assert!(
    read_session_header_exact(&path, "different")
      .unwrap_err()
      .contains("different session identity")
  );
}

#[test]
fn reads_a_header_split_across_concatenated_zstandard_frames() {
  let directory = tempfile::tempdir().unwrap();
  let split = 19;
  let mut compressed = zstd::stream::encode_all(&FIXTURE.as_bytes()[..split], 1).unwrap();
  compressed.extend(zstd::stream::encode_all(&FIXTURE.as_bytes()[split..], 1).unwrap());
  for name in ["session.jsonl.zstd", "session.jsonl.zst"] {
    let path = directory.path().join(name);
    fs::write(&path, &compressed).unwrap();
    let header = read_session_header_exact(&path, "dsh-fixture").unwrap();
    assert_eq!(header.id, "dsh-fixture");
    assert_eq!(header.path, path);
    assert_eq!(header.cwd.as_deref(), Some("/project/demo"));
    assert_eq!(fs::read(path).unwrap(), compressed);
  }
}

#[test]
fn rejects_incomplete_oversized_or_invalid_owning_headers() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("session.jsonl");
  for (text, reason) in [
    (FIXTURE.lines().next().unwrap().to_owned(), "not complete"),
    (format!("{}\n", "x".repeat(64 * 1024 + 1)), "size limit"),
    (
      FIXTURE.replace("\"version\":0", "\"version\":42"),
      "unsupported dsh session version",
    ),
    (
      "{\"type\":\"session/title\",\"id\":\"dsh-fixture\"}\n".into(),
      "invalid dsh session header",
    ),
    ("{broken}\n".into(), "invalid dsh JSON"),
  ] {
    fs::write(&path, text).unwrap();
    assert!(
      read_session_header_exact(&path, "dsh-fixture")
        .unwrap_err()
        .contains(reason),
      "{reason}"
    );
  }
  let oversized = format!("{}\n", "x".repeat(64 * 1024 + 1));
  let path = directory.path().join("session.jsonl.zstd");
  fs::write(&path, zstd::stream::encode_all(oversized.as_bytes(), 1).unwrap()).unwrap();
  assert!(
    read_session_header_exact(&path, "dsh-fixture")
      .unwrap_err()
      .contains("size limit")
  );
  assert!(
    read_session_header_exact(&PathBuf::from("missing"), " ")
      .unwrap_err()
      .contains("Invalid expected")
  );
}
