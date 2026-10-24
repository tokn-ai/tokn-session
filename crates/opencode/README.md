# tokn-session-opencode

OpenCode session discovery and normalization using read-only SQLite access.

```toml
[dependencies]
tokn-session-opencode = "0.1.1"
```

`OpenCodeSessionSource` lists session headers and loads V1 message/part histories
and V2 `session_message` projections in native sequence order.
Its scan and cache APIs support incremental catalog and viewer reads.
`OpenCodeLiveNormalizer` handles JSONL emitted by `opencode run --format json`.

The adapter decodes payloads with `tokn-opencode-protocol`, correlates tools,
and produces `tokn_session_core::AgentEvent` while preserving unknown native
records. ZCode-specific storage and semantics are exposed through
`tokn-session-zcode`.

`history_format_exact` identifies V1, V2, mixed, and empty sessions from their
actual conversation rows. Shared session metadata and the existence of a V2
table do not prove V2 ownership; empty native creations need the host's durable
creation receipt before control. Mixed histories reject rather than silently
continuing or merging incompatible contexts. `session_header_exact` reads one
native ID at the configured database without scanning other histories.
