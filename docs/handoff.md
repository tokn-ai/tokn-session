# Handoff

Read `AGENTS.md` first for the project goal, stable architecture, and working rules. This file tracks volatile implementation status and context a future AI would otherwise need to rediscover.

## Current Status

Version 0.1.1 is being prepared for all 23 publishable crates; the desktop stays
`publish = false`. Registry dependency versions, package-local docs/licenses and
test fixtures, and Rust 1.95 declarations are in place. Codex protocol preserves
the published 0.1.0 enum/struct shapes through additive metadata accessors.
Release CI verifies archives and the minimum Rust version. See
[releasing](releasing.md) for validation, separate frontend assets, and the
explicit publication step.
`scripts/publish-crates.py` resumes partial publication by skipping current
versions already on crates.io. `--list` checks only; `--dry-run` verifies without
uploading; the default publishes pending crates through Cargo.

Session delivery separates live subscription from loading. Direct browsers use
connection-scoped WebSocket interests and live diffs, with backward HTTP for
initial/older screens, details HTTP for complete groups/tool display payloads,
and dedicated inspection HTTP for source/native records. Tauri exposes the
same commands through its event bridge. Opening defaults to steps for the latest
three turns with collapsed inner groups and never backfills saved reading anchors before
first paint. Heartbeats renew identity leases without fetching data.
Cold source startup runs outside the shared subscription lock, and initial
source windows now contain three user turns plus dependency/attention context.
Codex window readers scan backward to explicit turn starts, normalize only the
latest three turns, and prepend older turns on request. Split tool outputs widen
the range to their invocation; completed lifecycle snapshots do not. Inherited
prefixes retain metadata/version/guard verification without normalizing their
omitted bodies. Legacy formats without turn starts and thread-spawn filtering
retain full-reader fallbacks. Other providers still normalize full histories.
Use `profile_local_codex_open` to compare full decoding with lazy reader startup.
Snapshot baselines and publication share revisions; reconnects and replacements recover
through backward reads. Old-socket cleanup cannot remove reclaimed interests.

Frontend replicas retain eight recent sessions. Independent group/tool/inspect
coverage tracks missing, loading, complete, stale, and failed resources; loaded
groups receive live appends after collapse. Display details and inspection are
separate caches. Legacy servers and the existing Hub HTTP tunnel retain SSE
compatibility; Hub allows the new loading commands. Shared-session authorization
continues through its legacy scoped commands. Compact attention notifications
avoid redundant catalog/timeline reads. See [session delivery](viewer-session-updates.md)
for ownership, recovery, limits, and remaining source/projection costs.

The viewer sidebar and conversation use a compact Codex-style layout with neutral
light/dark colors, larger message text, inline expandable tool activity, and an
aligned composer. Worked/Working disclosures use wrapping activity summaries and
an indented activity trail. Each turn contains activity groups separated by
intermediate messages. Inner disclosures summarize actions (commands, reads,
edits, searches) rather than duration; only the outer turn uses Worked/Working.
Lifecycle/bookkeeping is hidden by default, with an explicit reveal toggle.
Running and failed activity automatically opens its inner group. Child outputs open
independently and retain content during live refreshes. Shell panels expose the
command, cwd, exit status, and an eight-line output preview with local expansion.
Collapsed event headings occupy one line; expanded paths wrap. Work durations
use whole seconds. When questions or compaction split an active turn, only its
latest work segment remains running; earlier durations stop at the separator.
Live work shows a spinner. Provider filters and session metadata live in disclosures.
Desktop sidebar collapse preserves navigation state and removes hidden controls
from keyboard focus; mobile retains the modal session drawer. The latest-activity
button floats inside the history area so it does not shift the reading position.

The viewer tracks unanswered questions separately from unread final replies.
Sidebar badges and session notices show **Input required** for explicit blocking
requests and **Question available** for async/unknown blocking requests. They
remain visible after read acknowledgement; exact question-ID replies resolve
individual questions, while turn completion, supersession, final replies, and
errors retire remaining attention. Clicking a sidebar badge/session notice
opens and focuses the outstanding question card. Retained windows preserve
outstanding request context. Compact `session-activity.v4` markers persist counts;
older final-only markers migrate quietly without resetting unread replies.
Accepted event pages synchronize sidebar question counts, including cached child
rows; the selected timeline overrides catalog counts while indexing catches up.

`tokn-session` can list and show existing sessions from Pi, Codex, OpenCode,
ZCode, WorkBuddy, and DSH.

Codex Desktop `token_usage_record` rows now normalize to per-response usage
cards. Turn/thread totals remain native inspection detail; the separate
`token_count` events retain session-snapshot semantics. Both accounting forms
can occur between correlated compaction records. See [event semantics](event-ir.md#usage).

Codex incoming agent messages now have expandable Markdown communication cards,
including in paginated history. Cards expose the recorded turn-trigger flag,
link verified senders within the same task tree, and label encrypted bodies as
unavailable. See [communication semantics](event-ir.md#agent-communication).

Codex structured questions now have historical `question_request` events and
expandable viewer cards outside work trajectories. Paginated async message
items retain questions/choices; legacy tool invocations and recorded
`request_user_input` events retain structured question descriptions and recorded flags.
Native payloads stay inspectable, and malformed canonical question items remain
unknown. Structured tool results now become `question_reply` user rows, with
answers linked to question IDs and prompts by call ID across incremental reads.
Empty answers remain visible; malformed replies retain native unknown records.
Desktop/TUI `<send_user_message_question_reply>` envelopes now also become
answer cards, preserving `questionItemId` and deriving the request ID from its
async identity tuple. Ordinary, malformed, or quoted messages remain text.
Cards show recorded history; live answering is deferred.
`vendor/codex` is pinned to upstream `2351d9e1b6` (2026-10-09).
See [question semantics](event-ir.md#questions).

Implemented CLI:

```sh
tokn-session list --source codex --limit 5
tokn-session show --source opencode <session-id> --format pretty
tokn-session show --source codex <session-id> --scope tree
tokn-session show --source pi <session-id> --format jsonl
tokn-session list --source zcode --limit 5
tokn-session list --source workbuddy --limit 5
tokn-session browse --source codex <session-id>
tokn-session create --source opencode --executor "tokn-gateway proxy opencode --npx --" "create a todo app"
tokn-session append --source opencode --executor "tokn-gateway proxy opencode --npx --" --session <session-id> "next turn"
tokn-session append --source opencode --executor "tokn-gateway proxy opencode --npx --" --continue "next turn"
tokn-session-relay zeromq
tokn-session-relay stdout
cd apps/discord-pet && bun run login
cd apps/discord-pet && bun run start
cd apps/pet && bun run start
cd apps/terminal-pet && bun run start
cd apps/viewer && pnpm tauri dev
```

The old `tokn-session sessions list/show` shape is intentionally unsupported.

## Compaction

First-class `AgentEvent::Compaction` is implemented for Codex, Pi, OpenCode,
ZCode, and DSH; WorkBuddy is deliberately deferred. See [event semantics](event-ir.md#compaction)
for source evidence and provider-specific state/measurement differences.
The viewer projects correlated observations into one expandable card outside
work trajectories, with a stable first-record key, readable summary, scoped
token measurements, and optional Relay-native contributor detail. Compaction
does not complete a turn or count as unread conversation; terminal-pet ignores
it for activity/focus. Existing Relay follow/cache updates carry the event,
including when native is off. Earlier transcript content remains visible.
Codex checkpoint/completion correlation survives its validated snapshot metadata
and accounting, with session/turn guards; both native contributors remain inspectable.
Codex/Pi persisted history does not expose compaction start; OpenCode exposes a
request, while ZCode/DSH expose explicit start/end observations. ZCode coverage
is based on the installed 3.7.3 bundle and representative fixtures, not a real
captured compaction. Upgrade strict `AgentEvent` consumers with the producer.

## Session Hub

The Hub and browser UI are deployed at `https://ahub.clouds56.top` on ctl host
`vultr-2`, using nginx HTTPS/WebSocket termination and a dedicated loopback
systemd service. Persistent state stays outside versioned releases. See
[deployment](../deploy/vultr-2/README.md) for configuration, checks, and updates.

Desktop **Machines → This machine → Host this computer** now owns optional
hosting for the app lifetime. It reuses `~/.tokn/hub` trust, offers Hub/name and
remote-input settings, Start/Stop, status events, and an on-demand pairing
reference/QR/URI/current-code display. Secrets stay in the local command response
and are cleared from display when hidden. A token-protected ephemeral loopback
API shares the existing LocalViewer service/events; it does not persist its port
or install a daemon. App exit stops hosting before stopping local readers.
Shared `host_setup` prepares CLI/app profiles; connector ownership locks prevent
updated processes from competing. The app also probes the saved Hub's secure
route for older online connectors before modifying configuration, and refuses
silent takeover. Saved-host startup requires that check to succeed. Hosting
starts explicitly on each app launch. See [desktop hosting](hub-pairing.md#host-from-the-desktop-app).

Remote app/browser clients use the Hub for discovery, pairing and encrypted
WebRTC negotiation, then prefer direct connections with encrypted Hub relay
fallback. `hub-transport` supplies a bounded record interface for future native
tunnels; each WebRTC DataChannel retains fresh Noise IK and existing host/device
authorization. Direct peers survive Hub reconnects; path changes migrate live
streams without replaying commands. No public STUN service is enabled by default;
host `--stun-server` settings persist and reach authenticated clients. The panel
shows Direct/Relayed separately from encryption. See [transports](hub-transports.md).
Hub serves the browser UI and forwards encrypted records when needed; endpoints run the shared Rust pairing/Noise
implementation (`hub-client-core`, compiled to WASM for browsers).
`connect --hub …` saves UUID/keys/config, displays a local TOTP setup QR, and
prints a machine reference `UUID@host_public_key`. Open the Hub URL or choose
Hub in the app, then enter the machine and authenticator code. The host verifies
pairing with HMAC-SHA-256. App pairing grants persistent access using owner-only
native key files. Browser TOTP pairing only grants five-minute passkey enrollment;
passkey sign-in grants at most eight hours to a fresh in-memory tab key. New tabs,
reloads, and reopened windows require sign-in; live-tab network reconnects retain
authorization. IndexedDB v2 holds metadata/pins only and deletes legacy secrets.
Pagehide clears active content and keys; BFCache restoration reloads. Host state
v2 distinguishes native, browser enrollment, and browser session grants. Legacy
state fails closed; explicit `authenticator --upgrade-sha256` rotates OTP, clears
unclassified grants, preserves host keys/passkeys/limits, and requires rescanning
and app re-pairing. Protocol v2 and clients must upgrade together. Release `eb86123` is deployed to `ahub.clouds56.top` and this machine’s
connector/API. The authenticator CLI also shows its saved machine reference and current TOTP
with seconds remaining in trusted-terminal output, plus the full setup URI and
explicit manual settings/mismatch guidance; exports stay Base32-only.
Reference lookup reads existing keys without generating replacements.
Local SHA-256 migration preserved identities/passkeys and retired
two legacy grants; the authenticator must be rescanned. Private pre-upgrade
backups exist on each endpoint.
Enrolled host-owned passkeys authorize tab keys on their original Noise
channel; a new device needs the full machine reference to pin the host first.
Native passkey prompts use the Hub browser origin and a one-shot loopback form
callback carrying only the credential. Hub administration lives at `/admin`.
The administrator manages Hub-local namespaces and permanent `username:host`
addresses there. Exact public lookup returns routing metadata, never a Noise key;
UUID/key pins remain authoritative. Addresses survive revocation as reservations
and cannot move between hosts. Machines separates saved-machine reconnects
from code/passkey onboarding; its panel shows the machine, Hub, and encryption state.
Legacy saved UUID pins remain valid. First-device passkey access still requires a
trusted `UUID@key` or `username:host@key` reference.
Browser code delivery trusts Hub; a malicious code publisher can read decrypted
content even though the relay cannot decrypt traffic. The old installed
`client` helper remains optional compatibility, not part of the default path.
Switching cancels requests and streams; host-local
revocation is rechecked every second. Agent input requires saved host
`--allow-control`; `--allow-control=false` disables it. Seed import/export supports
user-managed synchronization; replay and five-attempt/five-minute limits persist
per host. Same-seed hosts share enrollment trust, not asymmetric identities.
SPAKE2/HKDF/HMAC pairing is experimental and unaudited; normal traffic uses Noise
IK. Hub registrations have bounded capacity/rate and persistent UUID tombstones.
The default flow defers sharing; earlier owner-signed grants and selected-session
scoping remain behind explicit CLI options. `connect --trusted-hub` retains the
older plaintext browser-through-Hub mode. See [onboarding](hub-pairing.md),
[legacy grants](hub-e2ee.md), and [Hub administration](hub.md). Remote connections
require HTTPS termination; the host API stays on loopback. Browser builds need
the `wasm32-unknown-unknown` Rust target and pinned `wasm-bindgen-cli` 0.2.126;
`pnpm build` generates bindings before bundling them.
The shared Hub viewer-route allowlist includes subscription, backward loading,
resource details, independent inspection, lease renewal, and legacy session
updates. These HTTP commands reach hosts through paired, signed-grant, and
trusted-Hub connections. Encrypted tunnel tests cover forwarding request bodies.

## Viewer core and remote API

Desktop Local calls shared Rust `crates/viewer-core` directly through Tauri.
The desktop app starts in Machines on a new installation, with This machine and
remembered remote machines. It reopens the last successful Local or Hub selection;
opening a different Hub only browses its saved machines. Navigation preferences
store Hub origins and UUIDs, while device identities remain in their existing
trust stores. Local initialization is lazy, retryable, and independent of Hub
access; failed or canceled opens keep the last successful selection. Viewer
requests and events capture their machine transport, including pagination and
compatibility fallbacks, so delayed local work cannot target a newly opened host.
Hub mode uses `hub-remote` through async Tauri commands/events; browser Hub mode
uses WebRTC with encrypted WebSocket fallback. Both select one machine and share the same
viewer command interface, host-scoped caches, cancellation, and live updates.
Standalone browser development can still connect directly to
`crates/viewer-api` over HTTP/SSE. `viewer-api` serves the compiled `apps/viewer/dist` frontend
with SPA fallback as well as authenticated `/api/v1` data routes. Build with
`pnpm --dir apps/viewer build`, then run `cargo run -p tokn-viewer-api` and open
`http://127.0.0.1:5558`. `pnpm --dir apps/viewer dev:web` starts the API on a
free loopback port and Vite with HMR, generates an access token, and prints a
fragment login link. The UI removes the token from the URL before rendering
and connects automatically; failures leave manual login available. Ctrl-C
stops both processes. For separate development processes, run the API with `--api-only`
and `pnpm --dir apps/viewer dev`; Vite provides HMR and proxies `/api` (including
SSE) to port 5558 without requiring a frontend build or CORS configuration.
See [viewer-api.md](viewer-api.md) for
the contract, authentication, origins, and SSH-tunnel setup. Remote keys must match
the discovered catalog before history can be read. Browser tokens stay in memory;
switching machines aborts old requests and clears the UI. SSE reconnects trigger
catalog/timeline refreshes. Desktop does not consume HTTP for local viewing.

Core owns the viewer domain, native index scheduler, and authoritative
snapshot/follow readers. Indexed Automatic and Local modes share those readers
and native file watches; neither starts a redundant Relay feed child. Polling
recovers missed notifications. Unindexed Automatic embeddings retain the bounded
managed-stdio feed and supervisor. External desktop mode connects to
`tokn-viewer-api snapshot --bind tcp://127.0.0.1:5557 [--native]`.
`tokn-session-relay serve` reports migration guidance. Relay remains the
provider-normalization/feed component and never serves a web UI.

Unchanged Relay status is not rebroadcast. Browser reconnects refresh progress/status
and catalog/timeline, establishing a fresh progress baseline after restarts. Event
pages expose `follow_error` while retaining last-good cards during follower retries.

## Session snapshots

The viewer now retains a one-user-turn initial history window in an
eight-session LRU, with two lower-priority activity preloads scoped by each
view's project/filter candidates. Explicit earlier loads and appended turns
remain until session eviction. View leases protect selected sessions; a 64 MiB
estimated byte target evicts unselected sessions. Older normalized records
live in temporary disk journals with compact in-memory indexes, rather than
full retained snapshots. Automatic and indexed Local share the window reader;
Local starts no Relay child. External uses additive `follow_window` requests.
Generation-scoped absolute keys keep prepends stable; legacy row APIs still
load full history. See [session cache](viewer-session-cache.md) for policy,
protocol details, and remaining transient-memory limits.
Initial loads, live refreshes, and earlier-history requests explicitly send
`direction: "backward"`; the TypeScript request union requires it for window
modes. The Rust default remains `forward` for legacy row-pagination callers.

Activity preloads keep their slots while loading or actively changing; only
settled preloads idle for 30 seconds can be replaced by another background
candidate. Per-session preload cooldowns also bound repeated work after
eviction/failure. Explicit opening bypasses that cooldown. Sidebar catalog
refreshes coalesce while a list/page request is in flight, with one trailing
refresh; changing filters still starts the new query immediately.

Metadata catalogs are shared; event snapshots load on demand. Concurrent clients
reuse one JSONL normalizer per session. Appends decode new complete lines;
replacement/truncation starts an atomic generation. OpenCode/ZCode reconcile raw
DB/WAL snapshots and reuse unchanged records/checkpoints. Unrelated changes are
silent; suffix appends preserve generations, while edits/deletions/reordering
reset them. See [snapshot protocol](relay.md#local-snapshotfollow-service).

Codex paginated rollouts can reference earlier physical files through
`session_meta.history_base`, including after a native revert. CLI and viewer
history assemble these bounded prefixes before the active segment, validating
both byte and ordinal cutoffs so reverted turns stay excluded.
Some persisted Codex rollouts repeat paginated ordinals, and forward
gaps are accepted like native Codex resume/projection. Distinct physical rows
remain visible in file order. A revert before a turn may set its exclusive
ordinal to that excluded turn across a gap; resolution verifies that next row
at the exact byte cutoff before accepting the prefix.
Ordinal validation follows the owning header's mode. Missing or regressed
ordinals report the physical file, byte offset, and ordinal details.
Repeated continuations may reference the physical segment UUID in a native
filename rather than its logical session ID; resolution verifies the filename owner
against metadata before accepting that alias. Native discovery
uses Desktop's current rollout path to avoid duplicate tasks; exported roots
collapse only a uniquely verified continuation chain. Missing or ambiguous
prefixes fail the read and preserve the last-good viewer snapshot. Linked
histories now retain a normalizer and active byte cursor, returning only new
complete rows on append. Replacement, truncation, same-size edits, and changed
history guards rebuild atomically. Local cache hits stat resolved dependency
paths instead of rediscovering history. Plain rollouts retain incremental
decoding; standalone Relay feeds remain per-file. The native append-only
assumption and measured limits are in [viewer performance](viewer-performance.md).

Automatic and Local modes use durable index queries for lists, search, trees,
and snapshot admission. The viewer-core indexer discovers provider headers and
backfills bounded titles/previews and attention in the existing SQLite index.
After the first catalog, Codex and Pi recovery scans enumerate paths and compare
stored file-revision cursors; unchanged rollouts reuse indexed headers, while
only new or modified JSONL files are opened. This preserves changes made while
the watcher was offline without repeating a cold header parse on API restart.
Known-file notifications use indexed source/session lookups; duplicate revisions
skip header reads. Provider-local scans exclude unrelated indexed sessions.
Relay hints coalesce for 200 ms of quiet, capped at one second and scheduled
deadlines, so streaming records do not each start a catalog pass.
Active Codex/Pi body indexing uses a bounded LRU of incremental activity readers,
retaining normalization cursors and reply counts instead of transcript bodies.
Recent changes bypass the historical quiet-file delay so running indicators and
final replies reach the sidebar promptly, including rollouts above 8 MiB.
Cold backfill still scales its quiet-file delay with size, up to five minutes;
cold JSONL bodies above 8 MiB stay catalog-only and preserve prior activity.
The existing 128 MiB source limit still applies. Pending-body polling
runs every five seconds and queries only unbaselined sessions and their source
rows, so deferred work does not repeatedly decode the complete session index.
Automatic snapshots no longer run a separate discovery or metadata-backfill
cache. Conversation/native payloads still come from on-demand snapshot readers,
which remain usable while the advisory Relay child starts or fails. Automatic
configuration installs the reader before publishing the mode and gives each
configuration a fresh cancellation scope; the supervisor reuses that reader. The legacy
External snapshot server retains its independent catalog and presentation cache
for explicitly configured provider roots.

Viewer mode (`automatic`/`external`/`local`), external endpoint, and optional
native inclusion persist in app config `relay.json`. Missing settings default
to Automatic with native off; legacy enabled connections migrate to External,
explicit disabled choices to Local. Automatic uses provider-owned root
resolution and environment overrides. Codex's explicitly resolved active/archive
roots retain their home-owned title/preview metadata; unrelated explicit roots
remain isolated from the active home's database and session-name index.
Local uses embedded snapshots without the Relay feed and keeps the same durable indexer.
Only External providers bypass the native index. Automatic timeline, trajectories,
and Inspector share viewer-core snapshots; External uses received snapshots. Failures/child restarts retain last-good data; explicit
mode/endpoint/native changes clear it. External services are never terminated.
Live updates refresh loaded timeline/trajectory items even when scrolled up,
retaining the reading position; New activity jumps to latest. Reading positions
persist locally for up to 200 machine/session pairs, using source-slot, type,
timestamp, and viewport offset without transcript text. Reopening loads earlier
retained turns as needed and restores the last reading line; newly appended
replies do not move it to the end. Unchanged sessions last viewed at the end
resume following. Missing/replaced anchors keep the recent window and fall back to recent context
with following paused; exhausted bookmark searches never publish the oldest
backfilled page as the opening position. Jump to latest remains available while paused and acknowledges
the newest committed page; merely restoring a historical position does not.
One scroll controller handles committed updates and asynchronous layout changes,
preserving follow intent through browser clamping and anchoring to visible child
rows inside long trajectories. Loading earlier history preserves a row position
rather than compensating for the total height (which can also grow at the end).
Same-generation detail refreshes retain rendered content until replacement,
including on retryable errors; session or snapshot-generation changes still
invalidate old detail ownership.
Refreshes are coalesced and page through one pinned snapshot. Append refreshes preserve expansion keys.
Live generation resets keep the last-good view until the replacement timeline
and expanded turn are ready, then publish them together. A surviving trajectory
slot keeps its disclosure choice while its child data is replaced. Fresh child
pages preserve the previously loaded count instead of reverting to 40 rows;
old selection/detail identities are invalidated at that commit. Failed resets
retain the view and retry as replacements. Native remains optional and bounded in
Inspector. Automatic now uses durable indexed unread tracking; External unread
tracking remains process-local. The v1 append/reset contract still
requires replacement generations for mutable OpenCode records; avoiding those
resets would need a record-update protocol and stable viewer event identities.

`tokn-session-relay` follows all six providers: Codex, Pi, OpenCode, ZCode,
WorkBuddy, and DSH. It requires an output subcommand:

```sh
tokn-session-relay zeromq --bind tcp://127.0.0.1:5556
tokn-session-relay stdout --format summary
```

All output modes and the automatic viewer child share provider-owned root
resolution, including environment overrides. Use `--codex-dir`, `--pi-dir`,
`--opencode-dir`, `--zcode-dir`, `--workbuddy-dir`, or `--dsh-dir` for explicit
roots. Codex includes active/archive roots by default. Existing sessions seed
without replay; `--poll-interval`, `--replay=<count>`, `--replay-all`, and
`--native` remain shared feed options.

ZCode reuses the OpenCode SQLite cache with distinct provider identity.
WorkBuddy and DSH expose grouped source snapshots; changed files currently
reload and normalize, while unchanged revisions are cached. WorkBuddy also
tracks catalog DB/WAL changes. DSH supports plain/concatenated Zstandard logs,
preserves packed native rows, filters inherited subagent events, and resets
follow generations when assembled output revises earlier chunks or usage.
Complete JSONL lines are required; malformed rows/compressed frames never
commit partial snapshots. The source/decoded DSH and serialized snapshot
limits are 128 MiB. Incremental WorkBuddy/DSH decoding is a remaining
performance opportunity. Shared pet activity deduplication covers their
mutable records as well as OpenCode/ZCode.

Native filesystem watching is registered between the initial file snapshot and
the EOF-seeding pass, so appends during startup remain visible. The periodic
scan is a 30-second fallback for missed notifications and roots created after
startup in standalone Relay feeds. The viewer-managed stdio child uses a
five-minute fallback because viewer-core already owns native index watches and
durable recovery; this avoids duplicate whole-history scans while idle.
Watcher notifications retain and coalesce their affected paths, so
normal updates inspect only changed files instead of rescanning every session.
The callback inbox bounds unique paths and coalesces bursts before scanning;
overflow requests a full recovery scan. Discovery skips nested directory
symlinks to avoid loops, while configured symlinked roots still work.
OpenCode/ZCode are watched non-recursively at its data directory plus the database and
SQLite WAL file; its transient SHM index is deliberately excluded because
readers can update it and feed their own watcher notifications back into the
relay. Unrelated logs, snapshots, and auth files do not trigger database work.
macOS uses the kqueue backend because FSEvents can omit these session-file
writes. Native watcher creation, registration, or runtime failure retires the
backend and reports one warning while Relay continues at its configured polling
interval (30 seconds standalone, five minutes for the viewer-managed child).
This includes macOS descriptor exhaustion on large recursive session trees.
Partial registrations are released before session discovery; failed backends
are not repeatedly recreated. The viewer's independent FSEvents index watcher
and provider-local recovery continue to refresh its snapshots.
Newly discovered or replaced files emit all normalized events beginning at the
third-most-recent message by default. `--replay=<count>` changes that window,
while `--replay-all` emits every complete record. These replay options only
apply to files discovered or replaced after startup.

`stdout` supports `--format pretty|summary|json` and defaults to `summary`.
Human-readable formats include the event timestamp, Codex Desktop project name
when available, abbreviated session id, and message id/parent when available.
Pretty output also prints the full session context before the first event
observed for each session. `--color` adds ANSI color to human output. JSON
remains colorless `RelayRecord` JSONL even when `--color` is present. JSON
flushes after each record, human output after each event; diagnostics stay on stderr.

`zeromq` binds `tcp://127.0.0.1:5556` by default. Each publication is a two-frame
ZeroMQ message:

1. `codex.<session_id>`, `pi.<session_id>`, or `opencode.<session_id>` topic
2. serialized `RelayRecord` JSON

`RelayRecord` wraps zero or more ordered `AgentEvent`s in `events`, with a
source-scoped `record_id`, `operation`, optional sibling `native` (opt-in),
source path, topic, and `SessionContext`. This replaces the single `event`
wire field; all bundled pets migrate together via `apps/shared/relay.ts`.
JSONL lines stay atomic; OpenCode messages plus parts are replacement snapshots
keyed by message ID, with removals for deleted messages in observed sessions.
See [Relay records](relay.md) for the contract and snapshot/resync limitations.
Context includes session id, optional parent/title, cwd and
start time, optional agent path/nickname/role, plus a project object. That
object carries the distinct `project_name`, `folder_name`, and
`repository_name` fields as well as the existing folder path, repository URL,
branch, commit, and compatibility `name`. For Codex sessions, `project_name`
comes from Codex Desktop's optional `.codex-global-state.json`: direct thread
assignment wins, then parent-thread assignment, then the longest matching
workspace root. Relay reloads this catalog when the file changes. Missing or
malformed Desktop metadata does not stop Relay.
`folder_name` comes from the cwd basename and `repository_name` from the Git
remote. Agent metadata comes only from the first session header, including its
thread-spawn source when needed. Missing paths remain null for root and
subagent sessions; the relay does not derive `/root`. Title is never invented
when the provider file does not contain one.

Pretty session context shows `agent_path` only when it is present and not
`/root`. Summary lines include the same paths as `agent=<path>`. JSON preserves
the recorded value unchanged, including null or an explicit `/root`.

The relay publishes all normalized events, including reasoning, tool calls,
errors, lifecycle events, and unknown provider-native shapes. It buffers partial
JSONL records, discovers newly created files, handles truncation/replacement,
and combines native filesystem notifications with a periodic rescan. OpenCode
session summaries are cached, so a database notification reloads only new or
changed sessions on the normal path; if no summaries change, it reloads current
sessions once to catch in-place message/part edits. A simultaneous summary
change in another session can mask such an edit until a later fallback scan.
New sessions use the replay window, while changed message records
republish their whole normalized batch. JSONL updates decode each appended
complete line once. The viewer uses the snapshot service above when configured;
the publication modes retain their existing best-effort behavior.
Recoverable scan failures are reported per file or provider without dropping
successful records from that pass; failed OpenCode sessions remain eligible for
retry. OpenCode/ZCode catalog refresh counts messages with one grouped query
instead of a query per session. Managed hints omit transcript/native bodies,
including source records larger than the pipe's 8 MiB frame limit.
Watcher paths now accumulate in a bounded inbox; overflow requests a complete
recovery scan. See [Relay I/O measurements](relay-performance.md) for the
first baseline comparison and benchmark driver. A later
[I/O reduction pass](relay-io-reduction.md) reuses one SQLite connection
for an OpenCode/ZCode catalog and its selected sessions, removing repeated
database opens and page reads, including during the all-session fallback for
timestamp-free part edits. Full record loads reuse messages for counts and
untitled previews. JSONL scans now use one metadata lookup per tracked file.
Codex/Pi directory notifications avoid a second metadata probe for discovered
rollouts, and startup reuses each header file handle to check only the final
byte of newline-terminated files before following at EOF. Stateful Codex feeds
silently restore the original prefix once on the first append, retaining
pre-start question/tool/compaction correlation and current cwd. Idle startup
stays cheap; the first active append pays for that prefix read, and later
appends remain incremental. Mutable pet activity matching compares event
occurrence counts rather than array slots, so shifted context events do not
replay unchanged replies. Metadata errors
retain file state for retry.
The database is opened read-only with WAL visibility and an immutable fallback;
the relay never runs provider migrations.

The reusable relay loop lives in the library as `SessionRelay`. `RelayConfig`
controls provider roots, native inclusion, new-file replay, and the periodic recovery interval.
Library consumers call `next_update().await`; notification and scan failures
that can be retried are returned as warnings alongside `TailUpdate.records`.

## Discord Pet

`apps/discord-pet` is a Bun/TypeScript application that consumes Relay JSONL
and mirrors root Codex and Pi conversations into Discord. By default it runs
an incremental workspace Relay build and spawns
`tokn-session-relay stdout --format json`; `--stdin` consumes an existing
pipeline instead. It reads `~/.tokn/pet/discord.yaml`, validates the bot and
configured guild/channel through Discord's REST API, and creates one public
thread per root session. It publishes root user messages and final assistant
messages only. Commentary, reasoning, tools, and child sessions are ignored.

The YAML contains `bot_token`, `guild_id`, and `channel_id`. Thread mappings are
persisted beside it, so later turns continue in the same thread after a process
restart. The default config uses `discord-state.json`; named configs derive
distinct state filenames. Discord embeds are split against the platform's
UTF-16 length accounting, mentions are disabled, transient requests and rate
limits are retried, and the token is never logged. The bot needs no privileged
intents.

`bun run login` from `apps/discord-pet` is the preferred configuration path. It
first walks through Guild Install and waits for the bot to appear in the server,
then prints where to obtain the bot token and Discord IDs. It hides token input,
asks before replacing an existing file, validates that the authenticated bot
can access the channel and that the channel belongs to the configured guild,
then writes the YAML with owner-only permissions. The validated identity is
also recorded as optional `bot_username`; configs created before that field
remain valid. `--config` overrides the destination.

Existing files start at their snapshotted EOF. Newly discovered files use the
relay's three-message replay window so the first prompt is not missed. See
`apps/discord-pet/README.md` for setup and permissions.

## Pet Supervisor

`apps/pet` is the high-level Bun supervisor. It owns one Relay stream, evaluates
declarative fan-out rules, and delivers matching `RelayEvent`s to bounded,
serial queues for in-process async worker objects. Downstream workers are not
subprocesses. The initial worker types are terminal and Discord; multiple named
Discord workers may each reference a different credential/channel YAML and use
independent persistent thread maps.

The checked-in `pet.example.yaml` sends the complete Relay stream to terminal
so its state inference retains tool, reasoning, error, and lifecycle context.
It sends root user messages and final assistant messages to `discord_volty`
only when `SessionContext.project.repository_name` matches the case-insensitive
glob `volty*`. Rules fan out, AND fields inside one `when`, OR values inside
arrays, deduplicate targets, and drop events with no match.

Workers expose `start`, `handle`, and `stop`. A failure handling one event is
reported without poisoning later queue work. The default per-worker capacity is
256; full queues backpressure Relay consumption rather than dropping events.
Terminal `q`/Escape aborts the shared source and shuts every worker down.

## Terminal Pet

`apps/terminal-pet` is a Bun/TypeScript prototype that consumes Relay JSONL and
shows one graphical terminal companion beside a multi-session roster. It runs
an incremental workspace Relay build, then spawns
`tokn-session-relay stdout --format json`, or accepts an existing stream with
`--stdin`.

The reducer keeps a session graph keyed by Relay topic. Root tasks are rendered
as project-labelled families, with active and recent subagents nested beneath
them by `parent_session_id`. Provisional child rows appear as soon as a parent
reports `agent_activity.started` and reconcile when the child's own Relay topic
arrives. Child urgency bubbles into the root summary while automatic focus
stays on the actionable child. `interacted` is only an annotation;
`interrupted` becomes a recent Interrupted outcome rather than falsely showing
Blocked. Stable agent activity is deduplicated by provider and event id, and
provider occurrence times prevent replayed old activity from looking current.

Within each family, state still uses
`needs_input > blocked > ready > running > idle`, followed by idle sessions
that were inferred Ready or Interrupted in the last five minutes. Up/Down or
`j`/`k` selects another session by topic, `a` restores automatic focus, and
`Enter` opens a composer for the focused session. A second `Enter` submits the
message and `Escape` cancels it. Pi input records the observed Relay path,
resolves the live bridge descriptor, and submits an `auto` request to the
owning Pi process's Unix socket. Idle input starts immediately and busy input
enters Pi's follow-up queue. Root Codex input first uses the Desktop IPC owner.
A missing/refused IPC endpoint or explicit `no-client-found` response falls
back to one `codex exec resume` turn with the prompt on stdin; ambiguous IPC
failures never fall back. The CLI route restores the latest model and reasoning
effort from the rollout, and `TOKN_CODEX_BIN` overrides executable discovery.
Codex subagents and unobserved sessions remain read-only. `c` acknowledges
the selected notification. Responsive text rows keep concurrent
and recent sessions visible; roster rows use the state glyph plus a compact
provider badge instead of repeating the state label. The renderer uses overflow
windows around a manual selection so it cannot disappear off-screen. Root
labels prefer `project_name`, then folder name, repository name, and the legacy
inferred name. Child labels prefer agent nickname, then agent path. Wide
terminals show the art and roster side by side, while narrow terminals become
roster-only.

States currently derive from normalized messages, reasoning, tool calls,
errors, goals, and preserved input-request events. Codex task start/complete
and abort records now normalize into turn lifecycle, but the reducer still uses
leases and a short ready debounce instead of claiming authoritative runtime
status. The recent-Ready roster is explicitly an observed-run heuristic, not an
authoritative completion log. It includes only work seen while the pet is
running because Relay seeds existing session files from their snapshotted EOF.
Codex commentary messages count as progress rather than completion now that the
normalized message delivery is preserved.

Provider input bridges live under the top-level `extensions/` directory. The
Pi bridge is an opt-in extension that starts a process-instance Unix socket for
interactive sessions and publishes a session-scoped descriptor in a private
runtime directory. Requests are bound to both the Pi session and live process
generation. Idle input starts immediately; busy input defaults to Pi's
follow-up queue, with explicit steering available. Request IDs are deduplicated
within the bridge process. Admission only means Pi's live runtime accepted the
input; Relay observing the resulting user message remains the authoritative
confirmation. Terminal-pet consumes this descriptor and shares the bridge wire
contract from `extensions/pi/lib/`; it does not fall back to a second Pi
process when the bridge is unavailable.

`extensions/codex/` now contains an isolated experiment for Codex App's private
length-prefixed JSON IPC router. The client requires an explicit IPC endpoint:
a Unix socket on macOS/Unix or a local Windows named pipe. Platform discovery
maps `$CODEX_HOME/ipc/ipc.sock` on Unix and `\\.\pipe\codex-ipc` on Windows.
The client sends the observed version-2 `thread-follower-start-turn` request
(Desktop build 26.901.41123), using the rollout thread id as `conversationId`
and `turnStart.request.threadId`. The version-1 `turnStartParams` shape no longer
matches that build. Requests allow the router's ten-second owner-discovery
window to complete. An isolated fake desktop
router exercises client initialization, owner discovery, forwarding, and the
successful response path over the native transport on Linux, macOS, and Windows
CI. Fake-router error responses are tested on Unix only because Bun 1.3.13 does
not flush those server-side named-pipe responses on Windows. Its
lab owner can forward accepted input to a standalone `codex app-server` under a
temporary `CODEX_HOME`; the local smoke passes with `deepseek-v4-flash` at
`http://localhost:4141/v1`. These are protocol and transport regression tests,
not general compatibility guarantees for future Codex App builds. Real IPC
delivery has been observed in persisted rollouts; that alone does not validate
Desktop rendering, which also requires `text_elements` on text inputs.
Model and effort overrides require a version-1
`thread-follower-update-thread-settings` request before start-turn; inline
start-turn fields are silently replaced by the owning window's current settings.
The update is retained for subsequent turns. Terminal Pet uses the client for
root Codex session input and falls back to non-interactive CLI resume only when
the endpoint is unavailable or no App window owns the session. The desktop IPC
contract is private and must remain a fail-closed
compatibility transport rather than being treated as a supported app-server
API.

Rendering uses Kitty graphics where available, the Kitty local-file protocol
in iTerm2 3.6+, and a truecolor ANSI half-block fallback. Wide mode includes a
focused-session detail panel with the current activity kind/detail, age, and
working directory when available; narrow modes preserve the compact roster.
`bun run dev` cycles through states for art iteration; `bun run check` runs
strict TypeScript and Bun tests. The checked-in Hachiware frames are explicitly
prototype-only fan art and must be replaced before publishing or distributing
the project.

## Desktop Session Viewer

The viewer switches to a native modal session drawer at 860px, preserving the
mounted sidebar across resizing, containing keyboard focus, and focusing search
on open. Provider filters wrap and include a reset action. Narrow conversation
headers put the lifecycle filter on its own row. Both desktop and browser reserve
remaining height for the timeline. Relay controls live in the bottom status bar
and open a floating Connection panel; index status is also a clickable panel
trigger. Browser, Hub and paired-host connections use the same bottom placement,
with host switching and sign-out actions in their Connection panel. Panels dismiss
on Escape, outside click, or keyboard focus leaving.
Session rows show a title, distinct preview, provider and recency rather than
provider-letter avatars and IDs; full IDs remain in row tooltips and labels.
The composer starts at one line, expands on focus or a draft, grows within a
viewport bound, and keeps Send inside the input. Drafts and delivery diagnostics
retain the existing per-session safeguards.

`apps/viewer` is a Tauri 2/React desktop and browser viewer for historical Pi,
Codex, OpenCode, ZCode, WorkBuddy, and DSH sessions. It aggregates root sessions
into one searchable, provider-filterable sidebar, lazily expands known
subagents into a tree, renders the selected session's normalized events as a
conversation, and keeps reasoning, tools, metadata, errors, and unknown events
inspectable. A failure in one provider is
reported without preventing the other providers from loading.

The conversation footer sends multiline messages through native Rust live-input
transports in `viewer-core`, shared by Tauri and authenticated HTTP commands.
Root Codex tasks use the owning Desktop IPC client without a CLI fallback;
text inputs include `text_elements: []`, which the Desktop renderer requires
even though app-server accepts its omission. Without it, a turn can execute and
persist successfully while its optimistic Desktop view crashes.
Pi uses its exact live session's opt-in input bridge, starting idle turns or
queueing busy follow-ups. Other providers, Codex subagents, and External snapshot
mode are unavailable. Catalog membership and a fresh source header validate
every target. Availability checks do not submit input. Drafts stay in memory
per session; Cmd/Ctrl+Enter sends. Admission clears the draft, while uncertain
delivery retains it and requires explicit editing before another send; the
footer preserves backend and connection diagnostics alongside that notice. There
is no optimistic transcript or automatic resend. Acceptance triggers bounded
history refreshes immediately and at 1/3/8/20 seconds, covering delayed provider
writes or missed live notifications while preserving loaded history and reading
position. Refreshes coalesce with live reads and stop on session/machine changes.
The bounded in-memory request cache deduplicates UUIDs and guards the resolved
runtime owner against parallel
sends, including when the HTTP caller disconnects. Limits are 16,384 Unicode
characters and Pi's 32 KiB encoded frame. Markdown whitespace is preserved.

The conversation keeps user prompts and final assistant replies visible. A
contiguous stretch of intermediate assistant progress and non-message activity
with substantive work (including a non-final assistant message) is represented
by a work trajectory item; metadata-only stretches remain flat.
Terminal bookkeeping written after a final reply also remains chronological,
inspectable flat rows rather than creating a second `Worked` item.
Observed turn starts show `Working for …` with a ticking elapsed time and
auto-expand while following if no older section is open. The latest working
turn auto-collapses once when it transitions to `Worked` while following.
Already finished turns reopened by the reader stay open when newer items or
turns arrive, including generation resets. Manual collapses of the same turn
survive refreshes and pending requests.
Jump to latest explicitly opens current work. Without reliable
turn signals the label is neutral `Work`, not a claim of runtime activity.
Duration uses provider timestamps, never session-file metadata.
Expanding a trajectory lazily loads its contained normal event cards
through a separately bounded page, preserving per-event inspection and verified
direct-child delegation navigation. A trajectory is a viewer projection, not a
provider-authoritative turn. Its identity uses the earliest normalized source
event in the folded run, so it remains expandable when more work or a matching
tool result is appended after the outer timeline loads.

The conversation header's **Hide lifecycle** toggle filters rendered rows in
both the main timeline and loaded trajectory pages. It defaults off and stays
selected while switching sessions in the mounted viewer. Viewer-core supplies
`EventSummary.is_bookkeeping` from validated event kinds and a conservative
provider-record allowlist; missing flags from older backends keep rows visible.
The active button reads **Show lifecycle**. Intermediate usage is classified
against the full snapshot before paging, keeping the last accounting of each
usage kind at a final reply or turn completion. Active-turn usage is hidden;
orphan or conflicting-turn usage remains visible. Accounting scopes are never
combined. Successful Codex completion markers can hide their final-message echo;
substantive content, unknowns, errors, and exceptional lifecycle outcomes remain
visible. The filter never changes stored history, cursor
windows, selection/detail ownership, or underlying totals. Filtered empty
ranges retain their pagination controls; hidden counts cover loaded rows only.

Visible user and assistant messages, expanded reasoning, and readable inspector
content render GitHub-flavored Markdown. Raw HTML is disabled, images become
inert placeholders, and links cannot navigate the WebView.

Mac desktop responses have **Translate → 简体中文** using Apple Translation
(macOS 15+). A small SwiftUI host in the existing window owns the native session
and Apple's language-download UI; async Tauri commands bridge bounded prose
batches and cancellation. Browser responses use the browser's local Translator
and Language Detector APIs when available, with feature/language-pair checks,
download progress, and a Continue translation action when model preparation
requires a fresh user gesture. Unsupported or insecure browsers show a disabled
action with an explanation. Browser translation needs HTTPS or a trustworthy
loopback origin; it does not call a translation endpoint in viewer-api.
Other desktop platforms still lack a native translation engine.
The frontend loads complete bounded message detail, translates Markdown text
nodes, and retains code, URLs, and structure. Formatting boundaries limit
translation context. Originals remain available with a toggle and in Inspector;
results live only in the mounted card, with source-change invalidation. Both
engines cancel pending work and release job resources on completion or unmount. Nested
trajectory responses use the same component. Mac builds require Xcode 16+ and a
macOS 15+ SDK for the Swift bridge; other platforms skip it.
Truncated assistant previews include a full-text revision in page and detail
responses. Translation survives an unchanged page refresh and rejects detail
from a changed response before starting the engine. Older servers retain
conservative refresh invalidation. The Markdown translator recovers text spans
around GFM `www.` autolinks and keeps link destinations out of translation.

Known code-execution, terminal, shell, file, search, web, and task events have
source-neutral tool cards with compact command, path, query, status, and change
facts. Providers still contribute an append-only fact stream, but the shared
core `ToolOperationAssembler` turns safely correlated invocation, progress, and
result records into one logical historical operation. The viewer consequently
shows one card, with a derived `pending`/`running`/`completed`/`failed` state;
its normalized Inspector view contains the semantic input and final output,
while the Native tab shows any provider-native envelopes the adapter retained
from contributing records. Completed cards sit at their terminal source record
so intervening assistant activity remains chronological. Missing or overlapping
call IDs deliberately remain separate rather than being guessed.

Expanding a tool loads a bounded plain-text or JSON output preview without
opening the inspector; the inspector remains available as a separate action.
Semantic results with a readable `text` field display that text directly rather
than surrounding response metadata. Inline output retains at most 64 KiB with
both its head and tail visible and never renders provider output as Markdown or
HTML. Codex Code Mode wrappers are decoded only for the generated single-call
`write_stdin` and `exec_command` shapes; arbitrary JavaScript remains a raw
Code Execution operation.

Usage events have dedicated token cards. The card labels model-call,
operation-total, and replaceable session-snapshot scopes so users do not sum
unrelated rows; cache counts are already included in input, while reasoning and
total counters remain provider-reported. Usage-card `u64` counters cross the
viewer IPC as decimal strings to avoid JavaScript precision loss. Reasoning
cards use a sanitized one-line preview and load readable Markdown lazily when
expanded.
Encrypted and provider-redacted reasoning remain opaque in the timeline; a
redaction is visible metadata rather than a hidden event.

The Tauri backend calls `tokn-session-client`, `tokn-session-core`, and
`tokn-session-render` directly from async commands for local providers, and uses
received Relay snapshots for covered providers. It does not parse CLI output.
The frontend receives source-neutral snake-case
DTOs with opaque, source-aware session keys. Session and event pages keep IPC
responses bounded, including tool-card command and query fields, while expanded
native event detail and inline tool output are fetched lazily and hidden Pi
content stays redacted.

For Automatic and Local modes, viewer-core owns a SQLite index at
`~/.tokn/sessions/index.sqlite`. It stores
opaque source checkpoints; source/session identity and paths; bounded sidebar
metadata (title, preview, cwd, timestamps, parent, and agent labels); and
opaque attention markers/revisions. It never stores normalized event records,
provider-native payloads, reasoning, tool input/output, or full message bodies.
Title and preview are retained presentation text, and a preview can derive from
a user prompt, so index-only does not mean zero textual metadata. A successful
stable header pass commits a provider catalog sentinel immediately, so sidebar,
search, and tree IPC read only durable index rows without waiting for every
session body. A provider without that sentinel is reported as indexing; the UI
never falls back to native headers or synchronously hydrates missing metadata.
The background body pass backfills its bounded title/preview metadata together
with attention, while later blank lightweight headers preserve that backfill
until a successful body refresh replaces it. The initial viewer leaves its main
pane unselected; an explicit selection opens its conversation snapshot while
the background indexer separately reads bodies for bounded metadata and attention. The selected session's normalized `total_events` arrives
with its first event page. The existing CLI continues to use the counted
`list_sessions` API. The scheduler emits its sidebar refresh as soon as that
catalog transaction commits, before later bounded body work.

One viewer-core indexer owns each database through an OS-held sidecar lock
(`index.sqlite.indexer.lock`). Other viewer/API processes query SQLite WAL, poll
its data version once per second, and show `Using shared session index`. They
never scan providers until acquiring the released lock. The lease survives
async task cancellation until any outstanding blocking scan finishes, and the
OS releases it after a process exits. Explicit retries append a request generation
to `index.sqlite.indexer.retry`, which the owner checks without consuming another
process's request. Do not delete these sidecars while viewers are running.
Processes sharing an index must use the same provider-root configuration; use
separate `--index-path` values for different source sets. Detailed active-provider
progress and errors currently belong to the owner; followers expose shared
ownership and durable queue counts, not the owner's live progress snapshot.
External mode releases the native lease and continues querying its chosen server.

Relay records are bounded advisory indexer hints: Codex/Pi target changed paths;
other providers request provider-local discovery. A lagged Relay hint receiver drops
its historical backlog instead of escalating it to a global catalog; native file
watchers and periodic recovery cover missed feed events.

The viewer also keeps a process-local operational snapshot for its one index
scheduler. It contains only a monotonic string revision, provider identities
and counts, the remaining body queue, and sanitized scheduler failure
categories; it never reads provider storage or caches historical bodies.
`session-index-progress` is deliberately separate from the durable
`session-index-changed` sidebar signal, so an active-provider or queue-count
update does not reload the session list. The React client subscribes before it
reads the snapshot and ignores an older revision that arrives afterward.

A persistent bottom status bar describes the work in plain language: `Finding
sessions`, `Checking for changes`, `Loading details`, `Queued`, or `Up to date`.
`Finding sessions` is reserved for provider discovery; a watcher-led check of
an already indexed Codex or Pi rollout file says `Checking for changes`.
Its bell opens a
non-modal operational center with every provider's state, readable warnings,
and a durable `completed / total` detail count for its current catalog
baseline. Only the provider owning the current bounded body job is shown as
loading; providers with work waiting behind it are queued. The fraction derives
from staged source-cursor generations plus unbaselined rows, so it survives a
viewer restart and resets cleanly when a catalog establishes fresh body work.
The normal progress label is intentionally not a live announcement on every
count tick. Retrying queues a coalesced wake for the existing scheduler and
forces its next pass to be a catalog pass; it never starts a second competing
provider scan.

`sources` remains the normalized owner of `provider` and `source_key`.
`indexed_sessions` is a read-only SQLite view that joins those fields onto every
session row for diagnostics and future read-only consumers.

After cataloging, reconciliation backfills bodies, bounded presentation
metadata, and attention in newest-first batches. Codex and Pi session roots use
the cross-platform `notify` watcher: an ordinary data or metadata change to an
already indexed JSONL file reads only that file's header and stages only its
body source. Notifications coalesce for 200 ms (with a one-second maximum
batch age), so one appended record does not produce several reads. A created
JSONL path receives that same identity-checked direct read; it commits only if
it is the existing one-to-one source, while a new, moved, replaced, or unknown
source immediately requests a complete catalog. A direct header read that races
a write retries that same source after one second, up to three times, before it
uses the full recovery path. Remove, rename, directory, overflow/rescan, and
source-identity events deliberately use complete cataloging, preserving atomic
relocation and unread handling. Each existing rollout root also keeps a small
non-recursive parent watch, so a later root removal or rename reaches that same
recovery path instead of stranding its recursive watcher. A Notify backend
error first runs one complete recovery catalog, then disables the native watcher
and falls back to the provider-local cadence instead of repeatedly rescanning
on every error.

The full all-provider header catalog now runs at startup, on an explicit retry,
for those structural/recovery cases, and every five minutes as a safety sweep;
it is no longer the normal steady-state update mechanism for active Codex/Pi
rollouts. macOS waits two background-only seconds after FSEvents registration
before the first catalog, so the first snapshot follows the watcher stream's
asynchronous startup; an existing SQLite sidebar remains immediately usable.
OpenCode, ZCode, WorkBuddy, and DSH retain a ten-second *provider-local*
catalog cadence, and a Codex/Pi root with no working native registration joins
that subset. This preserves their update latency without repeatedly discovering
large watched rollout trees. While eligible rows remain unbaselined, a one-second
body-only pass advances the next batch without rediscovering the whole provider.
Active Codex/Pi JSONL uses retained incremental readers; cold backfill keeps its
quiet-period and size limits. A body failure remains visible but is not retried every second;
a source-generation change or explicit retry makes it eligible again. When no
body work is eligible, the one-second lease tick checks only SQLite's cheap data
version and does not enumerate the index.
A membership or source-revision race, provider catalog warning, or transient
refresh failure keeps the prior rows visible and makes up to two one-second
retries of the relevant catalog scope before returning to its normal cadence;
mutable title, preview, and modification-time changes are intentionally not
catalog races. A newly cataloged row never
shows a dot before its body finishes, except a relocated row that retains an
existing unread state while its new path is validated. The initial catalog
establishes no unread attention; a session first discovered after that catalog
can become unread only after its body confirms completed, unhidden final
assistant replies. User messages, commentary, tools, reasoning, and metadata
never increase the unread count. OpenCode and ZCode mutable assistant text stays
in progress until its native message completes. Revisions advance by the number of new final
messages, not by refresh count; multipart replies with the same message identity
count once. History reductions retire removed unread replies without rewinding
revisions. Older user-plus-assistant markers establish a quiet final-only baseline.
The compact marker also stores running state, derived from turn boundaries and
work events; final replies, turn completion/interruption, and provider errors
stop it. Known running rows reconcile even when a cold JSONL body would normally
be deferred or catalog-only; v3 running markers receive a one-time body recheck
when upgrading to v4, including already-matching source cursors. Unrelated cold
histories retain their lazy body policy. Semantic updates use this same
session-wide activity projection, independent of visible groups or history
windows. Compact notifications include canonical ancestors so collapsed running
indicators clear when the last active descendant stops. Unknown activity is not
inferred from file modification alone.
Question attention is independent of read acknowledgement and is session-local;
it does not increase final-reply unread counts or propagate to ancestors.
Outstanding questions have a separate badge and navigable session notices.
Among activity/unread indicators, a running circle takes precedence;
otherwise one unread reply is a dot, multiple replies show a count, and read
sessions have no indicator. Unread counts belong only to the session itself;
subagent replies never contribute to an ancestor’s unread state or count.
Canonical ancestors still aggregate descendant running state.
A newest event page acknowledges only its captured revision after React commits
it and the view is following latest; scrolling up retains unread counts.
A downward scroll to the physical end clears the New activity action and
acknowledges that page, even when the final wheel gesture has no scroll delta.
Layout clamping alone does not resume following or mark the page read.
Successful body refreshes separately name `updated_session_keys`, letting the selected timeline
refresh tool/progress/lifecycle changes without creating unread attention.
Unrelated indexing does not reload the conversation.

An index cursor is an opaque source revision, not a byte offset or event-page
cursor. File-backed sources use a metadata fingerprint; OpenCode uses its
database and WAL, deliberately excluding its reader-writable SHM file. A
header-only metadata change is written without replaying an unchanged session
body, preserving its attention state. The catalog pass and its later body pass
both carry optimistic source-cursor preconditions: the body result must still
match the catalog snapshot before it can commit. A checkpoint contains both the
provider-owned cursor and an index-owned mutation generation, so an unchanged
provider cursor cannot let a stale catalog/body write overwrite newer metadata.
Catalog source replacements commit atomically, and these staged checks prevent
concurrent viewers from overwriting a newer catalog or attention snapshot with
stale work. A failed or racing scan retains the last good index rows; actual
provider read failures report an isolated warning, while ordinary catalog races
retry quietly. Warning changes refresh the sidebar too. When a session file
moves between sources, a staged target preserves any existing unread state and
body-derived title/preview fallback until its body validation succeeds, so a
transient archive read failure cannot clear a dot or an established label.

Session rows prefer a non-placeholder provider title, then the first meaningful
user-prompt preview, then a child agent nickname, role, or path, and finally
`Untitled session`. The shortened session id is shown separately and the full
id remains available through the row's accessible label and tooltip. Titles,
previews, and agent labels are normalized to bounded, single-line text before
IPC; ANSI escapes, control characters, and bidirectional override or isolate
marks are removed. Root search matches title and preview as well as session id,
project, cwd, and agent identity.

Event paging currently bounds the data sent across IPC, not all source-reader
memory: a provider parser may still load the full selected session before
producing a page. A one-entry normalized-session cache avoids reparsing between
page and inspector requests and invalidates on source revision changes,
including OpenCode's SQLite WAL/SHM sidecars. This cache is separate from the
durable sidebar index checkpoint, which tracks OpenCode's database/WAL only.
Periodic sidebar reconciliation remains the fallback. Native incremental
cataloging currently covers only Codex and Pi JSONL rollouts; OpenCode, ZCode,
WorkBuddy, and DSH receive a ten-second provider-local header catalog until
they gain precise invalidation paths. When all Codex roots are watched, Codex
`state_5.sqlite`-only presentation changes still wait for the all-provider
recovery catalog (or an explicit Retry); if native Codex watching is unavailable,
the ten-second provider-local fallback reads that metadata too.
Each normalized and provider-native inspector representation is capped at 512 KiB before IPC;
oversized values are replaced by structured JSON truncation metadata. A full,
uncapped export path is not implemented yet.

Visible message previews retain up to 16 KiB characters so Markdown blocks can
render directly in the timeline. Other event summaries and hidden/redacted
content retain the compact 500-character budget. Longer messages can still be
loaded through the normalized inspector detail, subject to its 512 KiB
representation cap.

The root roster stays paged and does not eagerly serialize every descendant.
Each expandable session uses a separate bounded, metadata-only direct-child
query. Parent-child edges are resolved within one provider after duplicate IDs
are canonicalized by newest provider timestamp (then path); orphaned and cyclic
records remain visible as roots instead of disappearing. `agent_path`, nickname,
and role cross the viewer boundary as sanitized bounded labels. A parent
timeline shows a historical delegation card only when an `agent_activity` target
id resolves to its canonical, same-provider direct child; unknown, ambiguous,
cross-provider, or non-child targets remain visible but are not navigable.
Opening that card materializes the verified child in the sidebar and selects its
independent timeline. Child searches are not yet included in root search, and
historical headers or activity cards do not claim live subagent status.
Communication cards also resolve senders within the same canonical relation
tree, including parent and sibling senders. A path must match uniquely; explicit
session IDs must agree with any supplied path. Summary pages contain presence
flags only; message expansion and inspector content load the readable body.

Codex normalization follows the first session header's `history_mode`. Legacy
rollouts keep their response-item and legacy-event projection, while paginated
rollouts use canonical `item_started`/`item_completed` records and suppress
duplicate raw response records. Raw reasoning remains authoritative so its
encrypted content is retained. Incoming `agent_message` response records are
also preserved because they have no canonical completed-item duplicate.
Every current Codex turn-item and extension kind
has an explicit disposition; malformed and future shapes remain visible as
subtype-specific unknown events.

## Provider Sources

- Pi session roots resolve in this order: `--session-dir`,
  `$PI_CODING_AGENT_SESSION_DIR`, `$PI_CODING_AGENT_DIR/sessions`, then the
  platform home directory's `.pi/agent/sessions`. Pi environment overrides
  expand a leading `~` like the upstream agent.
- DSH reads `session.jsonl` and `session.jsonl.zstd` recursively from
  `$DSH_HOME/sessions` or `~/.dsh/sessions`, with `--session-dir` overriding it.
  `show` accepts paths and exact/unambiguous-prefix IDs; `browse` and tree scope
  also work. Compressed files support concatenated frames. Invalid JSON,
  corrupt/truncated frames, invalid packed runs, and unsupported format versions
  are reported, never repaired. Relay follows these same logs; DSH SQLite,
  create/append, and input are not implemented.
- Codex reads JSONL from `sessions` and `archived_sessions` below a valid,
  non-empty `$CODEX_HOME`, falling back to the platform home directory's
  `.codex`; `--session-dir` still overrides discovery directly.
- OpenCode uses `--session-dir` first, then `$OPENCODE_DB`; absolute database
  overrides are used directly and relative overrides resolve below the
  OpenCode data directory. That data directory is `$XDG_DATA_HOME/opencode`,
  falling back to the upstream home-directory `.local/share/opencode` path.
  In-memory databases are rejected because historical discovery requires
  persisted sessions.
- OpenCode opens its database with a WAL-aware read-only SQLite URI so active WAL data is visible without application writes; if that cannot open, it falls back to immutable read-only mode. Viewing sessions never runs migrations.
- OpenCode validates the required `session`, `message`, and `part` tables and columns, then detects optional session columns from the actual SQLite schema.
- OpenCode accepts schemas both with and without the optional `session.model` column; it never runs migrations against the user database.
- ZCode reads its extended OpenCode-compatible SQLite store from
  `--session-dir`, `$ZCODE_STORAGE_DIR/cli/db/db.sqlite`, or
  `~/.zcode/cli/db/db.sqlite`. Explicit directories may be either the storage
  root or the directory containing `db.sqlite`. The database is opened
  read-only with WAL visibility and the same immutable fallback as OpenCode.
  ZCode message semantics preserve model-only records as hidden provenance,
  reasoning signatures remain available, and known runtime model/shell,
  checkpoint, and input-resolution entries normalize into provider or metadata
  events. Metadata entries retain their native envelopes, while future runtime
  entry kinds remain visible as unknown events.
- WorkBuddy reads the read-only `workbuddy.db` catalog and per-session JSONL
  histories below `projects`. `--session-dir` overrides discovery;
  `$WORKBUDDY_CONFIG_DIR`, `$CODEBUDDY_CONFIG_DIR`, and then
  `~/.workbuddy-ai` provide the default root.
  Catalog metadata is merged with discovered JSONL so headless or otherwise
  uncataloged histories remain listable. Session paths point to the JSONL body,
  and explicit JSONL paths work for `show`. The observed schema has message
  ancestry but no session-level parent relationship, so tree scope treats each
  WorkBuddy history as a leaf. Loading never writes catalog rows or JSONL
  histories; a catalog without a live WAL is opened immutable, while a
  non-empty WAL is read through SQLite's read-only WAL-aware mode.

`SessionRef` and `SessionHeader` carry optional `title` and `preview` fields in
addition to relationship and agent identity. Successful background body loads
can persist their bounded reference title/preview in the index when header-only
discovery lacks those fields; no selected-session load writes presentation data
back. Codex uses a read-only, fail-soft
lookup of the optional private `state_5.sqlite` title metadata, correlated by
both thread id and rollout path, with legacy `session_index.jsonl` names and a
bounded rollout scan as fallbacks. Its state location follows `config.toml`
`sqlite_home`, `CODEX_SQLITE_HOME`, then Codex home. Pi takes the latest
`session_info.name` and first meaningful user prompt. DSH takes the latest valid
`session/title` event and first direct user message. OpenCode reads its optional
session title column, filters strict generated `New session` and `Child session`
placeholders, and lazily queries the first user text or subtask when needed.
ZCode uses the same title behavior.
WorkBuddy prefers its catalog title, then the latest valid `ai-title` record,
and derives previews from the first user text when body hydration is requested.

Codex Desktop can copy a root thread's state-db title, preview, and first user
message into each subagent row. The Codex adapter intentionally ignores those
private-state presentation fields for sessions with `parent_thread_id`; the
viewer then labels the child from its nickname, role, or agent path. The
background body pass can later backfill the child's own bounded presentation
metadata into the durable index.

Relationship metadata still includes optional `parent_session_id`,
`agent_path`, `agent_nickname`, and `agent_role`. Codex takes owning identity
only from the first valid `session_meta`, because subagent rollouts can contain
copied parent headers. Pi resolves `parentSession` paths to parent IDs, and
OpenCode and ZCode use their session `parent_id`.

For Codex, only `parent_thread_id` establishes a subagent relationship.
`forked_from_id` records that a user fork was created from another thread, but
the fork remains a separate root session.

`tokn-session show` defaults to `--scope self`. `--scope tree` discovers
descendants, prints a compact hierarchy, and then renders every session in a
separate section. Tree output is currently pretty-only; self-scoped JSONL keeps
the existing event-only format. Tree discovery uses header-only relationship
scans, including the provider's global roots when the selected session is an
explicit file path. Historical Codex thread-spawn rollouts omit inherited parent
bootstrap history and begin at the explicit trigger-turn boundary. Other
parented Codex sessions, such as guardian work, retain their body from the start.
If an older thread-spawn rollout has no trustworthy boundary, pretty output
warns that its body is unavailable and JSONL output fails instead of attributing
parent work to the child. Tree sections remain separate rather than merging
timestamps into a single timeline.

## Event IR Status

The shared IR is `AgentEvent`.

Persisted Codex rollout wire types live in the standalone
`tokn-codex-protocol` crate. The crate is intentionally decode-oriented:
stable session, response, agent-communication, turn-context, and world-state
fields are typed; volatile subtrees remain JSON values; and unknown tags retain
their original payloads. It does not mirror Codex's internal Rust API.

`tokn-session-codex` uses those local wire types directly. The published
`codex-protocol` dependency is no longer part of the workspace.

Persisted Pi session wire types similarly live in `tokn-pi-protocol`.
Top-level entries, nested message roles, and content blocks all fall back to
lossless unknown values when Pi adds or changes a shape. `tokn-session-pi`
owns their normalization into `AgentEvent`.

OpenCode wire types live in `tokn-opencode-protocol`. Its `v1` module models
the JSON payloads stored in the SQLite `message.data` and `part.data` columns,
while `run` models JSONL from `opencode run --format json`. Both decode through
native-JSON-first wrappers: unknown tags and malformed known variants remain
inspectable instead of preventing the rest of a session from loading. The
OpenCode source crate still owns SQLite queries, relational row identity, and
normalization into `AgentEvent`.

ZCode 3.7.3 persists the same tolerant V1 message/part payload family with
additional envelope fields, so `tokn-session-zcode` deliberately shares that
wire decoder while assigning the distinct `zcode` provider identity. The
ZCode application itself is closed source; compatibility is based on its
read-only local schema and retained native records rather than an upstream API.

Persisted WorkBuddy JSONL wire types live in `tokn-workbuddy-protocol`.
Messages, reasoning, function calls/results, file-history snapshots, and AI
titles have tolerant typed views while every record retains its exact native
JSON. Future tags, malformed known shapes, nested unknown content, and duplicate
record IDs remain losslessly inspectable. `tokn-session-workbuddy` merges the
SQLite catalog with JSONL discovery and normalizes messages, model changes,
reasoning, tools, usage, metadata, provider errors, and unknown records.

Current event families include:

- `session_started`
- `provider_changed`
- `session_settings_applied`
- `message`
- `question_request`
- `question_reply`
- `reasoning`
- `goal_updated`
- `agent_activity`
- `tool_call`
- `lifecycle`
- `usage`
- `metadata`
- `error`
- `unknown`

All providers use the accounting contract in `docs/event-ir.md`. Usage
distinguishes model calls, operation totals, and replaceable session snapshots.
OpenCode emits one model-call usage event per historical assistant turn: the
last valid `step-finish` tokens win, with assistant-message tokens as fallback.
DSH and Codex expose turn lifecycle (DSH also exposes steps). Compact human output labels the usage scope;
expanded browser rows and JSONL preserve native details except explicitly
hidden Pi content, which is available only in JSONL. Terminal Pet ignores
accounting/metadata/hidden content for activity and lease handling.

Messages carry an orthogonal `delivery` field: `commentary`, `final`, or
`unspecified`. Codex preserves the provider's response phase. Pi and OpenCode
assistant text is final because those persisted message records do not expose a
separate commentary channel; ZCode has the same persisted distinction. Current
Codex `final_answer` and legacy `final` phases both normalize to `final`; user
and other messages use `unspecified`.

Tool calls carry explicit operation roles and semantic display metadata:

- `record_kind`: `invocation`, `progress`, `result`, or provider-state `snapshot`
- `tool_name`, plus optional `provider_tool_name` and `transport` when a provider wrapper differs from the semantic operation
- `tool_kind`: `code_execution`, `terminal`, `shell`, `file_read`, `file_write`, `file_edit`, `search`, `web`, `task`, or `unknown`
- `summary`: compact facts for known tool families, such as shell command/exit code or file edit path and rough line counts
- `native`: the original provider record when an adapter has projected cleaner semantic fields

Raw `input` and `output` remain in the IR for debugging and provider-native
detail. Historical pretty rendering and the desktop viewer use the shared
operation projection; JSONL and live event consumers retain the atomic source
records so results can update naturally as they arrive.

Reasoning is intentionally flat:

- `text`
- `summary`
- `redacted`
- `encrypted_content`
- `signature`

`redacted: true` is a visible marker that the provider withheld readable text;
it is distinct from a hidden event. Pretty rendering shows visible reasoning
text and summaries, but does not display encrypted reasoning payloads. JSONL
preserves encrypted reasoning in the IR.

Codex `event_msg.thread_settings_applied` maps to
`session_settings_applied`. The normalized event exposes a compact settings
snapshot and retains the provider-native snapshot for JSON consumers. Human
rendering intentionally omits permission internals and embedded developer
instructions. The relay updates `SessionContext.cwd` when these settings change
without replacing the session's original project metadata.

Codex `event_msg.sub_agent_activity` maps to `agent_activity`. Its
`agent_thread_id` and `agent_path` identify the target of the activity, so the
IR names them `target_session_id` and `target_agent_path`. Actor identity is
optional and is not inferred from the containing rollout because child files
can include copied parent history. Human output therefore says `interaction
with /root` unless an actor is independently known. The first Codex
`session_meta` owns the rollout; later copied session headers do not replace
the normalizer or relay session identity.

Reusable display formatting lives in `crates/render`. It depends on `core`, not on terminal libraries. The CLI uses it for linear output and the interactive browser uses its `EventDisplay` rows for collapsed summaries and expanded detail.

Pretty rendering also prefers compact semantic tool lines, such as:

```text
shell cargo test #call_abc
edit crates/core/src/agent_event.rs +4 -1 #call_abc
read crates/cli/src/render.rs #call_abc
```

Unknown tools still render their raw input/output so new provider shapes remain discoverable.
Unknown events preserve raw provider-native payloads when available and pretty rendering shows that native payload.

`browse` is the first interactive historical-session view. Without a session id, it opens an alternate-screen session list; Enter opens the selected session. With a session id, it opens the event browser directly. The event browser uses one row per normalized event. Rows are collapsed by default; expanded rows reuse the same per-event pretty rendering as linear output.

Current browser keys:

- `j`/Down and `k`/Up move the selected event row.
- `h` collapses the selected row; `l` expands it.
- Enter/Space toggles expansion.
- In the session list, Enter opens the selected session.
- `z` expands only the selected row.
- `C` collapses all rows.
- `g`/Home and `G`/End jump to the first/last event.
- Ctrl-D/Ctrl-U move by a coarse page.
- In the event browser opened from the session list, `q`/Esc returns to the session list.
- In direct event browsing and the session list, `q`/Esc quits.

## Current Decisions And Edges

- OpenCode shell tools with nonzero `metadata.exit` are marked as errors even when OpenCode records the tool state as completed.
- Tool kind classification and summary extraction live in `crates/core`; provider normalizers should use the shared helpers where possible.
- OpenCode support currently uses the V1 `message` and `part` tables seen in
  local data, not the newer `session_message` projection. The newer table
  exists locally but is empty, and upstream has repeatedly reset its
  projections, so it is not yet treated as an authoritative history source.
- OpenCode V1 message roles, part types, nested tool states, and run-envelope
  types are decoded by `tokn-opencode-protocol`. Unknown and malformed shapes
  preserve their complete native JSON. The adapter retains SQLite row IDs and
  uses a part row ID as the fallback tool-call ID for historical records that
  lack `callID`. Assistant token rows normalize as one `model_call` per turn:
  the last valid `step-finish` row wins, with assistant-message usage as a
  fallback; malformed accounting stays visible as unknown data.
- Pi native JSONL parsing uses `tokn-pi-protocol`. Unknown message roles such
  as historical `bashExecution` records remain visible without preventing the
  rest of the session from loading.
- Pi branch-summary, opaque extension state, label, session-info,
  leaf, and active-tool records are validated metadata. Extension context
  messages have system role and explicit provenance/visibility; hidden content
  is redacted from human views and does not displace replayed visible messages.
  Assistant usage is per-call; tool-result and summary usage are operation
  totals. Cached input is included once; native costs remain inspectable.
- Codex native JSONL parsing uses `tokn-codex-protocol`. New rollout and
  response tags retain their native identity and payload for unknown-event
  discovery instead of being erased by an upstream catch-all enum.
- DeepSeek Harness is pinned as the `vendor/dsh` source-of-truth submodule.
  `tokn-dsh-protocol` decodes its logical session records, including the core
  event envelope and packed chunk rows. It preserves plugin-defined events and
  malformed known records losslessly. `tokn-session-dsh` expands packed rows,
  prefers assembled messages over redundant chunks, keeps unfinished deltas,
  and correlates tool calls/results. Its output is a chronological log view,
  not a reconstruction of the compacted model surface. Turn/step boundaries
  and outcomes are typed lifecycle events. Per-call usage prefers assembled
  usage, falling back to the last stream snapshot, and includes cached input
  in the normalized total. Recognized plugin/control records are validated
  metadata; plugin attribution and surface operations accompany messages and
  reasoning as provenance. Unsupported/malformed records and content remain
  native unknown events. Only explicit
  subagents form tree relationships; their `seedLength` excludes inherited
  parent history, while resume markers never hide their own earlier turns.
- Codex `response_item.agent_message` and legacy
  `inter_agent_communication` records map to `agent_activity` with
  validated author/recipient paths and optional typed `communication` content.
  Readable text and encryption presence are separate; ciphertext stays native.
  Only an immediately adjacent metadata marker contributes `trigger_turn` to a
  response item, including the consumed first-owned-message child boundary.
  Metadata remains independently preserved. Malformed identities or unsupported
  content stay unknown.
- Codex `world_state`, `turn_context`, `inter_agent_communication_metadata`,
  and rollback records are metadata, not conversation replies.
  `token_count` emits replaceable usage snapshots with consecutive identical
  info suppressed; decreases and context estimates are not rewritten as deltas.
  Missing usage and changed rate limits are diagnostic metadata. Historical
  subagent filtering still excludes copied parent context/accounting.
- Codex `event_msg.thread_goal_updated` maps to the visible `goal_updated` IR event.
- Codex `event_msg.thread_settings_applied` is a full effective snapshot, not a
  diff. Repeated applications remain visible in the event stream.
- Timestamps are provider-native strings/numbers today; there is no unified timestamp type yet.
- CLI help prints usage to stdout and exits successfully; parser errors exit nonzero.

## Print Invocation Status

`create` and `append` have an initial configurable executor path. They do not assume provider binaries are installed. Pass `--executor <launcher>` or set `TOKN_SESSION_<SOURCE>_EXECUTOR`, such as `TOKN_SESSION_OPENCODE_EXECUTOR`.

The executor is only the launcher, equivalent to the provider binary. Provider-specific print-mode arguments are added by the source adapter. For OpenCode, `create` appends `run --format json <prompt>`, so gateway-style commands look like:

```sh
tokn-session create --source opencode --executor "tokn-gateway proxy opencode --npx --" "create a todo app"
```

`append` supports exactly one target:

```sh
tokn-session append --source opencode --executor "tokn-gateway proxy opencode --npx --" --session <session-id> "next turn"
tokn-session append --source opencode --executor "tokn-gateway proxy opencode --npx --" --continue "next turn"
```

Advanced custom executors may include an argv that is exactly `{prompt}`; in that case the executor is treated as the full command and no provider-specific args are appended.

`--cwd <dir>` runs the executor from a specific working directory.

Current limitation: provider output is inherited directly from the child process. The shared `LiveSessionEvent` envelope now exists in `crates/core`, and `crates/render` can pretty-render live events, but the CLI print path does not consume it yet.

OpenCode has the first live-output normalizer: `OpenCodeLiveNormalizer` parses `opencode run --format json` JSONL envelopes into `LiveSessionEvent`. It maps `text`, `reasoning`, `tool_use`, `error`, and valid `step_finish` token data into normalized `AgentEvent`s; `step_start` and malformed or missing `step_finish` accounting stay lossless unknown native events.

## Known Gaps

- No `attach` command yet.
- ZCode, WorkBuddy, and DSH support historical reads and Relay watching;
  create/append and live input are not implemented.
- Codex and Pi have normalization fixtures. OpenCode now has wire-format
  fixtures plus adapter/source regression tests; full SQLite-backed CLI golden
  tests are still missing.
- Relay's ZeroMQ `PUB/SUB` mode intentionally has no persistence or
  delivery acknowledgement; subscribers that are disconnected can miss events.
- The terminal pet cannot distinguish every runtime state authoritatively until
  provider task lifecycle and interaction events are represented in `AgentEvent`.
- Viewer message input requires a live Codex Desktop owner or Pi bridge;
  historical sessions are not resumed through a fallback process.
  Without a Relay connection, selected
  timelines refresh from the durable index and historical source reads. Relay
  snapshot/follow supports all six providers; it is not an agent-control
  transport, and its unread tracking is not persisted yet.
- Viewer session-file relocation is deliberately conservative. Repeated or
  overlapping moves can make the retired source ambiguous, in which case the
  later path is treated as a new row instead of transferring prior attention.
  An already selected task is still tied to its physical path; after a native
  revert creates another continuation, reselect its current catalog entry.

## Useful Smokes

GitHub Actions CI runs Rust formatting, workspace check/test, the CLI build,
the pnpm viewer check, and all three Bun app check suites on pushes to `main`
and pull requests. The Rust job installs Tauri's Linux WebKit/GTK build
dependencies because the viewer backend is a workspace member.

```sh
cargo run -p tokn-session-cli -- list --source codex --limit 1
cargo run -p tokn-session-cli -- list --source opencode --limit 1
cargo run -p tokn-session-cli -- list --source zcode --limit 1
cargo run -p tokn-session-cli -- list --source workbuddy --session-dir crates/workbuddy/fixtures --limit 1
cargo run -p tokn-session-cli -- show --source workbuddy --session-dir crates/workbuddy/fixtures wb-shell-command
cargo run -p tokn-session-cli -- show --source opencode <session-id> --format pretty
cargo run -p tokn-session-relay -- stdout
cargo run -p tokn-session-relay -- zeromq
cd apps/discord-pet && bun run check
cd apps/discord-pet && bun run start -- --help
cd apps/discord-pet && bun run login -- --help
cd apps/pet && bun run check
cd apps/pet && bun run start -- --help
cd apps/terminal-pet && bun run check
cd apps/terminal-pet && bun run snapshot
cd apps/viewer && pnpm install --frozen-lockfile
cd apps/viewer && pnpm run check
cd apps/viewer && pnpm tauri dev
```

## Next Likely Work

- Wire `create`/`append` stdout through provider live normalizers instead of inheriting child stdout directly.
- Decide whether live stream consumption should live in `client` as callbacks/iterators or in the CLI command path.
- Extend provider fixture coverage with OpenCode SQLite normalization.
- Add CLI golden tests for tiny fixture-backed `list` and `show` outputs.
- Extend native storage invalidation beyond Codex/Pi and add incremental source
  paging to the desktop viewer after the historical read-only surface stabilizes.
- Teach terminal pet to use the preserved Codex turn lifecycle
  instead of heuristics. Pi live boundaries require a bridge feature; do not
  infer them from historical assistant/tool records. OpenCode input-request
  events remain a follow-up.

### Sidebar ordering

The viewer remembers a Time/Projects toggle locally. Time groups roots into the
last hour, today, yesterday, past seven days, older, and unknown time. Recent
rows keep their relative order for the mounted viewer; new activity arrivals
prepend and older pagination results append. Calendar groups refresh every 30
seconds and on focus. Switching views preserves the open conversation and tree.
Projects sort newest-discovered first before root pagination. Git worktrees and
subdirectories group under the shared repository, using Git common-directory
metadata rather than matching folder names. SQLite migration 8 caches these cwd
aliases so recognized historical worktrees stay grouped after removal. Unresolved
paths retain their own identity. Migration 7 stores immutable project anchors;
existing projects seed from earliest known session time (index discovery as
fallback). Later older history and removal/reappearance do not move the anchor.
