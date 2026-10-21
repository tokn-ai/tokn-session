use std::{collections::HashMap, fs, path::Path};

use crate::project_aliases::{Alias, Provenance};

/// Only a Git marker in this exact folder can establish a repository identity.
/// A non-Git folder is a project even when an ancestor is a checkout.
pub(crate) fn direct_repository(cwd: &str) -> Option<Alias> {
  let directory = Path::new(cwd);
  if !directory.is_absolute() {
    return None;
  }
  let dot_git = directory.join(".git");
  let git_dir = if dot_git.is_dir() {
    dot_git
  } else {
    let contents = fs::read_to_string(&dot_git).ok()?;
    directory.join(contents.trim().strip_prefix("gitdir:")?.trim())
  };
  if !git_dir.is_dir() {
    return None;
  }
  let (root, provenance) = if let Ok(common) = fs::read_to_string(git_dir.join("commondir")) {
    let common = git_dir.join(common.trim()).canonicalize().ok()?;
    let root = if common.file_name().is_some_and(|name| name == ".git") {
      common.parent()?.to_path_buf()
    } else {
      common
    };
    (root, Provenance::Worktree)
  } else {
    // Separate Git directories and submodules still belong to this checkout;
    // their administrative directory is not the project's working folder.
    (directory.canonicalize().ok()?, Provenance::DirectGit)
  };
  Some(Alias {
    project_key: root.to_string_lossy().into_owned(),
    provenance,
  })
}

/// Distinguish an ordinary existing folder from a removed or unreadable
/// checkout. A broken/present marker is not evidence to discard its identity.
pub(crate) fn marker_absent_in_existing_folder(folder: &Path) -> bool {
  folder.is_absolute()
    && matches!(fs::symlink_metadata(folder.join(".git")), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    && fs::metadata(folder).is_ok_and(|metadata| metadata.is_dir())
}

/// Registered worktrees can still identify historical sessions after their
/// checkout directory disappears. Never merge unrelated repositories by basename.
pub(crate) fn registered_worktrees(root: &Path) -> HashMap<String, Alias> {
  let mut aliases = HashMap::new();
  if !root.is_absolute() {
    return aliases;
  }
  // A bare/nonstandard common Git directory is itself the group key.
  let mut directories = vec![root.join(".git/worktrees")];
  if root.join("HEAD").is_file() && root.join("objects").is_dir() {
    directories.push(root.join("worktrees"));
  }
  for registrations in directories {
    let Ok(entries) = fs::read_dir(registrations) else {
      continue;
    };
    for entry in entries.flatten() {
      let Ok(gitfile) = fs::read_to_string(entry.path().join("gitdir")) else {
        continue;
      };
      if let Some(checkout) = Path::new(gitfile.trim()).parent().filter(|path| path.is_absolute()) {
        if marker_absent_in_existing_folder(checkout) {
          continue;
        }
        aliases.insert(
          checkout.to_string_lossy().into_owned(),
          Alias {
            project_key: root.to_string_lossy().into_owned(),
            provenance: Provenance::Worktree,
          },
        );
      }
    }
  }
  aliases
}
