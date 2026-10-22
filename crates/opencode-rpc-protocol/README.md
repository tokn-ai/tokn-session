# tokn-opencode-rpc-protocol

Rust wire types for OpenCode's native V2 HTTP API and SSE events, audited against
OpenCode 1.18.8 (`017a5977d2107092007623e507fc5c6eb337d3b2`). This is separate
from `tokn-opencode-protocol`, which decodes persisted V1 history and run JSONL.

The protocol preserves added event fields and unknown native events. Prompt
admission is durable, uses a caller-selected message ID, and distinguishes
native `steer` and `queue` delivery. An admission is not a completion.

The bounded SSE decoder supports fragmented UTF-8, CRLF, multiline data,
comments, and final unterminated events. Transport and normalization belong in
`tokn-opencode-adapter`.

Licensed under the [MIT License](LICENSE).
