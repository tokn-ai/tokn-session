# tokn-opencode-protocol

`tokn-opencode-protocol` provides tolerant wire types for the OpenCode formats
consumed by `tokn-session`:

- `v1`: message and part JSON payloads persisted in SQLite
- `run`: JSONL envelopes written by `opencode run --format json`

The crate deliberately does not contain SQLite queries, row identity, session
assembly, or normalization into `AgentEvent`. Those remain responsibilities of
`tokn-session-opencode`.

## Install

```toml
[dependencies]
tokn-opencode-protocol = "0.1.1"
serde_json = "1"
```

## Design

- Decode a native JSON value before selecting a typed view.
- Preserve unknown message roles, part types, tool-state statuses, and run
  event types.
- Fall back to an inspectable `UnknownItem` when a known tag has a malformed
  shape.
- Preserve added fields on known records.
- Serialize top-level wrappers back to the unchanged `serde_json::Value`.

OpenCode stores message and part identifiers in relational columns and omits
them from the corresponding JSON `data` columns. The V1 part types therefore
accept optional `id`, `sessionID`, and `messageID` fields, allowing the same
types to decode both persisted payloads and hydrated parts embedded in run
events.

The preservation guarantee is structural JSON losslessness. Parsing into
`serde_json::Value` does not preserve source whitespace, duplicate object keys,
or the original textual spelling of numbers.

## Example

```rust
use tokn_opencode_protocol::v1::{PartData, PartItem};

let part: PartData = serde_json::from_str(
  r#"{"type":"future-part","answer":42}"#,
)?;

match part.item() {
  PartItem::Unknown(item) => {
    assert_eq!(item.native_type.as_deref(), Some("future-part"));
    assert_eq!(item.native["answer"], 42);
  }
  _ => unreachable!(),
}

# Ok::<(), serde_json::Error>(())
```

To parse JSONL emitted by `opencode run --format json`, use the `run` module:

```rust
use tokn_opencode_protocol::run::{RunEvent, RunLine};

let line: RunLine = serde_json::from_str(
  r#"{
    "type": "text",
    "sessionID": "ses_example",
    "timestamp": 1710000000000,
    "part": {"type": "text", "text": "done"}
  }"#,
)?;

let RunEvent::Text(part) = line.event() else {
  unreachable!();
};
assert_eq!(part.text, "done");

# Ok::<(), serde_json::Error>(())
```

## Compatibility

This crate follows persisted OpenCode data and `opencode run` output, not a
stable upstream API. OpenCode can add payload shapes independently. Unknown
tags and added fields remain inspectable through typed unknown values and the
original decoded JSON. "Lossless" refers to JSON structure; it does not
preserve source whitespace, duplicate object keys, or original number spelling.
To retain a complete envelope, serialize `MessageData`, `PartData`, or
`RunLine`, or use `native()`; serializing a nested typed item alone is not an
envelope round trip.

## License

Licensed under the [MIT License](LICENSE).

## Repository

<https://github.com/tokn-ai/tokn-session>
