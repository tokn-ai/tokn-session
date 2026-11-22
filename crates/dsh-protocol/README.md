# tokn-dsh-protocol

`tokn-dsh-protocol` provides tolerant, lossless Rust wire types for persisted
DeepSeek Harness (DSH) session logs.

It models decoded logical records. Reading Zstandard-compressed JSONL,
discovering session files, and loading the SQLite backend belong in the DSH
session provider rather than this protocol crate.

```toml
[dependencies]
tokn-dsh-protocol = "0.1.1"
serde_json = "1"
```

## Usage

Deserialize one logical JSONL record at a time. `item()` exposes a typed view,
while `native()` retains the complete decoded JSON value.

```rust
use tokn_dsh_protocol::{DshSessionItem, DshSessionLine};

let line: DshSessionLine = serde_json::from_str(
  r#"{"type":"turn/start","seq":0,"time":1,"data":{"turn":1}}"#,
)?;

assert!(matches!(line.item(), DshSessionItem::Event(_)));

# Ok::<(), serde_json::Error>(())
```

## Design

- Preserve every complete native JSON record independently of its typed view.
- Type the core session events used by consumers.
- Preserve plugin-defined and future event tags as unknown records.
- Retain malformed known records with a parse diagnostic instead of rejecting
  the whole stream.
- Model packed `text-chunks`, `reasoning-chunks`, and `tool-call-chunks` rows.
- Preserve unknown message sources, content blocks, stream chunks, and reason
  variants.

The current pinned DSH format is version `4`; older version 0–3 records retain
their native shapes. Header types cover both the older `seedLength` and newer
`isSeeded` forms. Current tool-role results, system/developer messages, compact
assistant attempts and canonical surface coordinates are modeled without
rewriting the stored representation. The event vocabulary is merge-extensible
through plugins. Consumers performing authoritative reconstruction must apply DSH's
`ignorable` policy; this crate only decodes and preserves the wire data.

"Lossless" refers to JSON structure. It does not preserve source whitespace,
duplicate object keys, or original number spelling. Serialize `DshSessionLine`
or use `native()` for a complete record round trip.

## Non-goals

- Zstandard decompression or JSONL framing
- SQLite persistence access
- session discovery
- normalization into `tokn_session_core::AgentEvent`
- mirroring DSH's internal TypeScript APIs

## License

Licensed under the [MIT License](LICENSE).
