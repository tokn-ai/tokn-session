# Tokn Sessions Viewer

The viewer is a desktop and browser app for browsing historical Pi, Codex,
OpenCode, ZCode, WorkBuddy, and DeepSeek Harness (DSH) sessions in one place. It
shows root sessions in a searchable, provider-filterable sidebar, expands their
known subagents on demand, and renders each selected normalized event stream as
a conversation with inspectable technical events.
Its composer sends messages to supported live sessions through their owning app.

## Relay lifetime and data modes

By default the viewer uses **Automatic Relay**. Indexed Automatic and Local
modes share authoritative source readers in `viewer-core` and native file watches;
there is no second child parsing the same session append. Polling recovers missed
notifications. Development and packaged builds need no separate Relay install.

The Relay panel offers Automatic, External, and Local modes. Automatic supports
optional native Inspector records (off by default). External connects to an
independently configured `tokn-viewer-api snapshot` endpoint; its processes are
never terminated by the viewer. All six providers use the same provider-root
environment overrides as local reads.

Conversations receive semantic updates at final, steps, or details level.
The frontend retains eight recent session displays, renders cached content on
reopening, and catches up through revisioned snapshots/changes. Tool details
are requested only when expanded or inspected. See
[session update delivery](../../docs/viewer-session-updates.md).

Settings persist in app-config `relay.json`; legacy enabled connections keep
their external endpoint, and explicit disabled settings remain Local. Missing
settings default to Automatic. External snapshots retain last-good content while
reconnecting. Unindexed embeddings still support the managed-stdio Relay child.

## Development

Install the platform prerequisites from the
[Tauri 2 documentation](https://v2.tauri.app/start/prerequisites/), then run:

```sh
cd apps/viewer
pnpm install --frozen-lockfile
pnpm run check
pnpm tauri dev
```

`pnpm run dev:web` builds and starts the API, starts Vite with HMR, and prints
a `#token=…` login link. Open it to connect automatically; the browser removes
the token from the address bar before rendering. Ctrl-C stops both processes.
Set `TOKN_VIEWER_DEV_PORT` if port 1437 is occupied. The launcher uses a free
loopback API port and requires no frontend build.

For separately managed processes, `pnpm run dev` starts Vite. Start
`cargo run -p tokn-viewer-api -- --api-only` in another terminal, then open
`http://localhost:1437` and connect using the prefilled address. Vite proxies
API requests and SSE to port 5558, so no frontend build or CORS flag is needed.
For the integrated browser viewer, run `pnpm run build`
and start `tokn-viewer-api` from the repository root; it serves `dist/` and the
API at `http://127.0.0.1:5558`. See [remote setup and API
contract](../../docs/viewer-api.md). Build the desktop application with:

```sh
pnpm tauri build
```

macOS builds also compile the small Swift bridge for Apple Translation. Use
Xcode 16 or newer with the macOS 15 SDK; other platforms do not compile Swift.

## Response translation

On macOS 15 or newer, **Translate → 简体中文** translates a visible assistant
response using Apple's on-device Translation framework. The first use may show
Apple's prompt to download language packs. No LLM or API key is needed. **Cancel**
stops a pending request, and **Show original** switches back without another
translation. Translation is currently available in the Mac desktop app only.

The viewer loads the full response before translating. It translates prose
inside Markdown while preserving code, URLs, lists, tables, and other markup;
formatting boundaries can limit sentence context. The original session and
Inspector data stay unchanged. Results are kept only in the mounted response
card and discarded when its source changes or the card closes. Responses beyond
the existing detail size limit report an error instead of translating a preview.

## Sending messages

Select a session and use **Message this session** below the conversation.
**Send** or **⌘ / Ctrl + Enter** submits; Enter inserts a newline. Drafts stay
with their session while switching conversations, and Markdown whitespace is
preserved. Drafts are held in memory and cleared when changing machines or
closing the viewer.

- Root Codex tasks use Codex Desktop's local IPC. Desktop must be running and
  own the selected task. Subagents receive messages through their parent task.
- Pi requires the [input bridge](../../extensions/pi/README.md) in the live
  process for that exact session. Idle input starts a turn; busy input queues a
  follow-up.
- Other providers and the desktop External snapshot connection show why input
  is unavailable. To message another machine, connect to its viewer API.

The footer reports acceptance or failure, including the backend's failure
reason. After acceptance, the viewer refreshes history immediately and briefly
checks for delayed writes, preserving loaded history and your reading position.
The conversation displays the message when the provider records it. If delivery
cannot be confirmed, the draft stays locked until **Edit message** is chosen
after checking the conversation. Messages
are never retried automatically. Input is limited to 16,384 characters; Pi also
has a 32 KiB encoded-request limit. The viewer does not create sessions or fall
back to a separate CLI process.

Codex tasks that continue in a new rollout file retain their earlier messages
in the viewer. The reader follows the saved history references and respects
revert boundaries, so removed turns stay removed. If a referenced history file
is unavailable, a read error preserves the last loaded conversation.

## Using the viewer

For Relay-backed providers the sidebar reads Relay's metadata catalog. For
local providers it starts from the local session index rather than reading
provider storage in the initial viewport. On a fresh database, it shows an indexing
state until the background cataloger has committed each selected provider. Use
the provider pills to include or exclude sources, type in the search box to
match indexed title, preview, session id, project, working directory, or agent
identity, and select a row to load its newest normalized events. A disclosure
control loads that session's direct subagents as indexed metadata only; nested
controls continue the tree, and selecting a child opens the child's independent
timeline. This does not merge child events into the parent conversation or
infer live completion states. Rows prefer the indexed provider title, then an
indexed preview of the first meaningful user prompt, then an agent label for
known subagents, and otherwise show **Untitled session**. The shortened id
beside the title remains a separate identity field; the full id is available to
assistive technology and on hover.
Historical agent-activity records become delegation cards when their native
target id resolves to a canonical direct child of the selected session within
the same provider. **Open** selects that child and makes it available in the
sidebar; unavailable, ambiguous, or non-child targets remain inspectable but
are never guessed. These cards describe recorded activity, not live state.
Incoming agent messages show sender and recipient, whether delivery starts a
turn, and expandable Markdown content. **Open sender** navigates to a verified
session in the same task tree; ambiguous identities remain unlinked. Encrypted
bodies show an unavailable notice, while any accompanying readable text remains
visible. The inspector's **Content** view also shows readable communication.
Earlier history is loaded on demand. Technical event headers expand in place,
while their **Inspect** action opens the full inspector. Messages and reasoning
have a readable **Content** view, while **Normalized** and **Native** expose the
debugging representations.

**Hide lifecycle** is a quick filter for routine lifecycle and bookkeeping
rows without message content, including session settings, delivery markers, and
successful turn completion. It applies to the conversation and expanded work
sections, hiding intermediate usage while keeping the final accounting for each
usage kind. Errors, messages, tools, agent communications, and unfamiliar events
remain visible. All events are shown initially; the active button reads
**Show lifecycle**, which restores the hidden rows.
Counts and pagination still describe the complete history, and hidden counts
refer only to loaded rows.

User prompts and final assistant replies remain in the outer conversation.
Contiguous stretches of intermediate assistant progress and non-message work
are grouped into a compact work trajectory item, keeping the
surrounding conversation easy to scan. A non-final assistant message itself is
enough to make a stretch fold; metadata-only stretches remain flat. Terminal
bookkeeping after a final reply remains as ordinary chronological rows instead
of creating a second **Worked** item. It shows
**Working for …** with a ticking elapsed time when a provider turn-start is
observed, auto-expanding to show the newest work. Completion collapses it to
**Worked for …**; it can be reopened manually. Providers without reliable turn
signals show neutral **Work** until a final reply or turn boundary closes the
run. Durations use provider timestamps, never file modification time.
Expanding it fetches its contained event rows in bounded pages and shows
them as their normal event cards. Each row remains independently inspectable,
and a recorded delegation can still open its verified direct child session.

Relay batches refresh existing timeline and expanded child items without
discarding loaded history. Scrolling follows new work only at the bottom;
reading older rows preserves the scroll anchor. Local indexing also refreshes
the selected session after successful body updates, including progress that
does not qualify for unread attention.

Known shell, file, search, web, and task tools use compact semantic headers.
Expanding a tool fetches its output lazily, including the matching result when a
provider records invocation and result separately under the same call id. The
inline preview is bounded, selectable plain text or JSON; it never renders as
Markdown or HTML. User and assistant messages, expanded reasoning, and readable
inspector content do render GitHub-flavored Markdown. Raw HTML is disabled,
remote images are never loaded, and links remain inert so provider content
cannot navigate the WebView.

Usage events have an inline token card. It labels whether a row is a model call,
operation total, or cumulative session snapshot; snapshots replace earlier
snapshots and must not be summed. Cache counts are already part of input, and
reasoning or total counters remain provider-reported. Reasoning cards use a
safe, single-line preview and load readable Markdown only after expansion.
Encrypted or provider-redacted reasoning stays opaque in the timeline.

Provider storage is resolved as follows:

- Codex: `$CODEX_HOME`, then the platform home directory's `.codex` folder.
- Pi: `$PI_CODING_AGENT_SESSION_DIR`, `$PI_CODING_AGENT_DIR/sessions`, then the
  platform home directory's `.pi/agent/sessions` folder.
- OpenCode: `$OPENCODE_DB`, including paths relative to
  `$XDG_DATA_HOME/opencode`; otherwise `$XDG_DATA_HOME/opencode/opencode.db` or
  the upstream home-directory fallback is used.
- ZCode: `$ZCODE_STORAGE_DIR/cli/db/db.sqlite`, then the platform home
  directory's `.zcode/cli/db/db.sqlite` path.
- WorkBuddy: `$WORKBUDDY_CONFIG_DIR/workbuddy.db`, then
  `$CODEBUDDY_CONFIG_DIR/workbuddy.db`, then the platform home directory's
  `.workbuddy-ai/workbuddy.db` catalog and its `projects` histories.
- DSH: `$DSH_HOME/sessions`, then the platform home directory's `.dsh/sessions`
  folder.

For providers using local history, the app commits a stable provider-header catalog to its shared index at
`~/.tokn/sessions/index.sqlite`, then uses only that durable index for sidebar,
search, and subagent-tree requests. Provider reads at startup belong to the
background cataloger, not those UI requests. It then backfills event-derived
attention and any body-derived title/preview metadata in bounded, newest-first
batches. Codex and Pi JSONL rollouts update from native filesystem
notifications; ordinary writes inspect only the changed source. OpenCode,
ZCode, WorkBuddy, and DSH retain a ten-second provider-local catalog cadence,
while an all-provider catalog runs only at startup, on recovery, on explicit
Retry, and as a five-minute safety sweep. While body work is pending, a
body-only pass runs every second without rediscovering a provider catalog. If
active source membership changes during a catalog pass, the previous catalog
remains visible and the app quietly retries; mutable titles, previews, and
modification times do not become false provider-read errors. A
row has no dot until its body has finished
backfilling, except that a relocated row retains an already-unread dot while
its new path is validated. Sessions first discovered after a provider catalog
exists can become unread only after that body confirmation finds a new unhidden
user message or final assistant reply. A dot on a collapsed parent can represent
unread activity in a known subagent. The open timeline refreshes after successful
body updates, including progress that does not qualify as unread activity.
The composer uses the owning runtime; the viewer never edits provider session
files directly.

## Architecture

React owns the presentation and calls a small set of typed Tauri commands. The
Rust backend invokes the workspace's `client`, `core`, and `render` crates
directly for local providers; Relay-backed providers use received snapshots.
It does not parse CLI output. Provider histories normalize to `AgentEvent`
before source-neutral, snake-case DTOs
cross the IPC boundary. Source errors remain isolated so one unavailable
provider does not hide sessions from the others.

Local session discovery starts with provider headers in the background and
deliberately does not compute message or event counts. A complete header
catalog is committed promptly to the shared SQLite sidebar index at
`~/.tokn/sessions/index.sqlite`; sidebar, search, and tree queries are strictly
index-only. Before a provider's complete-catalog sentinel exists, its rows are
reported as indexing rather than read directly from that provider. The separate
body pass backfills attention and missing title/preview metadata in bounded
newest-first batches; the one-second pending worker reads only those selected
bodies. The all-provider catalog is a recovery safety sweep, while unwatched
providers are discovered on their independent ten-second cadence. The
index retains opaque source checkpoints, session identity/paths, bounded
title/preview/cwd/timestamp/relationship metadata, and unread revisions, but
never event records, native payloads, reasoning, tool I/O, or full message
bodies. Header-only metadata changes update the index without a body replay.
Catalog and body replacements are staged with optimistic source-cursor checks:
a body result must still match the catalog snapshot before it can apply.
Checkpoints include an index-owned mutation generation as well as the
provider-owned cursor, so a same-cursor metadata update cannot be overwritten
by a second viewer. The initial main pane intentionally has no selected
timeline; only an explicit session selection reads provider history. Codex can
also
read title metadata from its optional private `state_5.sqlite` in read-only
mode; rows are correlated by both thread id and rollout path, and incompatible
or unavailable private state fails softly. Known Codex subagents intentionally
ignore title and preview fields inherited from their parent in that private
state; their agent nickname, role, or path becomes the row label instead. The
selected session's normalized
event count arrives with its first event page. Root, direct-subagent, and event
responses are listed in bounded pages. Direct-subagent pages resolve edges only
within one provider, canonicalize duplicate provider IDs by newest provider
timestamp (then path), and keep missing-parent or cyclic records visible rather
than hiding them.

The index keeps source identity normalized in `sources`. Its read-only
`indexed_sessions` SQLite view joins `provider` and `source_key` onto every
session row for diagnostics without duplicating those fields in storage.
Conversational Markdown previews are capped before IPC,
tool-card fields are also capped, and full event detail is loaded only when
requested. Inline tool output keeps at most 64 KiB using a head-and-tail preview;
the full normalized and native inspector representations retain their separate
512 KiB limits. The backend keeps at most one normalized session snapshot and
reuses it across page and inspector requests while the source revision is
unchanged; OpenCode and ZCode revision checks include their SQLite WAL
sidecars.
The independent durable index checkpoint uses the database and WAL but excludes
the reader-writable SHM file. The current provider readers may still load an
entire selected session before producing an event page, so paging does not yet
bound parser memory for very large histories.
Trajectory items are a viewer presentation projection over that normalized
timeline: user prompts, final assistant replies, their terminal bookkeeping,
and hidden-event boundaries remain outside the item, while intermediate
assistant progress is folded into it. The item's inner rows are loaded only
after expansion. Their page is bounded separately from the outer conversation
page, so a long turn cannot make initial timeline loading unbounded.
Each normalized and provider-native inspector representation is capped at 512
KiB before IPC. Oversized values become structured JSON truncation placeholders;
an uncapped export path is future work.

Usage-card counters cross IPC as decimal strings so every Rust `u64` remains
exact in the JavaScript renderer. Reasoning-card summaries contain only safe
preview metadata; encrypted content and signatures stay out of that projection.
