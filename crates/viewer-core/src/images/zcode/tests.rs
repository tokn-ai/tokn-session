use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};

const OWNER: &str = "sess_fixture";
const REFERENCE: &str = "zcode-artifact://sess_fixture/tool-result-image";

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, Vec<u8>) {
  let directory = tempfile::tempdir().unwrap();
  let cli = directory.path().join("storage/cli");
  std::fs::create_dir_all(cli.join("db")).unwrap();
  let database = cli.join("db/db.sqlite");
  std::fs::write(&database, []).unwrap();
  let artifacts = cli.join("artifacts").join(OWNER);
  std::fs::create_dir_all(&artifacts).unwrap();
  let mut bytes = std::io::Cursor::new(Vec::new());
  image::DynamicImage::new_rgb8(2, 1)
    .write_to(&mut bytes, image::ImageFormat::Png)
    .unwrap();
  let bytes = bytes.into_inner();
  std::fs::write(
    artifacts.join("prompt-upload-tool-result-image.txt"),
    format!("data:image/png;base64,{}", STANDARD.encode(&bytes)),
  )
  .unwrap();
  (directory, database, artifacts, bytes)
}

#[test]
fn native_artifact_reads_preserve_opaque_identity_and_validate_image_format() {
  let (_directory, database, artifacts, bytes) = fixture();
  assert_eq!(read_artifact(&database, OWNER, REFERENCE).unwrap(), bytes);
  std::fs::write(
    artifacts.join("prompt-upload-tool-result-image.txt"),
    format!("data:image/jpeg;base64,{}", STANDARD.encode(bytes)),
  )
  .unwrap();
  assert!(
    read_artifact(&database, OWNER, REFERENCE)
      .unwrap_err()
      .contains("format")
  );
  std::fs::write(artifacts.join("prompt-upload-tool-result-image.txt"), [0xff]).unwrap();
  assert!(read_artifact(&database, OWNER, REFERENCE).is_err());
}

#[test]
fn artifact_owners_and_components_are_exact_and_never_decoded_or_sanitized() {
  let (_directory, database, _, _) = fixture();
  for reference in [
    "zcode-artifact://sess_other/tool-result-image",
    "zcode-artifact://SESS_fixture/tool-result-image",
    "ZCODE-artifact://sess_fixture/tool-result-image",
    "zcode-artifact://sess_fixture/../tool-result-image",
    "zcode-artifact://sess_fixture/%2e%2e",
    "zcode-artifact://sess_fixture/tool-result-image?other",
    "zcode-artifact://sess_fixture/tool-result-image#other",
    "zcode-artifact://sess_fixture//tool-result-image",
    "zcode-artifact://sess_fixture/tool-result-image/extra",
    "zcode-artifact://sess_fixture/tool.result.image",
    "zcode-artifact://sess_fixture/图片",
  ] {
    assert!(!matches_reference(reference, OWNER), "{reference}");
    assert!(read_artifact(&database, OWNER, reference).is_err());
  }
  assert!(!matches_reference(REFERENCE, "../sess_fixture"));
  assert!(!matches_reference(
    &format!("zcode-artifact://{OWNER}/{}", "a".repeat(121)),
    OWNER
  ));
  assert!(read_artifact(&database.with_file_name("custom.sqlite"), OWNER, REFERENCE).is_err());
}

#[test]
fn artifact_suffix_matches_must_be_unique_and_scans_are_bounded() {
  let (_directory, database, artifacts, _) = fixture();
  let duplicate = artifacts.join("second-upload-tool-result-image.txt");
  std::fs::write(&duplicate, "unrelated").unwrap();
  assert!(
    read_artifact(&database, OWNER, REFERENCE)
      .unwrap_err()
      .contains("ambiguous")
  );
  std::fs::remove_file(duplicate).unwrap();
  for index in 0..MAX_ENTRIES {
    std::fs::write(artifacts.join(format!("unrelated-{index}.txt")), []).unwrap();
  }
  assert!(
    read_artifact(&database, OWNER, REFERENCE)
      .unwrap_err()
      .contains("directory limit")
  );
}

#[test]
fn artifact_encoded_size_is_checked_before_reading_bytes() {
  let (_directory, database, artifacts, _) = fixture();
  let file = File::options()
    .write(true)
    .open(artifacts.join("prompt-upload-tool-result-image.txt"))
    .unwrap();
  file.set_len(MAX_ENCODED_BYTES as u64 + 1).unwrap();
  assert!(
    read_artifact(&database, OWNER, REFERENCE)
      .unwrap_err()
      .contains("size limit")
  );
}

#[cfg(unix)]
#[test]
fn artifact_files_and_owner_directories_cannot_follow_symlinks() {
  use std::os::unix::fs::symlink;
  let (directory, database, artifacts, _) = fixture();
  let custom_database = directory.path().join("custom.sqlite");
  std::fs::rename(&database, &custom_database).unwrap();
  symlink(&custom_database, &database).unwrap();
  assert!(
    read_artifact(&database, OWNER, REFERENCE)
      .unwrap_err()
      .contains("layout")
  );
  std::fs::remove_file(&database).unwrap();
  std::fs::rename(custom_database, &database).unwrap();
  let original = artifacts.join("prompt-upload-tool-result-image.txt");
  let outside = directory.path().join("outside.txt");
  std::fs::rename(&original, &outside).unwrap();
  symlink(&outside, &original).unwrap();
  assert!(read_artifact(&database, OWNER, REFERENCE).is_err());
  std::fs::remove_file(original).unwrap();
  std::fs::remove_dir(&artifacts).unwrap();
  let outside_directory = directory.path().join("another-session");
  std::fs::create_dir(&outside_directory).unwrap();
  std::fs::rename(&outside, outside_directory.join("prompt-upload-tool-result-image.txt")).unwrap();
  symlink(&outside_directory, &artifacts).unwrap();
  assert!(read_artifact(&database, OWNER, REFERENCE).is_err());
  std::fs::remove_file(artifacts).unwrap();
  let root = database.parent().unwrap().parent().unwrap().join("artifacts");
  std::fs::remove_dir(&root).unwrap();
  symlink(&outside_directory, &root).unwrap();
  assert!(read_artifact(&database, OWNER, REFERENCE).is_err());
}
