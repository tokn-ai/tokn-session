# Viewer projects and session creation

The desktop viewer discovers or manually registers folders and creates Pi, Codex, OpenCode,
and DSH sessions on the viewing computer’s local connection. The project registry
and creation commands remain unavailable to browser, HTTP/Hub remote, session-share
and External reader connections. There is no new CLI `create` or `attach` command.

## Create a session

Choose **New session** (or a project group’s `+`), select the agent, registered
folder and runtime, then enter the first message. **Automatic** uses a suitable
provider default or a single suitable saved profile. Ambiguous profiles require
an explicit choice. Missing configuration shows an actionable reason before
creation. Opening the dialog checks configuration without starting a runtime.
New session remains accessible when no folder is available. Choose **Add project**
inside the dialog to register an existing directory and return to the same
provider, prompt and image draft.

Pi keeps its existing model/authentication settings and exclusive worker.
Codex uses app-server RPC and its existing local executable discovery if no saved
profile exists. OpenCode requires a runtime whose configured directory matches
the selected project. DSH passes the project directory to native session creation
and retains the deployment’s default agent preset.

Initial images are supported for Pi, Codex and OpenCode, with host-side validation
against the effective model before delivery. DSH currently advertises no verified
image support. Switching agents keeps drafts and previews; unsupported images
must be removed or used with another supported agent.

The host publishes a conversation only after verifying the native identity and
canonical project folder and indexing its local history. Codex/DSH can defer file
creation until the first input, so initial delivery precedes history readiness.
OpenCode V2 `session_message` history is now decoded locally with native sequence
ordering; V1/ZCode display is preserved, and V1/mixed histories remain read-only.
Remote-only endpoints and histories outside configured local sources cannot be
opened through this flow. Configure readable sources, then check the existing
creation rather than creating another native session.

## Creation recovery and ownership

The shared core durably admits one UUID before native effects. OpenCode/DSH use
reserved native IDs; Codex returns its native thread ID. First-prompt admission
has its own state, and a disconnect or timeout never automatically recreates a
session or replays that prompt. **Check session start** observes the same original
request, provider, runtime and revision. Verified history may open while a first
admission awaits a native question; subsequent answers and controls use the
normal session supervisor. An unconfirmed/rejected first message remains visible.
If an OpenCode/DSH create acknowledgement is lost, the host can find history by
its reserved native ID and adopt the exact existing runtime owner. The first
prompt is still marked unsent and is never replayed during this reconciliation.
Codex cannot recover an unacknowledged server-assigned ID through this receipt.

`viewer/session-creations.json` stores bounded RPC creation identities, phases and
fingerprints, not prompt/image bytes, environment or credentials. The existing
Pi `managed-sessions.json` and its fingerprints/leases remain unchanged.
`session-creation-routes.json` binds canonical creation UUIDs to one provider
across processes and both strategies. Saturated or malformed catalogs reject new
creation while preserving read-only viewing and their original data.

Stop fences a pending first prompt, cancels only that native turn, and waits for
admitted creation work to settle. It does not stop an RPC server shared by other
sessions or an attached external server. Native ownership remains reserved until
its exact runtime is cleaned up. After a host restart, receipts can verify and
open existing saved history; only a new explicit Send resumes its runtime.

## Folder discovery and registration

Every valid absolute `cwd` discovered in saved Codex, Pi, OpenCode, ZCode,
WorkBuddy or DSH session metadata becomes a registered launch folder. Discovery
uses the metadata catalog, never sidebar filters or the currently loaded page.
A verified index can seed these folders without reopening provider histories.
Legacy indexes first reread lightweight headers to verify exact paths; older
display normalization could collapse spaces. Message bodies are not read.
Folder records and per-provider presence are stored in
`~/.tokn/sessions/index.sqlite`.

A discovered folder remains active while any provider retains saved sessions
there. Only a complete, stable scan of the same readable source scope can remove
a provider's claim. Partial reads, disconnected or missing storage, and changed
source roots leave prior claims unknown rather than declaring them removed.
When all claims are gone, the folder stays listed but cannot start sessions.
Reappearing history reuses its saved identity. A missing local directory disables
launching independently; readable session history is retained.

Choose **Add project** in the sidebar and enter an absolute path to an existing
directory. The optional name defaults to the folder's name. Registration saves
metadata; it does not create, copy, or move project files.
Manual folders remain eligible without history, so they can create their first
session. Adding an automatically discovered folder manually pins the same record
without changing its identity or name. Migration 13 treats existing explicit
registrations as manual.

The saved launch directory is canonical: symlink aliases to the same folder
return the existing registration, including its original name and identity.
Registering again does not rename it. Missing folders remain listed as
unavailable and cannot launch a new session. The directory is checked again
before launching or resuming, including whether its saved path now resolves
elsewhere.

Registered folders appear in **Projects** grouping even before they have any
sessions. A folder can be a project without Git. The viewer reads Git metadata
only in the recorded working directory; it never searches parent folders.
Explicit Git worktrees can share their repository group, while a subfolder
without its own Git metadata keeps its own project identity. Each registration
retains its launch directory and remains separately selectable in **New session**. A registration name does not retitle historical sessions.

Migration 12 distinguishes explicit aliases from legacy inferred mappings.
Legacy parent-repository mappings are reconsidered using direct Git or worktree
metadata, with an independent folder identity when that evidence is unavailable.
Registration IDs, names, launch paths, ownership, and existing folder ordering
anchors are preserved.

## Pi worker behavior

Choose **Pi** in New session to retain the established managed-worker behavior.

The viewer starts `pi --mode rpc` in the selected folder and sends the prompt.
Pi owns the session identity and history location. The dialog waits until Pi
has written a complete native session header whose identity and working
directory match the request; the viewer then indexes that exact file and opens
the conversation. It does not create a placeholder transcript or scan all
provider histories to discover the new session. An extension or a slow first
reply can delay this step.

Closing the dialog leaves the pending start accessible from **Pi session
starting · View**. If a start response is uncertain, **Check session start**
checks the same request identity rather than sending its first prompt again.
No prompt is automatically replayed after a runtime failure or restart.

Once open, the ordinary composer sends directly to this owned Pi process.
Idle messages start a turn; messages sent during work enter Pi's follow-up
queue. For a saved owned session whose worker has stopped, **Send** resumes its
exact history, verifies Pi's session identity, and sends only the new message.
The composer shows **Resuming Pi…** while starting. Startup failures preserve
the draft; uncertain delivery requires checking the conversation before retrying.
The durable Pi history remains the displayed transcript.

## Stop, Resume, and dialogs

The controls distinguish `starting`, `running`, `idle` (shown as **Ready**),
`stopping`, `stopped`, and `failed`.

Provider failures appear in the conversation and as **Last turn failed** in
the controls. Pi can remain idle and accept another message after a failed
request. Retries finish before the controls report the final outcome; a later
successful reply clears that error. Resuming a process alone preserves the
previous turn's failure because it does not retry the prompt.

- **Stop** aborts the current turn and cancels its pending dialog. After a
  history file exists, Pi stays owned and ready for another message.
- Before Pi has persisted any history, **Stop** ends the provisional process.
  There is no saved file to resume in that case.
- A worker that remains fully idle for five minutes exits and releases its
  process lease. Cleanup rechecks Pi's native streaming, compaction, and queue
  state, and waits for pending RPCs and extension dialogs. The saved conversation
  remains available; sending another message starts a fresh worker.
- Viewer shutdown stops and reaps its managed processes and prevents further
  starts. After restart, idle cleanup, or a process failure, **Send** opens the
  exact saved file with `pi --mode rpc --session <file>` automatically. The
  optional **Resume Pi** action starts it without sending a message. Both check
  the registered folder and native identity again and never replay an old prompt.

Automatic and explicit resume are limited to sessions created by this viewer;
opening an arbitrary Pi history does not adopt its process. A process lease
prevents two viewer processes from owning the same managed session concurrently.

The supervisor uses the shared Rust Pi adapter's exclusive supervised transport.
The adapter owns child startup, pipes, native response validation, bounded writes,
stderr, and reaping. The viewer retains durable ownership/history and admission
workflows, including its existing model-on-send, dialog, and idle-retirement
behavior. Control frames are filtered before the ordered bounded queue, so token
and tool bodies cannot displace lifecycle evidence. The common adapter API keeps
its existing capability limits; supervised admission is coordinated by this host.
The adapter observes the owned process independently of stdout EOF, so a child
inheriting the output pipe cannot keep an exited Pi worker marked as running.

Pi extension confirmation, selection, input, and editor dialogs appear in the
viewer. Responses return to their exact pending native dialog. Closing a dialog
cancels it; dialogs expire within two minutes, or sooner if Pi requests a
shorter timeout. Unsupported interactive requests are cancelled. Cosmetic
extension notifications and widgets are not rendered here.

## Pi settings and executable

Pi uses the environment inherited by the desktop app and its existing model,
authentication, project configuration, and extensions in the selected working
directory. Registration does not grant a new trust level or provide a sandbox;
the selected folder and Pi configuration determine the code and tools Pi runs.
The viewer does not configure a model or store provider credentials. Set up Pi
on this machine before starting a session.

The executable defaults to `pi` resolved from the desktop process's `PATH`.
Set `TOKN_SESSION_PI_EXECUTABLE` in that process's environment to choose another
executable, for example `/absolute/path/to/pi`. This value is an executable path
only, not a shell command or a place to append arguments. The viewer supplies
the RPC and resume arguments itself. A desktop app launched from the system UI
may inherit a different environment from a terminal.

For TLS connections, an explicit `NODE_EXTRA_CA_CERTS` setting takes precedence.
When it is absent, the viewer forwards a nonempty inherited `SSL_CERT_FILE` to
Node's extra CA setting for managed Pi processes. This honors an already
configured certificate bundle while keeping certificate verification enabled.

## Storage and limits

Project registrations live in the shared session index (`registered_projects`,
migration 10). Migration 11 adds the derived managed-session admission index.
Existing registrations and session metadata survive both upgrades; the known
development build that combined these schemas is adopted transactionally after
schema verification. Unknown migration checksums continue to block startup.
Managed ownership receipts default to
`~/.tokn/sessions/viewer/managed-sessions.json`; with a custom index path, they
live in `viewer/managed-sessions.json` under the index's parent directory.
Per-request `managed-pi-<request-id>.lease` files live beside the receipts.
Provider/native-ID leases in `viewer/session-leases` additionally coordinate
owned Pi sessions with the shared RPC/Desktop session supervisor. A known
identity is claimed before resume; a new identity is claimed before its first
prompt. The last owner releases its lease after confirmed cleanup.
Receipt writes are private, bounded, locked, and atomic. Receipts retain project
and session identity, lifecycle revisions, and a request fingerprint, rather
than prompt text or credentials. Pi keeps its native histories at its own
configured location. See [native storage](native-storage.md).

Each viewer runtime permits eight active managed Pi processes, including idle
ones until cleanup releases them. The five-minute idle timeout restarts after
work settles; reading a conversation does not keep its worker alive. The receipt
store retains at most 256 creation receipts. Those receipts are not automatically
pruned, and there is no project rename/removal or receipt
cleanup UI yet. RPC frames, command queues, pending requests, diagnostics, and
dialog payloads have independent bounds; token updates do not produce viewer
lifecycle events or per-token receipt writes.

Automated lifecycle tests use a synthetic RPC executable without contacting a
model. Successful real work still depends on the local Pi installation, model,
authentication, and project configuration.

**Settings → Runtimes** lists these workers and saved conversations alongside
the viewer's other runtimes. Its Restart action replaces a worker without
replaying a prompt; see [runtime management](viewer-runtimes.md).
