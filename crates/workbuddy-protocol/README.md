# tokn-workbuddy-protocol

Tolerant Rust wire types for persisted WorkBuddy session JSONL.

```toml
[dependencies]
tokn-workbuddy-protocol = "0.1.1"
serde_json = "1"
```

Deserialize each logical record independently. Typed session values retain
unknown variants and additional native fields, allowing consumers to inspect
new provider shapes without discarding the original decoded JSON.

The preservation guarantee concerns JSON structure, rather than whitespace,
duplicate object keys, or the original spelling of numbers. SQLite catalog
access, file discovery, and conversion to `AgentEvent` belong to
`tokn-session-workbuddy`.
