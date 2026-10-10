# tokn-session-opencode

OpenCode session discovery and normalization using read-only SQLite access.

```toml
[dependencies]
tokn-session-opencode = "0.1.1"
```

`OpenCodeSessionSource` lists session headers and loads message/part histories.
Its scan and cache APIs support incremental catalog and viewer reads.
`OpenCodeLiveNormalizer` handles JSONL emitted by `opencode run --format json`.

The adapter decodes payloads with `tokn-opencode-protocol`, correlates tools,
and produces `tokn_session_core::AgentEvent` while preserving unknown native
records. ZCode-specific storage and semantics are exposed through
`tokn-session-zcode`.
