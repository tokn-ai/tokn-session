# Viewer runtime management

**Settings → Runtimes** lists services and processes known to this desktop
viewer, independently of the selected machine. Opening it preserves the current
conversation and does not initialize local readers, create identities, or launch
stopped workers. Browser clients cannot use these native management commands.
The inventory is not an operating-system process scan.

Compact rows show the name, status, available restart action, and an external
ownership marker when relevant. Runtime icons and labeled status badges use
theme-aware color; external connections are not presented as verified healthy.
Details hold execution/ownership information,
explanations, session/project identity, working directory, public endpoint, and
directly known PID/start time when available. Errors remain visible in their row. Tokens,
environment variables, prompts, and private credentials are not inventory data.

## Listed runtimes

- **Pi sessions:** live viewer-owned RPC workers and their saved stopped
  conversations. A worker's PID and start time disappear when it exits; those
  process facts are never restored from durable session receipts. A session
  leased by another viewer process is externally owned here.
  Pi restart also respects the existing local-reader control restriction:
  selecting External reader mode keeps these rows visible but disables restart.
- **Session reader / Relay:** the native indexed reader runs inside the viewer,
  including Automatic mode. It is not a process per provider. Legacy unindexed
  Automatic embeddings may own a bundled Relay child. An explicitly external
  Relay is shown without process restart controls.
- **Hub hosting:** the app-owned API and connector restart together using their
  existing hosting configuration and identity. Known background API/connector
  services remain external and read-only in this view.
- **Hub connections:** registered open machine connections show their current
  direct or encrypted-relay carrier. Carrier metadata does not assert that the
  remote host is healthy. Use Machines to manage these connections; the viewer
  cannot restart their remote processes.

## Restart behavior

Restart is available only when the owning backend can replace the displayed
runtime safely. It targets a stable runtime ID and the exact displayed instance;
a stale action refreshes the inventory rather than restarting a replacement.
Each action has a UUID receipt, so a canceled or repeated invocation does not
repeat the same restart. Receipts are memory-only and bounded to 256 per app
lifetime, with at most eight pending actions.

Pi restart requires verified saved history, interrupts any current turn, waits
for the old worker to exit and release its lease, and resumes that exact file.
It never resends a previous prompt. A new Send during replacement waits for the
replacement worker before submitting its new message. A failed restart leaves
the saved conversation readable and reports its error.

Reader restart rebuilds the owned reader connection using its current settings;
previously expanded history and native inspection remain available while source
followers recover without reopening the conversation. Hosting restart
temporarily disconnects clients and keeps the enrolled host identity. It waits
briefly for the Hub to observe the old connector's departure before reconnecting;
an unconfirmed handoff reports an error without bypassing ownership checks.

Runtime lifecycle events refresh the visible inventory. Ordinary chat appends
do not trigger inventory scans. Refresh failures retain the last known rows and
offer Retry; restart progress and errors belong to the affected row. Inspection
does not keep idle Pi workers alive. Viewer quit closes restart admission and
stops its owned workers and services, including an in-progress replacement.
