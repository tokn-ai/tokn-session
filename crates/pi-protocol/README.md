# tokn-pi-protocol

`tokn-pi-protocol` provides tolerant wire types for persisted Pi session JSONL.
It is the decoding boundary between Pi-owned session files and consumers in the
`tokn-session` workspace.

The crate is deliberately not a Rust mirror of Pi's complete TypeScript API.
It types stable session fields, keeps extension-owned and fast-changing
subtrees as `serde_json::Value`, and preserves the original JSON for every
session line.

## Install

```toml
[dependencies]
tokn-pi-protocol = "0.1.1"
serde_json = "1"
```

## Why this crate exists

Pi can add session-entry types, message roles, and content blocks independently
of `tokn-session`. A strict enum can make the rest of a valid session unreadable
when any one of those shapes changes. This crate instead:

- decodes known records into useful Rust types
- retains unknown record, message-role, and content-block tags
- falls back to an `UnknownItem` when a known shape cannot be decoded
- serializes `PiSessionLine` back to the unchanged native record

## Usage

Deserialize one JSONL record at a time. The typed view is available through
`item()`, while `native()` retains the complete decoded JSON value.

```rust
use tokn_pi_protocol::{PiSessionItem, PiSessionLine};

let line: PiSessionLine = serde_json::from_str(
  r#"{
    "type": "future_entry",
    "id": "future-1",
    "payload": {"answer": 42}
  }"#,
)?;

match line.item() {
  PiSessionItem::Message(message) => {
    println!("message id: {:?}", message.id);
  }
  PiSessionItem::Unknown(item) => {
    assert_eq!(item.native_type.as_deref(), Some("future_entry"));
    assert_eq!(item.native["payload"]["answer"], 42);
  }
  _ => {}
}

# Ok::<(), serde_json::Error>(())
```

## Supported record families

- session headers
- model and thinking-level changes
- user, assistant, tool-result, and unknown message roles
- text, thinking, tool-call, image, and unknown content blocks
- compaction and branch summaries
- custom entries and custom messages
- labels, session metadata, leaf pointers, and active-tool changes
- error and unknown records

Normalization into the provider-neutral `AgentEvent` IR remains the
responsibility of `tokn-session-pi`.

## Compatibility

This crate follows persisted Pi session files, not Pi's complete TypeScript
API. Pi can add record, message-role, and content-block shapes independently.
Unknown tags and added fields remain inspectable through the typed unknown
values and the original decoded JSON. "Lossless" refers to JSON structure; it
does not preserve source whitespace, duplicate object keys, or original number
spelling.
To retain the complete envelope, serialize `PiSessionLine` or use `native()`;
serializing a nested typed item alone is not an envelope round trip.

## Maintenance

Use `vendor/pi` and representative historical session files as the schema
sources. Add stable fields when consumers need them, keep provider- or
extension-specific payloads as JSON, and add regression tests whenever a new
shape is observed.

## License

Licensed under the [MIT License](LICENSE).

## Repository

<https://github.com/tokn-ai/tokn-session>
