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
selects a saved runtime profile, starts/connects it if necessary, and binds the
native session before delivering input. A provider default wins; a single
suitable profile can be selected without a default. Ambiguous configuration
requires an explicit default. OpenCode profiles must match the session's native
directory. When Codex has no suitable saved profile, the host can discover
`codex` through `TOKN_SESSION_CODEX_EXECUTABLE` or `PATH` and launch a managed
app-server. This transient local profile does not write new saved configuration.

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
- `viewer-core` owns executable/profile configuration, authorization, project and
  index integration, historical identity verification, and display projection.

Provider adapters retain native transport, state interpretation and command
sequencing. Pi's managed workflow lives in `pi-adapter::managed`, and its bridge
protocol lives in `pi-adapter::bridge`. Typed native models and dialog validation
live in `pi-rpc-protocol`; persisted header validation lives in `tokn-session-pi`.
The former viewer `managed_pi` module no longer exists.

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
`rpc-runtimes.json`; creation receipts remain in `managed-sessions.json`.
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
| OpenCode | Native V2 runtime/profile binding and start-or-steer. V1 history remains read-only; an unsuccessful legacy-history check also blocks mutation. |
| DSH | Existing native sessions through a configured ApiProxy runtime. State/model inspection does not activate cold agents. General session creation is not implemented. |

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
There is no new general permission-approval UI or generalized create-session UI.
OpenCode V2 historical display and broader provider creation remain follow-ups.

Session commands retain existing agent-control authorization and read-only share
rules. Editing executable/environment profiles or administering runtimes remains
a separate host permission; the remote runtime-management opt-in is not granted
by viewing a conversation. See [runtime configuration](viewer-rpc-runtimes.md)
and [live interaction](interaction-core.md) for transport and capability details.
