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

History cutoffs address decoded JSONL bytes. Compressed reads use anonymous
temporary files for backward seeking, with a 128 MiB decoded limit per segment;
inherited segments decode only their retained prefix. Plain rollouts retain
incremental append reads, while changed compressed files rebuild atomically.

Wire decoding is provided by `tokn-codex-protocol`; this crate owns filesystem
discovery, catalog enrichment, history boundaries, and normalized semantics.
For provider-independent access, use `tokn-session-client`.
