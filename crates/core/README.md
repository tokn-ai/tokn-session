# tokn-session-core

Provider-neutral types for agent session readers and viewers. `AgentEvent`
represents messages, reasoning, tools, usage, compaction, lifecycle, metadata,
and unknown provider records. `SessionHeader`, `SessionRef`, and `LoadedSession`
describe discovery results and normalized histories.

```toml
[dependencies]
tokn-session-core = "0.1.1"
```

The crate also assembles correlated tool and compaction observations into
operations. Provider-native details remain available for inspection; unknown
records are preserved so new provider shapes can be discovered. Filesystem
readers and provider dispatch live in the other `tokn-session` crates.
