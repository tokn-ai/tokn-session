# tokn-session-zcode

Read-only discovery and normalization for Z.ai ZCode sessions.

```toml
[dependencies]
tokn-session-zcode = "0.1.1"
```

`ZCodeSessionSource` exposes discovery and loaded histories. Use
`tokn-session-client` for provider-independent dispatch.

ZCode persists its agent history in an extended OpenCode-compatible SQLite
schema. This crate shares the tolerant V1 message and part decoder while
retaining ZCode as a distinct provider. ZCode-specific envelope fields remain
available through normalized provenance or unknown native events.

Storage resolves from `--session-dir`, then `$ZCODE_STORAGE_DIR`, then
`~/.zcode/cli/db/db.sqlite`. Explicit paths may name the database, its `db`
directory, or the ZCode storage root.
