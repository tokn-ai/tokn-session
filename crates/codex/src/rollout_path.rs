use std::path::Path;

pub(crate) fn rollout_thread_id(path: &Path) -> Option<&str> {
  rollout_parts(path).map(|(owner, _)| owner)
}

/// Continuation filenames carry a physical segment UUID after the owner UUID.
/// Require the filename owner to agree with metadata before trusting that alias.
pub(crate) fn rollout_segment_matches(path: &Path, owner: Option<&str>, segment: &str) -> bool {
  let Some((filename_owner, suffix)) = rollout_parts(path) else {
    return false;
  };
  owner == Some(filename_owner) && suffix.strip_prefix('_').is_some_and(|id| is_uuid(id) && id == segment)
}

fn rollout_parts(path: &Path) -> Option<(&str, &str)> {
  let name = path.file_name()?.to_str()?;
  let stem = name
    .strip_suffix(".jsonl.zst")
    .or_else(|| name.strip_suffix(".jsonl"))?
    .strip_prefix("rollout-")?;
  // Native names contain a 19-byte timestamp, '-', then the owning UUID.
  // New segments add a second UUID separated by '_'.
  let rest = stem.get(20..)?;
  let owner = rest.get(..36)?;
  let suffix = rest.get(36..)?;
  (is_uuid(owner) && (suffix.is_empty() || suffix.starts_with('_'))).then_some((owner, suffix))
}

fn is_uuid(value: &str) -> bool {
  value.len() == 36
    && value.bytes().enumerate().all(|(index, byte)| {
      if matches!(index, 8 | 13 | 18 | 23) {
        byte == b'-'
      } else {
        byte.is_ascii_hexdigit()
      }
    })
}

#[cfg(test)]
mod tests {
  use super::*;

  const OWNER: &str = "01a0c8ab-3b96-7c40-adae-51c20a43c2cb";
  const SEGMENT: &str = "01a0c923-943a-7820-b93f-15e26e7e7cd4";

  #[test]
  fn plain_and_compressed_names_retain_the_same_owner_and_segment_identity() {
    for extension in ["jsonl", "jsonl.zst"] {
      let original = format!("rollout-2026-09-22T20-42-27-{OWNER}.{extension}");
      assert_eq!(rollout_thread_id(Path::new(&original)), Some(OWNER));
      let continuation = format!("rollout-2026-09-22T20-42-27-{OWNER}_{SEGMENT}.{extension}");
      let path = Path::new(&continuation);
      assert_eq!(rollout_thread_id(path), Some(OWNER));
      assert!(rollout_segment_matches(path, Some(OWNER), SEGMENT));
      assert!(!rollout_segment_matches(path, Some(SEGMENT), SEGMENT));
      assert!(!rollout_segment_matches(path, Some(OWNER), OWNER));
    }
  }

  #[test]
  fn unrelated_extensions_do_not_create_native_rollout_aliases() {
    for extension in ["zst", "jsonl.zst.backup", "txt"] {
      let name = format!("rollout-2026-09-22T20-42-27-{OWNER}_{SEGMENT}.{extension}");
      assert_eq!(rollout_thread_id(Path::new(&name)), None);
      assert!(!rollout_segment_matches(Path::new(&name), Some(OWNER), SEGMENT));
    }
  }
}
