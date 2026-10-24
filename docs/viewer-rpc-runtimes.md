# Viewer agent runtimes

**Settings → Agent runtimes** configures the selected machine's native RPC
processes and server connections. This is separate from **Runtimes**, which
inventories the local desktop's reader, hosting, and existing Pi workers.
Opening settings, inspecting profiles, and validating configuration never launch
an agent, resume history, or send a prompt.

## Profiles and storage

Profiles support managed Codex/Pi/OpenCode/DSH processes, attached Codex WS(S),
OpenCode V2 HTTP/SSE, and DSH HTTP/WebSocket servers. Pi attachment needs explicit
IO handles and is not available through profiles. Managed profiles require an
absolute executable and working folder; attached OpenCode also requires its
native working folder. Endpoint URLs exclude embedded credentials, queries,
and fragments. OpenCode/DSH managed launch arguments are fixed by their adapters.

Native storage defaults to `~/.tokn/sessions/viewer/rpc-runtimes.json`, beside
the configured viewer index, with private permissions, bounded size, atomic
writes, and a revision fence. There are at most 32 profiles and one optional
default per provider. The session supervisor uses the default, or a single
suitable profile, when an explicit first Send needs a runtime. Saving a default
does not start it or replace an already connected binding. Corrupt or newer-format
storage is preserved;
repairing the file and choosing Refresh retries the read without restarting.

Reads expose environment variable names and credential kinds, never their
values. Writes can retain, replace, or clear saved environment/authentication.
Attached OpenCode supports Basic authentication; attached Codex/DSH support
Bearer authentication. Managed credentials use environment variables. Public
arguments must not contain secrets. Validation checks schema and local path
readiness without running executables or probing endpoints. Saving a profile
does not require its executable or folder to exist yet.

## Lifecycle and access

Start/Connect checks managed launch files, explicitly opens an adapter, verifies
metadata, and registers it with the shared Rust `InteractionService`. One runtime
per profile gets a fresh instance ID and an exclusive cross-process profile lease. Saved edits apply on
the next connection. Stop targets that exact instance and awaits owned child
cleanup; Detach closes transport while leaving an external server running.
Delete requires the runtime to be closed. Disconnects and event gaps require
explicit close/reconnect rather than silent replacement or prompt replay.

Start/close admissions survive caller cancellation and use bounded UUID
receipts. Repeating an ID cannot repeat an uncertain action. Public failures
exclude native diagnostics that might contain secrets. A failed cleanup keeps
ownership reserved until shutdown. Managed launch errors also reserve the
profile when the adapter cannot prove whether a child was spawned and reaped;
repeating Stop cannot establish that missing proof. Host shutdown closes
admission, waits for in-flight actions, and shuts down registered adapters before
releasing leases. Deletion holds the same lease, and startup rechecks the profile
under that lease so concurrent viewer processes cannot launch deleted profiles.
Normal Start/Connect retries reuse their request ID. After an explicit Refresh
confirms the same profile revision and no runtime, a separate New attempt action
can request a fresh admission. Active or quarantined instances exclude it.

Desktop commands and full-token direct HTTP administration expose the same
profile and lifecycle APIs. Encrypted Hub clients additionally require all of:
active paired Native/BrowserSession trust, agent control, and the separate
host-owned **Remote runtime management** opt-in (off by default). The local
desktop switch and `tokn-session-hub runtime-management --enable|--disable`
manage that opt-in; remote clients cannot enable it themselves. This permission
allows configuring executables and their environment on the host. Shares,
enrollment clients, and legacy grants cannot manage runtimes. See
[Hub pairing](hub-pairing.md) for connector compatibility and permission storage.

`rpc-runtimes-changed` carries only an empty invalidation. The visible settings
page reloads bounded public inventory; native events and credentials do not
become timeline traffic. Delayed requests remain bound to their original
machine, and profile revisions reject stale edits.

## Current integration boundary

Host-verified session bindings now serve the shared conversation controls.
Bindings verify catalog identity/provider, fence the runtime instance, recover
state/pending interactions, and reserve native ownership even after uncertain
resume or lost transport. A separate provider/native-ID cross-process lease
prevents another profile, owned Pi driver, or explicit Desktop route from
claiming that same session. Codex subagents remain controlled through their
parent. OpenCode V1 history cannot bind to a V2 runtime.

First Send can start/connect the selected runtime and resume/bind an existing
session. Codex defaults to RPC and can discover a local executable when no
suitable saved profile exists; Desktop control must be explicitly selected.
Bound metadata and live callback answers use the common service. Runtime
failures do not automatically select another transport or replay a prompt.
Opening historical work remains view-only. Local desktop projects can create
Codex/OpenCode/DSH sessions through these configured runtimes; OpenCode V2 local
history is supported. Arbitrary Pi history adoption and remote-only RPC history
remain future work. See [shared session supervision](shared-session-supervisor.md).

Tests exercise profile persistence/redaction/revision rules, all four factory
paths with scripted native RPC peers, process reaping and leases, binding
ownership, cancellation, shutdown, transport authorization, and frontend state.
The separate [native RPC harness](../crates/interaction-tests/README.md) records
and replays real agents; the supervisor's scripted peers are not native-agent
captures.
