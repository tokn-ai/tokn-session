# tokn-session-storage

Shared native paths under `~/.tokn/sessions`, including the unchanged session
index, Hub state, desktop client identities, and viewer configuration.

```toml
[dependencies]
tokn-session-storage = "0.1.1"
```

`SessionPaths::from_home` resolves paths without touching the filesystem.
Directory migration renames complete private bundles and preserves an exact
legacy directory alias on Unix. It refuses active locks, arbitrary symlinks,
conflicting stores, malformed JSON/key files, and unsafe file metadata. Callers must ensure any legacy
processes without ownership locks are stopped before migration.

`migrate_directory_with_alias_and_locks` also acquires named connector ownership
locks that may not exist yet, while preserving their inode through the move.
Defaults and inspection remain read-only until an explicit offline migration.
