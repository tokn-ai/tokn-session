# tokn-codex-protocol

`tokn-codex-protocol` provides tolerant, lossless Rust wire types for persisted
Codex rollout JSONL files.

It is maintained by the `tokn-session` project and models only the file format
that session readers need; it does not mirror Codex's internal Rust API.

## Install

```toml
[dependencies]
tokn-codex-protocol = "0.1.1"
serde_json = "1"
```

## Usage

Deserialize one JSONL record at a time. The typed view is available through
`item()`, while `native()` retains the complete decoded JSON value.

`SessionMetaItem::history_mode()` and `history_base()` validate newer metadata
stored in `extra`. The inherited history position contains the owning thread
ID, exclusive ordinal, and exclusive byte offset. The wire crate preserves
this reference; `tokn-session-codex` resolves it into bounded physical segments.

`RolloutLine::token_usage_record()` exposes typed response accounting and its
turn/thread aggregates. These newer rows also remain `RolloutItem::Unknown`
through `item()`, preserving exhaustive matches written against version 0.1.0.
Malformed accounting remains inspectable with a parse error and no typed view.

## Design

- Preserve the complete native `RolloutLine` JSON value.
- Preserve unknown rollout and response-item tags with their original payload.
- Accept additional fields on known records.
- Type stable fields used by session consumers.
- Keep volatile structures, such as permissions and world state, as
  `serde_json::Value`.
- Accept historical and current wire representations, including string and
  structured custom-tool output.

An unfamiliar record remains inspectable:

```rust
use tokn_codex_protocol::{RolloutItem, RolloutLine};

let line: RolloutLine = serde_json::from_str(
  r#"{
    "timestamp": "2026-07-29T00:00:00Z",
    "type": "future_rollout_item",
    "payload": {"answer": 42}
  }"#,
)?;

match line.item() {
  RolloutItem::Unknown(item) => {
    assert_eq!(item.native_type.as_deref(), Some("future_rollout_item"));
    assert_eq!(item.payload["answer"], 42);
  }
  _ => unreachable!(),
}

# Ok::<(), serde_json::Error>(())
```

Known response items expose typed fields without discarding their extensions:

```rust
use tokn_codex_protocol::{ResponseItem, RolloutItem, RolloutLine};

let line: RolloutLine = serde_json::from_str(
  r#"{
    "type": "response_item",
    "payload": {
      "type": "agent_message",
      "id": "amsg_1",
      "author": "/root",
      "recipient": "/root/reviewer",
      "content": [{"type": "input_text", "text": "Please review this."}]
    }
  }"#,
)?;

let RolloutItem::ResponseItem(ResponseItem::AgentMessage(message)) = line.item()
else {
  unreachable!();
};

assert_eq!(message.author.as_deref(), Some("/root"));
assert_eq!(message.recipient.as_deref(), Some("/root/reviewer"));

# Ok::<(), serde_json::Error>(())
```

## Supported record families

- session metadata
- response items and content
- inter-agent communication and delivery metadata
- compaction records
- turn context
- model-response token usage records with turn and thread aggregates
- world-state snapshots and patches
- event messages
- unknown future records

`event_msg` has a large and independently evolving set of payloads. It is
therefore represented as a lossless tagged value rather than an exhaustive
enum.

## Compatibility

Version 0.1.1 preserves the published 0.1.0 `RolloutItem` variants and
`SessionMetaItem` fields; new typed access is additive.

This crate follows persisted rollout files, not a stable upstream Codex API.
Codex can add fields and record types independently. Unknown tags and added
fields remain available through the typed unknown values and the original
decoded JSON. "Lossless" refers to JSON structure; it does not preserve source
whitespace, duplicate object keys, or original number spelling.
To retain the complete envelope, serialize `RolloutLine` or use `native()`;
serializing a nested typed item alone is not an envelope round trip.

## Non-goals

- Codex app-server request and notification types
- OpenAI Responses API client types
- Codex command or configuration types
- normalization into `tokn_session_core::AgentEvent`
- exact reproduction of Codex's internal implementation

Provider normalization belongs in `tokn-session-codex`. This crate only
decodes the persisted wire format.

## Maintenance

The checkouts under `vendor/` are schema references, not build dependencies.
When a new rollout shape appears:

1. Add a minimal, non-sensitive fixture or focused test.
2. Type fields that are stable and useful to consumers.
3. Keep unstable nested data as JSON.
4. Preserve unknown tags and fields instead of using a unit
   `#[serde(other)]` fallback.

Future provider-specific protocol crates can follow the same narrow,
lossless-decoding approach without introducing a shared lowest common
denominator wire format.

## License

Licensed under the [MIT License](LICENSE).

## Repository

<https://github.com/tokn-ai/tokn-session>
