# Pi RPC protocol

Native Pi headless JSONL requests, responses, notifications, session state, and
extension dialogs. Unknown or malformed inbound records retain their native
JSON. This crate does not launch processes, read historical sessions, or define
provider-independent interaction policy.

The source schema is the pinned `vendor/pi` RPC implementation. Native wire
field spelling is preserved; compact application metadata belongs in an adapter.
`parse_model`, `parse_model_catalog`, and `State::parsed_model` validate native
model identities and image support before adapter mapping. `ExtensionDialog::response`
checks answer types, sizes, and selections against the native request.
