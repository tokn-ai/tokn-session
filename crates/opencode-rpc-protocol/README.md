# tokn-opencode-rpc-protocol

Rust wire types for OpenCode's HTTP API and SSE events, including the current
2.3.0 inbox/form contracts at `5756d59a68a3d16ce412280c3ab055031898cce2`
and earlier V2 at `017a5977d2107092007623e507fc5c6eb337d3b2` (1.18.8).
The `current` module keeps flattened prompt input, inbox admissions and forms
separate from the earlier nested prompt/question shapes. This is separate
from `tokn-opencode-protocol`, which decodes persisted V1 history and run JSONL.

The protocol preserves added event fields and unknown native events. Prompt
admission is durable, uses a caller-selected message ID, and distinguishes
native `steer` and `queue` delivery. An admission is not a completion.

The bounded SSE decoder supports fragmented UTF-8, CRLF, multiline data,
comments, and final unterminated events. Transport and normalization belong in
`tokn-opencode-adapter`.

Licensed under the [MIT License](LICENSE).
