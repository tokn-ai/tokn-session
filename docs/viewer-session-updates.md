# Session delivery

Viewer-core owns normalized provider history, turn/group membership, stable
identities, revisions, and live subscription baselines. The frontend owns
loaded history and per-resource display coverage. Delivery levels are
projections: they control filtering as well as payload richness. Existing
`final`, `messages`, `steps`, and `all` are predefined projections; they do
not describe the frontend's mixed cache contents. `final` selects user messages,
final assistant replies, and attention; `messages` also selects intermediate
assistant replies; `steps` adds activity summaries; `all` includes every source
event. The older `details` preset remains compatible, but new clients load
detail resources independently.

## Loading and live delivery

Direct browser clients register per-session interests over `/api/v1/live`.
The first WebSocket frame authenticates with the existing bearer token; tokens
never enter URLs. Same-origin and explicitly configured origins are allowed.
The socket forwards only its registered session updates. Other clients' tool
payloads are filtered before transfer. Catalog/index/attention notifications
remain on SSE; modern clients exclude session and background updates from that
stream when the WebSocket carries them.

`subscribe_session` returns only identity, generation, and revision. It does
not read or send session history. Initial registration has revision zero and
publication waits for its backward baseline. `load_session_backward` returns
a snapshot at the requested level, defaulting to `steps` with the latest three turns
and collapsed inner groups (`recent_turns` scope). The older `latest_turn` scope
remains available for callers explicitly requesting one turn. History pagination uses a separate `history_cursor`;
explicit earlier loading expands the retained range. Subscribing and publishing
share the update-store lock with backward loading, so changes cannot escape
between snapshot capture and baseline registration. Frontend replicas buffer
live changes arriving before the HTTP snapshot, discard older revisions, and
recover gaps with another backward read.

Opening never walks saved reading anchors through old history before first
paint. Recently opened sessions display their cached screen during catch-up.
Readers explicitly load older history. The returned turn anchor pins live
appends so new prompts extend an open view rather than dropping its first turn.
Outstanding older questions retain attention context.

The WebSocket carries live diffs only. Source replacements that require a
snapshot produce a small resync notice; backward HTTP performs recovery.
Reconnection restores open-session interests and refetches their display
baselines. Socket heartbeats renew leases
without fetching data. Connection ownership prevents delayed cleanup from
an old socket removing interests reclaimed by its replacement. Changing a
subscription's level establishes a new baseline through backward loading.

## Background messages and session coverage

One independent compact interest covers either **All sessions** on the connected
machine (the default), or **Sidebar sessions**, the loaded root and child rows.
This scope is separate from delivery level. The background projection carries
the session's latest assistant first line, including intermediate replies,
message identity/delivery/phase, and direct attention/running state. It does not
carry tools, native records, or child text as the parent's preview.
Background delivery offers `messages` (the default, including intermediate
replies) and `final` (only final assistant previews). Question/running state and
authoritative clearing/removal stay live at both levels. Intermediate-only
changes do not consume final subscription revisions; activity changes alongside
an intermediate reply omit message coverage so the previous preview survives.

`subscribe_background_updates` (WebSocket `subscribe_background`) registers
identity, scope, and background level only. Registration, scope/level changes, and reconnection never
read history or send a history snapshot. The first live update can advance from
revision zero without a backward baseline. Each subscription has its own
generation and decimal revision; updates are complete compact projections,
with unchanged projections suppressed. Heartbeats renew its 90-second lease.
Deduplication compares message and activity fields, so a tool-only source
checkpoint does not repeat an unchanged preview. Unknown staged bodies preserve
the last known message digest.
Disconnect releases only interests owned by that connection.
Expired leases fail renewal so a suspended desktop window can establish a fresh
interest when it wakes. Read acknowledgements retain the existing durable
notification/catalog authority; compact snapshots cannot restore a cleared dot.

The indexer's existing normalized body jobs persist this bounded projection in
the session index. Migration 9 adds it without requeueing old bodies. An unknown
message has a null `message_revision` and leaves an existing preview intact;
a committed empty message or removal clears it. Secondary local viewers follow
committed SQLite projections instead of opening sources for sidebar updates.
This does not change the indexer's existing source discovery/body work.
A runtime silently seeds compact SQLite fingerprints at startup, without sending
them or reading sources. Owner topology recovery checks tracked sessions;
shared-index followers compare present compact metadata against that baseline.
Subscription and reconnect therefore do not replay existing sidebar data.

The frontend keeps up to 2,048 compact previews independently of its eight rich
conversation replicas. Accepted first-screen pages can seed previews, but late
page responses cannot overwrite newer compact messages. Active semantic replicas
at Messages/Steps/All own their previews while subscribed, because their source
pipeline can be ahead of indexing. Subscription changes retain bounded preview text while
retiring live activity authority; stale IDs/generations are ignored. Gaps and
reconnects establish a fresh live interest without loading unopened history.
The scope and level preferences are stored locally. The bottom status bar shows a compact
live-updates entry; its popup separates session scope, background delivery,
and open-conversation delivery. The conversation offers `final`, `messages`,
`steps` (the default), and `all`; changes load the requested projection's
backward baseline and retain per-resource cache coverage. Unsupported
hosts show a warning there while open sessions continue through the existing path.
Legacy desktop External reader mode has no compact metadata feed and reports
background updates unavailable; native Local/Automatic mode and encrypted remote
connections to modern hosts support it.
Messages subscriptions omit the new level field for older-host compatibility.
Final requires an explicit matching acknowledgement; unsupported hosts report
the failure without silently reverting to Messages.
Legacy full-page conversation loading remains available at Steps. Missing
non-Steps presets report a conversation error rather than showing unfiltered rows.

## Explicit resources and cache coverage

`load_session_details` accepts a tagged resource request:

- `group`: explicit inner-group interests and the display revision cursor;
  delivers complete child-summary membership in a coherent update envelope.
- `tool`: an event key; returns its display payload without provider-native
  records.

`inspect_session_event` independently returns normalized/source inspection
and opted-in native records. It applies the existing redaction and payload
bounds. Display expansion never implicitly fills inspection coverage.

The frontend separately tracks group, tool, and inspection resources as
missing, loading, complete, stale, or failed, with generation/revision metadata
for accepted loads. A session can have collapsed groups, loaded groups, and
only some tool payloads. Complete groups remain cached and interested after
collapse; live changes update their summaries. Changed semantic objects
invalidate only affected payloads. A source revision marker catches payload-only changes that leave summaries
unchanged; it conservatively invalidates loaded payloads when source changes
cannot be localized. Visible stale tools/inspection reload;
collapsed payloads wait until requested. Obsolete in-flight results cannot
replace newer coverage. Resource payloads have a 32 MiB/256-entry cache budget,
in addition to the existing bounded display replicas.

## Adapters, compatibility, and limits

Tauri exposes the same subscribe/backward/details/inspect commands through
its native event bridge. Renewal commands carry identities only. The React
`loadSessionUpdates` compatibility facade translates snapshot requests into
live registration plus backward loading; it no longer issues data heartbeats.
Older servers fall back to the previous update/page commands and revision-based
lease renewal. Legacy trajectory
pages still assemble complete groups atomically before rendering.

The existing Hub and paired HTTP tunnels do not upgrade an upstream WebSocket. They keep
SSE compatibility and support the new HTTP commands through the route
allowlist. Shared grants clamp background scope to their exact granted session
keys, namespace subscription identities by principal, and filter events before
transfer. Owner read markers and unshared descendant activity stay private.
Scoped fixed-cadence invalidations remain compatible.

Subscriptions expire after 90 seconds and are bounded to 24 entries/64 MiB.
Compact background interests have an independent 24-entry limit, up to 4,096
sidebar keys/256 KiB per interest, and 10,000 in-memory deduplication entries.
Evicted fingerprints spill to a private temporary SQLite file with a bounded
page cache, so large follower scans do not replay unchanged messages. The spill
contains session keys and digests, never message text, and is removed on shutdown.
Frontend display replicas retain eight recent sessions with a 64 MiB target,
protecting the selected session. Session activity does not promote its LRU
position. Each level has independent revision coverage; receiving final events
does not advance steps coverage. Source events at `all` remain separate from
folded display objects. Native inspection is independently opt-in.

## Local opening diagnostics

Cold source loading happens before taking the shared subscription lock; snapshot
capture and publication still share that lock. One slow cold session therefore
does not hold up unrelated live subscriptions while its source reader starts.
Codex window readers resolve lineage, scan backward to the latest explicit turn
start, seed the normalizer from session metadata, and decode only that suffix.
Earlier requests prepend three turns; ordinary polls retain the loaded range.
Split tool results widen the source range to their invocation before publication,
including late live outputs. Completed lifecycle items already contain their
own display context. Dependency widening is bounded, with a full-reader fallback
for distant/missing invocations. Missing turn checkpoints and thread-spawn
filtering also use the full reader. Inherited prefix metadata, cutoffs and file
guards remain verified; omitted body parsing/validation waits until loading.
The tolerant decoder avoids temporary payload clones.

For a read-only source timing probe, save a JSON object containing `source_path`
and `session_id`, then run:

```sh
TOKN_PROFILE_SOURCE=/path/to/source-metadata.json TOKN_PROFILE_ROOT=/path/to/codex/sessions \
  cargo test -p tokn-viewer-core profile_local_codex_open -- --ignored --nocapture
```

It prints lineage/decode/reader timings and source/window counts, never message
contents. Debug timings are diagnostic, not desktop release benchmarks.

## Remaining costs

Other providers and Codex correctness fallbacks still normalize full history.
Codex source windows can be broader than the selected projection when dependencies
require earlier context. Initial delivery covers the latest three turns and required context. Earlier
source-window expansion can resend the retained window between source service
and viewer-core. Projection still rebuilds retained timelines and tool assembly
before comparison, under the shared subscription lock; this work has not been
made incremental. Embedded source transport still uses JSON framing. This
change removes automatic history backfill, unrelated browser session traffic,
and repeated data-fetching heartbeats; it does not eliminate all backend scans.
