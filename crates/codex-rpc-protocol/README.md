# tokn-codex-rpc-protocol

Native Codex app-server wire contracts, separate from the persisted rollout
format in `tokn-codex-protocol`. Based on upstream source
`2351d9e1b608e6f9d9a3699b71d7eb39ee41cfa4` and the generated experimental schema
from installed `codex-cli 0.163.0-alpha.11`.

`RpcMessage` classifies requests, successful responses, errors and notifications
while retaining the original JSON. String and integer IDs remain distinct.
Malformed or ambiguous envelopes stay `Unknown`; new methods and native
payload fields remain visible. Native app-server omits `jsonrpc: "2.0"`.

Typed projections cover initialization, paginated model discovery, thread
start/resume/read, turn start/steer/interrupt, user questions, and command/file
approvals. Volatile status, item, and approval-policy shapes remain native JSON.
Unknown fields are flattened into `extra`. The `UserInput` helpers use native
`text_elements` for text and `url` for inline images.

`turn/start` performs native start-or-steer admission, but its response does not
identify which occurred. It is not strict idle-only start. Model overrides can
update future thread settings even when the input steers a running turn; clients
must not advertise an atomic idle-only model change based on this method.
`turn/steer` requires `expectedTurnId` and fails if the active turn differs.

Callback-based blocking and nonblocking questions arrive as server requests named
`item/tool/requestUserInput`. `isBlocking` distinguishes them and defaults to
true for older peers. Replies map native question IDs to answer arrays and use
the original server request ID. The separate `request_user_input_async` tool
emits an assistant item with question metadata and no server callback; it stays
native item data. Approvals preserve `availableDecisions`, including
policy-amendment objects; parsing them does not grant any permission. Omitted
legacy model modalities use Codex's text-and-image default; explicit modality
lists retain their advertised limits.

This crate performs no I/O, starts no process, and does not attach or resume a
thread. Transports and runtime ownership belong to `tokn-codex-adapter`.
