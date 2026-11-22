# tokn-opencode-adapter

Rust implementation of `tokn-interaction-core::InteractionAdapter` for native
OpenCode HTTP/SSE runtimes. Supports the 2.3.0 inbox/form API at
`5756d59a68a3d16ce412280c3ab055031898cce2` and earlier V2 at
`017a5977d2107092007623e507fc5c6eb337d3b2` (1.18.8).
Read-only discovery uses earlier `/api/health` and `/doc`, or current
`/api/info` and `/openapi.json` when the earlier health endpoint is absent.
Authentication and transport failures do not select another dialect.

`OpenCodeAdapter::connect` attaches to an explicitly configured server origin,
directory/workspace, and optional host-owned Basic credentials.
`OpenCodeAdapter::launch` owns a loopback `opencode serve` child with fresh Basic
credentials. Closing an attached adapter only detaches; closing a managed adapter
stops its actual child. `create_session` creates a native V2 conversation.
`create_session_with_id` accepts a host-preallocated ID (`new_session_id`) and
returns the exact native location after checking both identity and directory/workspace.
It does not infer a local history path, send a prompt, or retry creation.
Managed startup waits for the child's native readiness URL and drains bounded
stdout/stderr buffers. Port `0` follows OpenCode's native port selection, including
its preference for `4096`; an already-running server is never adopted by probing a
configured port. Startup failure and explicit close kill and reap only the owned
child, with bounded waits.

The adapter reads live models, agents, providers, commands, session state, pending
questions and permissions. Message admission uses stable native message IDs and
native `steer` or `queue` delivery. A steering admission can start an idle loop,
and is reported as accepted rather than guessing whether it started or steered.
Strict idle-only/active-only admission and atomic model-on-send are unsupported.
Models remain selectable metadata; a separate model mutation is deliberately not
used to simulate an atomic prompt override.

Earlier V2 checks legacy V1 history before mutations because its V1 and V2 loops
share session metadata but use separate conversation projections. Such sessions
remain read-only instead of silently continuing without their existing context.
The current server resolves sessions only from its separate `session_v2`
namespace, so it never uses the removed legacy HTTP history route. Explicit
workspace-scoped connections are unsupported on the current API because its
public responses omit workspace identity; directory identity is still checked.
State describes this server's coordinator, not every process using the same
database. The host must bind an existing session to its configured runtime.
Native execution/step-start events publish running state for verified session
identities. While those sessions are running, one bounded passive active-map
read every 250 ms detects actual coordinator settlement; per-step completion
never guesses that an entire turn ended. Polling stops when the native map is
idle, and status events preserve the last known model selection.

Real-runtime coverage found that the native legacy-history endpoint loads the
legacy configuration parser, which rejects V2-only `providers`/`permissions`
configuration. Such locations remain read-only with an explicit unsupported
reason: a failed legacy check never counts as proof of empty history. The real
integration suite uses OpenCode's supported legacy configuration, which V2
migrates. Resolving this native split needs an authoritative history-ownership
query; V2 metadata and an empty V2 conversation cannot establish V1 absence.

Current question-tool forms preserve opaque field and option values. Ordinary
string/multiselect questions map to shared questions; conditional, constrained,
hidden and other forms stay visible as unsupported editors that can be declined.
Current permission replies use `decision`; earlier V2 retains `reply`.

JSON and SSE responses are bounded, requests have deadlines, redirects are
disabled, and malformed/unknown native events remain inspectable. Reconnection
emits a gap notification: consumers must refresh state/pending interactions and
recover durable history; global SSE itself has no replay guarantee. Attached
stream loss invalidates the adapter: attach again to obtain a new connection
instance before further commands. Managed reconnects retain their generation only
while the actual child remains alive; mutation checks its process handle.
Runtime event
sequences are local publication order, not native durable aggregate cursors.
Credentials and native correlation metadata must never be forwarded directly to
UI clients. No provider calls or native executables are needed by the fake HTTP
tests.

Licensed under the [MIT License](LICENSE).
