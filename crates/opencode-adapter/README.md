# tokn-opencode-adapter

Rust implementation of `tokn-interaction-core::InteractionAdapter` for native
OpenCode V2 HTTP/SSE runtimes. Audited against OpenCode 1.18.8 at
`017a5977d2107092007623e507fc5c6eb337d3b2`.

`OpenCodeAdapter::connect` attaches to an explicitly configured server origin,
directory/workspace, and optional host-owned Basic credentials.
`OpenCodeAdapter::launch` owns a loopback `opencode serve` child with fresh Basic
credentials. Closing an attached adapter only detaches; closing a managed adapter
stops its actual child. `create_session` creates a native V2 conversation.
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

Legacy V1 history is checked before mutations because OpenCode's V1 and V2 loops
share session metadata but use separate conversation projections. Such sessions
remain read-only instead of silently continuing without their existing context.
V2 state describes this server's coordinator, not every process using the same
database. The host must bind an existing session to its configured runtime.

Real-runtime coverage found that the native legacy-history endpoint loads the
legacy configuration parser, which rejects V2-only `providers`/`permissions`
configuration. Such locations remain read-only with an explicit unsupported
reason: a failed legacy check never counts as proof of empty history. The real
integration suite uses OpenCode's supported legacy configuration, which V2
migrates. Resolving this native split needs an authoritative history-ownership
query; V2 metadata and an empty V2 conversation cannot establish V1 absence.

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
