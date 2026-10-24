# Live interaction

The live interaction layer is Rust-owned and separate from historical session
reading. Provider-native protocols flow through adapters into common contracts:

```text
Pi JSONL ─────────── pi-rpc-protocol ───────── pi-adapter ───────┐
Codex app-server ── codex-rpc-protocol ────── codex-adapter ────┤
OpenCode V2 HTTP/SSE ─ opencode-rpc-protocol ─ opencode-adapter ─┼─ InteractionService ─ host API/UI
DSH HTTP/WebSocket ─ dsh-rpc-protocol ──────── dsh-adapter ─────┘
```

Dependencies point inward: the core defines `InteractionAdapter`, commands,
replies, capabilities, model identities, and normalized runtime events. The
protocol crates contain native wire types without core or viewer dependencies.
Each adapter depends on its protocol and the core contract. The host composes
them with `InteractionService::register(Arc<dyn InteractionAdapter>)`.
Historical `*-protocol` crates and `AgentEvent` keep their existing role.

Codex uses managed stdio or explicit attached stdio/WS(S). Create/resume binds
and subscribes a thread before control; metadata/state inspection never resumes
it. Native model catalogs provide verified choices for these registered
app-server runtimes. The viewer defaults to RPC and offers its existing Desktop
route as an explicit per-thread alternative. See the
[Codex adapter](../crates/codex-adapter/README.md) for native guarantees, callback
questions, transport limits, and ownership.

## Identity and ownership

A runtime has a host-chosen `runtime_id`, a fresh connection/process
`instance_id`, a provider, and `Managed` or `Attached` ownership. Commands bind
both identities and an optional native session ID; mutations require an explicit
session ID. Multiple sessions can share one Codex/OpenCode/DSH runtime. Never identify
a runtime solely by a saved history path, PID, or listening port.

Pi `spawn` owns a headless `--mode rpc` child; `attach` accepts IO handles
deliberately supplied by their transport owner. Pi has no endpoint discovery or
way to recover another process's pipes. OpenCode `launch` owns a loopback server
with a fresh Basic credential and requires that child’s native readiness URL
before connecting; `connect` uses an explicit endpoint and native directory/workspace. DSH `launch` owns a Web-profile child with an OS-assigned
loopback port, obtains readiness from that child's native output, and verifies
the actual HTTP/WebSocket API. `attach` uses a configured ApiProxy endpoint.
DSH's SDK stdio protocol is separately described but cannot satisfy the richer
interaction contract. Endpoint credentials and executable/launch configuration
remain host-owned profiles. The viewer's separately authorized runtime-settings
commands may update them; ordinary session commands carry only verified runtime
and session identities. See [viewer RPC runtimes](viewer-rpc-runtimes.md).

Snapshot inspection does not call a provider, start a process, or resume saved
work. Initial snapshots require recovery and do not claim a live connection until
a state/pending baseline or corresponding live event confirms one. Metadata,
state, and model reads also avoid activating native sessions. Closing
an attached adapter detaches only its transport. Managed close stops and reaps
only its actual child. Host shutdown must await `InteractionService::shutdown`; cleanup survives a
disconnected caller and later callers await the same result. Detach reserves the runtime ID until cleanup completes. A failed/uncertain close
keeps that binding reserved until shutdown, preventing accidental replacement.

## Admission and responses

Commands cover metadata, current state, model catalogs, messages, cancellation,
pending questions/permissions/dialogs, and typed replies. Consult capabilities
instead of treating all providers as equivalent. `StartOrSteer` uses native
admission to start when idle or steer when running. `Queue` uses native follow-up
delivery. Strict `Start` and `Steer` require native atomic state enforcement;
unsupported modes reject rather than relying on a racing status preflight.
An `Accepted` admission is distinct from started, steered, or completed work.
Cancellation ends native turn work and may preserve queued inputs.

`MessageInput.model` is reserved for an adapter that can atomically apply it only
when starting, ignoring it during steering. The four general adapters
advertise this as unsupported: their native APIs cannot guarantee applying it
only on idle admission. Model catalogs still provide real metadata.
The viewer's existing Pi bridge can retain its stronger native admission path.

The core permits one awaiting message admission per session while allowing
cancellation and question replies to proceed independently. A request receipt binds
the runtime instance, native session, and exact command. Client cancellation
does not cancel admitted work. Pending outcomes cannot be evicted; uncertain
outcomes are retained until host exit, and capacity pressure rejects new writes
instead of replaying them. Definite completions use bounded recent retention.
These are process-local receipts, not persistent exactly-once delivery. A caller
must reconcile after host restart or changing its request ID.

Pending interactions come only from the live adapter. Historical question cards
cannot authorize an answer. Replies bind the exact interaction and session,
validate complete question batches, preserve selected labels separately from
custom text, and claim the interaction while admission is unresolved. Multiple
concurrent dialogs are supported. Pi has no dialog-response acknowledgement or
pending-query RPC: its successful acknowledgement confirms the transport write,
and its pending snapshot is a bounded connector cache with conservative expiry.
Keep that connector alive across viewer-client reconnects. DSH pending replay
has no terminal baseline marker, so snapshots mean latest observed live state.

## Events and recovery

Adapters preserve native message/tool/unknown events alongside normalized state
and interaction events. Runtime publication sequences are local ordering, not
native durable history cursors. The core filters other instances/sessions,
invalidates proof after gaps or lag, and prevents a delayed query baseline from
overwriting newer events. State and pending-request baselines restore recovery
coverage for previously observed sessions. Recovery scopes are bounded; overflow
sets `reattach_required` and blocks further mutations until a fresh binding. Inspectors and history readers remain
separate from this live channel; token payloads need not be forwarded to every
frontend.

OpenCode and DSH do not negotiate an attached server-instance nonce. A lost
attached stream therefore requires an explicit fresh attachment and refreshed
state/history, with no automatic mutation replay. OpenCode V1 histories remain
read-only through the V2 adapter because the native coordinators have different
conversation stores. DSH models come from non-activating `llm.models`, not
`session.models`; catalog image capability is conservatively unknown/false.
Native correlation metadata stays host-side; any eventual UI projection must
allowlist fields and enforce existing viewing/control authorization.

## Current integration

`interaction-core::managed_sessions` owns durable creation receipts, coalesced
resume, native/process leases, restart fences and idle retirement behind driver
and host traits. Pi's native model-on-send, dialogs, workflow deadlines and stream
interpretation live in `pi-adapter::managed::PiSessionDriver`. The supervised
transport exposes no competing common adapter handle; it does not strengthen
native model-on-send capabilities. Native model/dialog parsing lives in the RPC
protocol crate, and persisted header verification lives in the Pi source crate.

The core's `admission` module owns message/control receipts and transport-change
fences. `session_supervisor::SessionCoordinator` owns per-session gates and route
changes; `runtime::RuntimeCoordinator` owns configured lifecycle, startup quarantine,
lease/binding recovery, event pumps and shutdown. The viewer supplies profile
policy, concrete factories, authorization, project/index integration and display
projection. Core modules never import providers or viewer types. Existing wire
fields remain compatible. The old viewer `managed_pi` module is removed.
`ViewerService::interaction_service` remains a host-only composition point,
unavailable to shares or External reader mode.

Selected-machine runtime profiles compose all four adapters through real
managed/attached factories, with separately authorized configuration/lifecycle
commands. A shared Rust session supervisor now routes conversation input,
bound model metadata, live callback answers, and cancellation through these
verified bindings. First Send activates a runtime; inspecting historical work
does not. Codex has explicit RPC/Desktop selection without automatic fallback.
Provider/native-ID cross-process leases are shared across profiles, the owned
Pi driver, and explicit Desktop control. Pi's existing owner/bridge remains
exclusive, and OpenCode V1 history still cannot bind to a V2 coordinator.
Session lifetime is separate from the lifetime of a server shared by several
sessions. See [shared session supervision](shared-session-supervisor.md) and
[runtime configuration](viewer-rpc-runtimes.md) for activation and current limits.

Adapter unit tests use synthetic peers for fault injection. The private
[`interaction-tests`](../crates/interaction-tests/README.md) crate additionally
launches the pinned Pi/OpenCode/DSH agents and an identified Codex executable
against a scripted local model server, captures native traffic, and replays those
fixtures through the adapters.
The default workspace run needs no native executable or provider credentials;
real-process capture is explicit and uses isolated temporary state.
