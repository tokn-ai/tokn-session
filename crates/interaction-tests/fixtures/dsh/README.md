# DSH native ApiProxy fixture

`interaction.json` is captured from the real DeepSeek Harness web profile at
revision `99f6f02fecdb7dff40c3fbc9470f5907c29f74ca` (0.1.0-rc.7), using the
repository's built CLI, ApiProxy, agent loop, tools, and pi-ai model adapter.
The model is a deterministic OpenAI-compatible server listening on loopback;
no hosted inference service, account, or real API key participates.

The scenario queries metadata/models and creates four real sessions. It checks
an assistant reply, native `ask_user_question` with a multi-select option and
free-text answer, steering at a live step boundary, a queued next turn, and
cancellation of a model stream that remains open. Native RPC requests/replies
and both WebSocket downlinks are captured. Temporary home/project/install paths
and loopback endpoint addresses are replaced; native correlation IDs, event
sequence numbers, unknown fields, and payload structure are preserved.

The ordinary Rust test replays this traffic through the real DSH adapter. The
ignored capture test starts the actual pinned process with cleared environment,
an isolated `HOME`/`DSH_HOME`, a dummy test key, telemetry disabled, and only the
local model route. The title model and shipped DeepSeek route are disabled so
no incidental model request can reach a hosted provider.

Prepare a disposable built source snapshot with `scripts/rpc/prepare-dsh.sh`.
Set `TOKN_DSH_RPC_ROOT` to that snapshot and `TOKN_RPC_NODE` to an absolute
supported Node executable, then run the ignored DSH capture test. Add
`TOKN_CAPTURE_RPC_FIXTURES=1` only when intentionally updating the fixture.
