# tokn-session-codex

Read-only discovery and normalization of persisted Codex rollout JSONL and
Zstandard-compressed `.jsonl.zst` files into
`tokn_session_core::AgentEvent`.

```toml
[dependencies]
tokn-session-codex = "0.1.1"
```

`CodexSessionSource` lists session headers, loads histories, and resolves child
relationships. `CodexHistoryReader` supports bounded paginated history and
incremental updates. The adapter preserves native unknown records and correlates
tool outputs, questions, usage, and compaction observations.

History cutoffs address decoded JSONL bytes. `CodexHistoryLimits` separates
individual records (128 MiB by default), materialized snapshot or append batches
(128 MiB), temporary disk space (a shared 512 MiB pool), and work duration
(120 seconds per poll). Source file length is not a streaming limit.

`CodexHistoryProjectionReader` folds the full verified history one record at a
time without retaining its transcript or using temporary disk. Its callback
must update staging state and publish only after a successful poll. Backward
window discovery decodes compressed retained prefixes into anonymous temporary
files, reserving shared disk space before writes and releasing it when files
close. Plain rollouts retain incremental append reads, while changed compressed
files rebuild atomically. Header guards retain fingerprints rather than bodies.

Wire decoding is provided by `tokn-codex-protocol`; this crate owns filesystem
discovery, catalog enrichment, history boundaries, and normalized semantics.
For provider-independent access, use `tokn-session-client`.
