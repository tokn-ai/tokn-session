# Native OpenCode RPC capture

`native-v2.json` records the real OpenCode source at
`017a5977d2107092007623e507fc5c6eb337d3b2` (package version 1.18.8), launched
through its unchanged Bun CLI. Only model responses came from the deterministic
loopback OpenAI-compatible server; the native HTTP/SSE handlers, agent loop,
question tool, permissions, and Bash tool were real.

The scenario exercises session creation, metadata/models, text streaming,
question/selected reply, permission/allow-once with `printf rpc-local-only`,
stream-held steering and queued messages, and cancellation. It also checks native
live question/permission events. The source build reports its version as `local`.

Capture with `TOKN_OPENCODE_RPC_ROOT` from
`scripts/rpc/prepare-opencode.sh`, then run the ignored
`real_opencode_rpc_local_model_capture` test with `TOKN_CAPTURE_RPC_FIXTURES=1`.
The ordinary `replay_captured_native_opencode_v2` test uses the same adapter and
scenario against these exchanges without a native runtime or model server.

Sanitization replaces temporary project/home/model-server paths and credentials,
including URL-encoded and project-relative paths. Native correlation IDs,
message order, event payloads, response schemas, and recorded timestamps remain.

OpenCode's V2 configuration is not accepted by its legacy history endpoint. The
adapter conservatively calls that endpoint before controlling an existing
conversation, so this scenario uses supported legacy configuration that V2
migrates. A separately captured native rejection is checked in
`crates/opencode-adapter/tests/fixtures/native-legacy-config-rejection.json`.
The adapter reports that limitation explicitly and never treats the failed check
as proof of empty history.
