# tokn-session-codex

Read-only discovery and normalization of persisted Codex rollout JSONL into
`tokn_session_core::AgentEvent`.

```toml
[dependencies]
tokn-session-codex = "0.1.1"
```

`CodexSessionSource` lists session headers, loads histories, and resolves child
relationships. `CodexHistoryReader` supports bounded paginated history and
incremental updates. The adapter preserves native unknown records and correlates
tool outputs, questions, usage, and compaction observations.

Wire decoding is provided by `tokn-codex-protocol`; this crate owns filesystem
discovery, catalog enrichment, history boundaries, and normalized semantics.
For provider-independent access, use `tokn-session-client`.
