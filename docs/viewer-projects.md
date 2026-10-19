# Viewer projects and Pi sessions

The desktop viewer can register existing folders and create Pi sessions on the
viewing computer's local connection. The project registry and managed session
controls are unavailable in browser, HTTP/Hub remote, session-share, and External
reader connections. Other providers retain their existing history and input
behavior; this feature adds no CLI `create` or `attach` command.

## Add a project

Choose **Add project** in the sidebar and enter an absolute path to an existing
directory. The optional name defaults to the folder's name. Registration saves
metadata; it does not create, copy, or move project files.

The saved launch directory is canonical: symlink aliases to the same folder
return the existing registration, including its original name and identity.
Registering again does not rename it. Missing folders remain listed as
unavailable and cannot launch a new session. The directory is checked again
before launching or resuming, including whether its saved path now resolves
elsewhere.

Registered folders appear in **Projects** grouping even before they have any
sessions. Existing inferred project identities, aliases, and ordering anchors
are preserved. Separate worktree folders can be registered under one repository
group; each retains its own launch directory and remains separately selectable
in **New Pi session**. A registration name does not retitle historical sessions.

## Start a Pi session

Choose **New Pi session**, select an available registered folder, enter the
first message, and choose **Start Pi session**. A project group's `+` button
opens the same dialog with that group preferred.

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
queue. The durable Pi history remains the displayed transcript.

## Stop, Resume, and dialogs

The controls distinguish `starting`, `running`, `idle` (shown as **Ready**),
`stopping`, `stopped`, and `failed`.

- **Stop** aborts the current turn and cancels its pending dialog. After a
  history file exists, Pi stays owned and ready for another message.
- Before Pi has persisted any history, **Stop** ends the provisional process.
  There is no saved file to resume in that case.
- Viewer shutdown stops its managed processes. After restart, or after a
  failure, **Resume Pi** explicitly opens the exact saved file with
  `pi --mode rpc --session <file>`. It checks the registered folder and native
  identity again and does not resend the original prompt.

Resume is limited to sessions explicitly created by this viewer; opening an
arbitrary Pi history does not adopt its process. A process lease prevents two
viewer processes from owning the same managed session concurrently.

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

## Storage and limits

Project registrations live in the shared session index (`registered_projects`,
migration 10). Managed ownership receipts default to
`~/.tokn/sessions/viewer/managed-sessions.json`; with a custom index path, they
live in `viewer/managed-sessions.json` under the index's parent directory.
Per-request `managed-pi-<request-id>.lease` files live beside the receipts.
Receipt writes are private, bounded, locked, and atomic. Receipts retain project
and session identity, lifecycle revisions, and a request fingerprint, rather
than prompt text or credentials. Pi keeps its native histories at its own
configured location. See [native storage](native-storage.md).

Each viewer runtime permits eight active managed Pi processes, including idle
ones. The receipt store retains at most 256 creation receipts. Those receipts
are not automatically pruned, and there is no project rename/removal or receipt
cleanup UI yet. RPC frames, command queues, pending requests, diagnostics, and
dialog payloads have independent bounds; token updates do not produce viewer
lifecycle events or per-token receipt writes.

Automated lifecycle tests use a synthetic RPC executable without contacting a
model. Successful real work still depends on the local Pi installation, model,
authentication, and project configuration.
