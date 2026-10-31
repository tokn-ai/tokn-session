# Shared session supervisor

`interaction-core` owns live session orchestration. The viewer composes it for
the desktop, direct HTTP client, and encrypted Hub clients through host traits. Historical readers and
timeline delivery remain separate. Opening a session, checking control
availability, or reading an unbound session's model context never starts an
agent, resumes a native session, or sends input.

## Routing and activation

Codex defaults to **RPC**. The composer offers an explicit **Codex Desktop**
alternative, saved per native thread on the selected machine. There is no
automatic fallback between these transports. Desktop uses the existing owner
discovery/input path; it does not provide a verified model catalog.

The first Send verifies the host's historical identity and working directory,
resolves a runtime plan, starts/connects it if necessary, and binds the native
session before delivering input. An explicit profile is checked at its exact
revision and never falls back. Automatic prefers a suitable provider default or
sole suitable saved profile; ambiguous profiles require a default. OpenCode
profiles must match the session's native directory. With no suitable saved
profile, the host can discover local Codex/OpenCode/DSH executables through
`TOKN_SESSION_CODEX_EXECUTABLE`, `TOKN_SESSION_OPENCODE_EXECUTABLE`, or
`TOKN_SESSION_DSH_EXECUTABLE`, then `PATH`. These project-scoped managed
configurations do not write saved profiles. An invalid explicit executable
override does not fall back to another binary.

`interaction-core::runtime_plan` separates configuration identity from a running
instance. `RuntimePlan` records provider, configuration/revision, ownership,
reuse scope, lifecycle strategy and working directory. The host uses
the same resolution for creation availability, draft catalogs and first Send.
The additive opaque `runtime_plan_id` fences the configuration shown to a draft;
the host rechecks it before admission. Existing-session activation also requires
a host-verified native descriptor and rechecks the pinned configuration before
launch. Connected bindings retain their exact runtime when defaults change.
Resolution itself reads only configuration/filesystem metadata. Native adapter
handshakes establish protocol compatibility when a runtime actually starts.

Missing executables, directories, or profiles produce an unavailable capability
or a definite pre-send failure. A dropped acknowledgement after input is written
remains uncertain; reconnecting or choosing another transport never replays it.
An existing broken/gapped runtime requires explicit reconciliation, rather than
being silently replaced by the next Send.

Changing Codex transport is an explicit operation. The host checks unresolved
admissions under the session gate and requires an existing RPC binding to be
idle, have no pending inputs/questions, and have current recovery proof before
releasing it. Desktop switching also checks recorded activity. Releasing a
session does not stop a server shared by other sessions. Stop cancels the native
turn; it does not mean stopping the runtime or clearing its queued inbox.

## State and ownership

Native protocols, adapter workflows, shared orchestration and host composition
have separate responsibilities. The core never imports provider crates or viewer
types:

- `managed_sessions` owns durable creation receipts, coalesced resume, process
  leases, restart fences, admission queues and idle retirement. Its driver trait
  supplies native behavior; its host trait supplies opaque verified session keys.
- `admission` owns message/control receipts, uncertain-delivery retention and the
  atomic admission fence used while changing routes.
- `session_supervisor::SessionCoordinator` owns session gates, deduplicated route
  transitions and route ownership. Host hooks supply idle release and persistence.
- `runtime::RuntimeCoordinator` owns configured process/connection lifecycle,
  instance fences, native binding recovery, quarantine and awaited shutdown.
  Host hooks select and validate profiles and construct concrete adapter handles.
- `runtime_plan` defines pure configuration intent and verified-history evidence;
  it does not start processes or acquire native ownership.
- `viewer-core` owns executable/profile configuration, authorization, project and
  index integration, historical identity verification, and display projection.

Provider adapters retain native transport, state interpretation and command
sequencing. Pi's managed workflow lives in `pi-adapter::managed`, and its bridge
protocol lives in `pi-adapter::bridge`. Typed native models and dialog validation
live in `pi-rpc-protocol`; persisted header validation lives in `tokn-session-pi`.
The former viewer `managed_pi` module no longer exists. Pi now resolves through
the common plan boundary as a session-scoped worker, while retaining its driver,
catalog, lifecycle and receipt strategy. Saved Pi RPC profiles do not replace
that creation workflow in this migration.

`viewer/session-leases` contains private cross-process leases keyed by a SHA-256
digest of provider and native session ID. RPC profiles, the owned Pi driver,
and explicitly selected Codex Desktop control use the same scope. Session
ownership is independent of the profile lease, so configuring a second profile
does not authorize concurrent control of the same native session. Cloned lease
handles share one lifetime; only the final holder explicitly unlocks the file.
Cleanup or resume uncertainty preserves the relevant ownership reservation.
These leases coordinate Tokn hosts sharing that storage; they do not fence
independently operated native applications or servers that ignore them.

Codex transport preferences live in private, bounded
`~/.tokn/sessions/viewer/session-controls.json`. Runtime profiles remain in
`rpc-runtimes.json`; Pi creation receipts remain in `managed-sessions.json`,
while RPC creation receipts use `session-creations.json`.
Legacy version-one Pi receipts migrate to version two with explicit provider
identity; their existing `managed-pi` process-lease namespace is retained.
Custom index locations use the corresponding viewer state directory. Input and
control receipts prevent duplicate native writes while retained, including when
the API caller disconnects. They do not promise persistent exactly-once delivery
after host restart. Question responses and cancellation remain independent of a
message awaiting native acknowledgement, allowing extension preflight to finish.

## Provider behavior and current limits

| Provider | Existing-session control |
| --- | --- |
| Codex | Explicit app-server create/resume binding; start-or-steer, turn interruption, model catalogs, and live callback questions. Desktop is an explicit alternative. |
| Pi | Viewer-owned sessions keep the exclusive Pi workflow driver; independently running bridges keep their existing owner. A Pi profile cannot adopt arbitrary saved history from an ID alone. |
| OpenCode | Native V2 creation, local history, runtime/profile binding and start-or-steer. V1 and mixed histories remain read-only. |
| DSH | Existing native sessions through a saved or locally discovered ApiProxy runtime. State/model inspection does not activate cold agents. Local project creation uses the native default preset. |

The Pi driver owns model-on-send sequencing, native extension dialogs,
dialog-aware deadlines, stream/error interpretation and idle eligibility. The
core owns durable creation, verified identity publication, restart/resume and idle
retirement policy. Pi's adapter owns the process, pipes, bounded framing and
correlation, ordered control events, stderr, and reaping. Known-session leases
precede resume; new-session identity is claimed before the first prompt.
Cleanup completes before ownership is released, and an unconfirmed cleanup
retains its reservation.

The common RPC adapters expose real model catalogs but cannot guarantee applying
draft model intent only when an admission starts an idle turn. Their model
selectors therefore remain read-only with an explanation. Pi's existing owned
workflow is preserved separately; its host coordination does not strengthen
generic native capability guarantees. Strict delivery modes also remain
provider-specific; the composer uses native start-or-steer admission and does
not guess a started/steered outcome when the native reply only says accepted.

Existing question cards can answer only a matching live RPC callback, with exact
session/request/question IDs and validated complete answer batches. Recorded
history alone cannot authorize a reply. Native callback envelopes and credentials
remain host-side. The separate Codex `request_user_input_async` tool emits
assistant-item metadata without a callback and is unavailable through RPC;
answering it through Desktop requires explicitly selecting Desktop control.
The desktop New session flow uses the same supervisor for Codex/OpenCode/DSH
creation, while preserving Pi’s specialized worker. Remote-only history and a
general permission-approval UI remain follow-ups.

Session commands retain existing agent-control authorization and read-only share
rules. Editing executable/environment profiles or administering runtimes remains
a separate host permission; the remote runtime-management opt-in is not granted
by viewing a conversation. See [runtime configuration](viewer-rpc-runtimes.md)
and [live interaction](interaction-core.md) for transport and capability details.


## Durable native creation

`interaction-core::creation::CreationManager` admits immutable creation intent
before native effects. Its persisted phases distinguish native-creation
uncertainty, known native identity, first-input uncertainty and history readiness.
Request leases and a shared UUID/provider ledger cover independent processes and
Pi/RPC strategy boundaries. RPC receipts persist the resolved plan and original
optional client proof, so retries keep their exact runtime even after defaults
change. Pi receipts retain the optional plan proof in their existing workflow.
Legacy receipts preserve their prior fingerprints. Receipts never persist
prompt/image bodies, executable paths, endpoints or credentials. Repeating or
reloading a request does not resend creation/input.

The runtime coordinator creates and reserves the exact native target, then adopts
an indexed host-verified key without calling native resume. While initial input
awaits a question, bounded history verification can publish the conversation so
normal callback controls can answer it. Live binding recovery failures remain
visible/retryable; history-only publication is allowed when that exact prior
runtime no longer exists after a host restart. Cancellation fences first input
and drains the admitted workflow while keeping shared servers alive.
Lost OpenCode/DSH create acknowledgements can reconcile through their planned
native IDs, verified local history and an existing exact runtime owner. This
does not retry native creation or first input, and preserves the uncertainty.
