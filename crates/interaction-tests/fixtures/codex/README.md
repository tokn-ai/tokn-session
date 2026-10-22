# Codex native RPC capture

`app-server.json` records real bidirectional JSONL from Codex's native
`app-server`, exercised by `tests/codex.rs` through the Rust adapter. Inference
uses the Rust loopback Responses server; agent execution and tool dispatch are
native. The approval executes only `printf fixture-approved` inside a disposable
workspace. The run uses a cleared environment and isolated home/config/session
storage, without provider keys or personal sessions.

The capture identifies the native version and executable SHA-256. It was made
with the macOS arm64 Desktop-bundled `codex-cli 0.163.0-alpha.11`, executable hash
`8a6dc3938a2f5d30cc19c50b9ccad83a5aa2ca4a2e3411665430bb78a917220a`.
`inspected_protocol_revision` refers to the separately vendored upstream source
used to interpret the protocol, not a verified source revision of that binary.

The fixture covers initialization, explicit thread creation, metadata/state and
native model catalog reads, assistant output, duplicate-send receipts, selected
and free-text question answers, command approval, explicit `turn/steer`, busy
`turn/start`, interruption, and explicit resume. Strict idle-only Start and Queue
reject without native mutations. Model-request assertions verify that answers,
approved command output, and both steer inputs reach subsequent inference.
The native question tool requires non-empty options for every question; custom
answers use Other, and notes accompanying a selection use `user_note: ...`.

Temporary paths, loopback model address, and hostname are normalized. Native
IDs, timing, extension fields, and correlation remain recorded. Replay validates
every outgoing request, including JSON-RPC responses to server requests, and
releases incoming frames only after preceding outgoing frames have matched.

To verify or deliberately regenerate:

```sh
TOKN_CODEX_RPC_EXECUTABLE=/absolute/path/to/codex scripts/test-real-rpc.sh codex
TOKN_CODEX_RPC_EXECUTABLE=/absolute/path/to/codex scripts/test-real-rpc.sh codex --capture
cargo test -p tokn-interaction-tests --test codex
```
