# Session delivery

Viewer-core owns normalized provider history, turn/group membership, stable
identities, revisions, and live subscription baselines. The frontend owns
loaded history and per-resource display coverage. Delivery levels are
projections: they control filtering as well as payload richness. Existing
`final`, `messages`, `steps`, and `all` are predefined projections; they do
not describe the frontend's mixed cache contents. `final` selects user messages,
final assistant replies, and attention; `messages` also selects intermediate
assistant replies; `steps` adds activity summaries; `all` includes every source
event. Display details and source inspection load independently.

## Loading and live delivery

The viewer opens one event stream per machine connection. Direct browsers,
remote desktop, Hub web and Apell use `POST /api/v1/events` with a fresh
connection prefix. The host sends general notifications plus timeline and
sidebar updates whose IDs belong to that prefix. Desktop Local receives the
same event families through Tauri. Clients establish listeners before reading
the catalog so they can buffer events that precede a command response.

`open_session` registers live interest and returns a coherent timeline in
one command. A cold open defaults to Steps and the latest three turns with
collapsed groups. Opening a cached session paints its existing cards immediately
and sends the previous subscription ID, generation, revision and SHA-256 state
hash in `resume`.
When the host can identify that exact projection, it sends changed items,
removals, compact edits to each affected order and current state. An unchanged reopen sends zero
timeline items. A missing or invalid baseline produces a snapshot in the same
response. A separate `history_cursor` extends the retained history window.
Opening a group changes the requested group membership through the same command.

The host captures the baseline under the same lock used for live publication.
The client checks that a queued delta's generation, base revision and base hash
match the cached timeline before applying its edits.
After a connection interruption it reopens its retained interest with its last
accepted revision and hash. A reader reset sends one `relay-changed` invalidation to
the owner and a targeted notice to affected shared viewers. A replacement
discovered during normal publication also produces a small resync notice on
SSE. Recovering a timeline reads history only for the open session. Operations
that submit agent input are independent of reconnect and recovery.

`release_session` retires a selected interest when the viewer switches away.
The host keeps up to eight released projections within its existing memory
budget. Changes to an inactive session do not produce timeline pushes for it.
A host restart or eviction discards the corresponding resume baseline. Readers
can explicitly request older history. The first open never searches older
pages for a saved scroll anchor. New turns extend the retained window, and
older outstanding questions retain their attention context.

## Session list

The sidebar opens a metadata-only window for the current search and providers.
Expanding a parent opens a separate window for its children. The `sessions`
array holds summaries, each with its session key and a SHA-256 revision.
When the index commits a change, the host compares the revisions in each
affected window and sends only new or changed summaries in `sessions`. Keys
of sessions leaving a window appear in `removed_keys`. An unchanged window
produces no list event, including for shared viewers.

The frontend owns display order. For example, when a session becomes active,
the host sends its updated summary and the frontend uses the activity time to
place it. Changing between Time and Projects is local when all matching sessions
are loaded. For a partial list the host uses the requested ranking to select
page membership, and the frontend sorts the loaded sessions.

Each client keeps one `synced_through` cursor with the list it has applied.
When two clients watch the same page with the same permissions, the host
shares the work of checking its membership. It records when each summary last
changed and keeps recent removal keys. A client can disconnect, miss several
changes, and reopen with `since` to receive them in one batch. Multiple edits
to a session yield its latest summary. If the host has discarded the history
needed for that cursor, it sends the current page as a snapshot.

For example, if a phone misses a rename of A and the deletion of B while a
desktop stays online, the phone can send its own cached cutoff and receive A
in `sessions` and B in `removed_keys`. The desktop keeps its live subscription.
Paginated and filtered views track page membership so a session that enters a
full page can displace another. Indexing progress and compact assistant
previews use their own event types.

The host caches up to 64 distinct pages and supports up to 1,024 live list
subscriptions. Each subscription holds a routing ID and the last cutoff sent.
Subscribers to the same page share its summaries and removal history. An edit
produces one internal broadcast per group of subscribers at the same cutoff
and transport. HTTP streams expand only their matching subscription IDs.
Local desktop subscriptions use a separate delivery group. A lease
lasts 90 seconds and the viewer renews up to 64 together. When capacity is
needed, the host can evict a page with no active subscriptions. If every page
is in use, the user can still page through the index. The viewer retries
admission and resumes live updates when a lease becomes available.

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

`subscribe_background_updates` registers
identity, scope, and background level only. Registration, scope/level changes, and reconnection never
read history or send a history snapshot. The first live update can advance from
revision zero without a backward baseline. Each subscription has its own
generation and decimal revision; updates are complete compact projections,
with unchanged projections suppressed. Heartbeats renew its 90-second lease.
Deduplication compares message and activity fields, so a tool-only source
checkpoint does not repeat an unchanged preview. Unknown staged bodies preserve
the last known message digest.
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
The scope and level preferences persist locally. The live-updates popup
controls the background subscription and the open conversation independently.
Changing conversation levels sends a single open for the requested projection.
The desktop External reader has no compact metadata feed and reports background
updates unavailable. Local and Automatic readers support compact updates.

## Explicit resources and cache coverage

`open_session` with `scope.group_keys` requests complete group membership
at the active projection's revision. `load_session_details` with kind `tool`
takes an event key and returns its display payload.

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

## Adapters and limits

Viewer protocol revision 4 uses the same open, release, resource and renewal
commands on Tauri and HTTP. The Hub transports HTTP-shaped commands and SSE
through the encrypted connection. Shared grants restrict access to their
explicit session keys and namespace subscription IDs per recipient and grant.
Shared viewers receive live updates to their own subscriptions. Idle
streams carry only transport heartbeats. Read markers and activity from unshared
descendants stay private.

The host admits up to 24 active timeline subscriptions with 90-second leases.
Active and released timeline projections share a 64 MiB target. The host evicts
released projections first. Renewal returns `renewed_ids` and `missing_ids`
without reading histories. Expiry, gaps and source failures suspend live
ownership of sidebar state while preserving the last displayed cards.
Targeted recovery revalidates only affected interests.

Compact background interests have an independent 24-entry limit, up to 4,096
sidebar keys/256 KiB per interest, and 10,000 in-memory deduplication entries.
Evicted fingerprints spill to a private temporary SQLite file with a bounded
page cache, so large follower scans do not replay unchanged messages. The spill
contains session keys and digests, never message text, and is removed on shutdown.
Frontend display replicas retain eight recent sessions with a 64 MiB target,
protecting the selected session. Eviction releases every active delivery level;
late opens cannot restore a released interest. Session activity does not promote its LRU
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

When the index signals a change, the host rereads affected pages and compares
summary fingerprints once per distinct active view. Multiple clients watching
the same page share this cost. Different filters, page boundaries or grants
can still require separate queries. Lease renewal and transport heartbeats
do not run them.

Other providers and Codex correctness fallbacks still normalize full history.
Codex source windows can be broader than the selected projection when dependencies
require earlier context. Initial delivery covers the latest three turns and required context. Earlier
source-window expansion can resend the retained window between source service
and viewer-core. Projection still rebuilds retained timelines and tool assembly
before comparison, under the shared subscription lock; this work has not been
made incremental. Embedded source transport still uses JSON framing. This
change removes automatic history backfill, unrelated browser session traffic,
and repeated data-fetching heartbeats; it does not eliminate all backend scans.
