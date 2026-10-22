# tokn-dsh-adapter

Rust `InteractionAdapter` for DeepSeek Harness ApiProxy endpoints.
`DshAdapter::attach(ApiProxyConfig::new(endpoint)).await` opens native mux and
host WebSocket downlinks and verifies `host.describe` over HTTP. Ordinary network
GETs to these downlinks return 426; upstream's SSE codec is only an in-process
carrier. Closing or dropping an attached adapter only detaches; it never posts
shutdown, cancels an agent, or kills the external host.

`DshAdapter::launch(ManagedOptions::new(cwd)).await` owns a fresh native
`dsh --profile web --host 127.0.0.1 --port 0` child. Configure `program`, optional
launcher-prefix arguments, and inherited-environment overrides to select the
installed runtime/profile environment. No shell is involved. Readiness accepts
only this child's native loopback URL line, which upstream emits after its
Loader/API settles, followed by the same downlink/HTTP handshake. The OS selects
a fresh port; an existing listener is never adopted as managed. Startup failure
kills this child and closes its bounded output drains. Managed close sends
SIGTERM on Unix so native disposal can finish, then escalates after the configured
grace period and waits for exit. Other platforms kill/reap the owned child.
Dropping the adapter requests the same bounded owned-process cleanup.

Metadata uses `host.describe`, state uses `session.list`, and models use the
session-independent `llm.models` directory. These queries never call
`session.models`, which can resume cold native Agents. Session models are known
only after live request-header/context observations; host defaults are not
presented as a session's current model. Catalog image capability is unknown and
therefore conservatively false. `model_catalog` exposes the native directory,
including per-provider failures and unknown fields.

`StartOrSteer` sends native `session.prompt` with `mode: steer`; `Queue` selects
native `queue`. The response confirms admission, without guessing whether it
started or steered a turn. Strict `Start` and strict `Steer` reject because the
native endpoint cannot atomically enforce their runtime-state preconditions.
Model intent on Send also rejects: native `selectModel` is a separate mutation
that can affect an active turn and deployment defaults. No model setter is
called implicitly. Native cancellation preserves pending inbox messages, so
`CancelTurn` does not mean stopping a worker or clearing its queue.

Questions and one-shot permissions come from native pending-request frames.
Replies preserve the stable native RPC id, session id, and permission audit id.
Question batches preserve selected labels and separate custom text, validate
single/multiple choice, and retain native presentation metadata. Permanent
permission grants are unsupported. Pending snapshots contain the latest observed
live requests; the upstream stream has no terminal baseline marker. Stable replay
does not duplicate them, and resolution frames retire the matching request.

Unary bodies, WebSocket frames, live-session state and pending interactions are bounded;
connect and unary operations have timeouts. Requests are never retried. Transport
or malformed-response failures after a mutation are uncertain, rather than claims
that input was not sent. Use the interaction-core broker for request receipts.

WebSocket control pings check liveness on each downlink, with bounded send/pong
deadlines. They send no application messages. Failure of either downlink stops
its peer and invalidates the entire attachment. State reads and mutations then
fail; non-activating metadata/catalog reads remain available. Event sequence
allocation and publication are serialized across downlinks and response tasks.

The upstream endpoint has no independently negotiated instance identity or event
cursor replay. Event loss requires an explicit fresh attachment, which gets a
new adapter instance id. Native pending interactions
replay on attachment; conversation recovery requires a fresh history baseline.
The adapter does not silently resume, reattach, or replay a prompt.

The SDK stdio protocol is described by `tokn-dsh-rpc-protocol::sdk` but is too small
for this interactive contract. Managed mode launches the richer native Web
profile; it does not invent SDK methods. Fake HTTP/WebSocket tests and pure
readiness-parser tests launch no DSH process and make no provider requests.
