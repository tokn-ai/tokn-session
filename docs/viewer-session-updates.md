# Session delivery

Viewer-core owns normalized provider history, turn/group membership, stable
identities, revisions, and live subscription baselines. The frontend owns
loaded history and per-resource display coverage. Delivery levels are
projections: they control filtering as well as payload richness. Existing
`final`, `steps`, `details`, and `all` remain predefined projections; they do
not describe the frontend's mixed cache contents.

## Loading and live delivery

Direct browser clients register per-session interests over `/api/v1/live`.
The first WebSocket frame authenticates with the existing bearer token; tokens
never enter URLs. Same-origin and explicitly configured origins are allowed.
The socket forwards only its registered session updates. Other clients' tool
payloads are filtered before transfer. Catalog/index/attention notifications
remain on SSE; modern clients exclude session updates from that stream.

`subscribe_session` returns only identity, generation, and revision. It does
not read or send session history. Initial registration has revision zero and
publication waits for its backward baseline. `load_session_backward` returns
a snapshot at the requested level, defaulting to `steps` with the latest turn
and collapsed inner groups. History pagination uses a separate `history_cursor`;
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
Reconnection restores interests and refetches affected display baselines,
including background final subscriptions. Socket heartbeats renew leases
without fetching data. Connection ownership prevents delayed cleanup from
an old socket removing interests reclaimed by its replacement. Changing a
subscription's level establishes a new baseline through backward loading.

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
allowlist. Read-only shares retain their authorized legacy commands and scoped
fixed-cadence invalidations. No share gains access to host-wide live streams.

Subscriptions expire after 90 seconds and are bounded to 24 entries/64 MiB.
Frontend display replicas retain eight recent sessions with a 64 MiB target,
protecting the selected session. Session activity does not promote its LRU
position. Each level has independent revision coverage; receiving final events
does not advance steps coverage. Source events at `all` remain separate from
folded display objects. Native inspection is independently opt-in.

## Local opening diagnostics

Cold source loading happens before taking the shared subscription lock; snapshot
capture and publication still share that lock. One slow cold session therefore
does not hold up unrelated live subscriptions while its source reader starts.
Inherited Codex history resolves its lineage once, and its tolerant decoder reads
typed fields from the retained JSON without temporary payload clones.

For a read-only source timing probe, save a JSON object containing `source_path`
and `session_id`, then run:

```sh
TOKN_PROFILE_SOURCE=/path/to/source-metadata.json TOKN_PROFILE_ROOT=/path/to/codex/sessions \
  cargo test -p tokn-viewer-core profile_local_codex_open -- --ignored --nocapture
```

It prints lineage/decode/reader timings and source/window counts, never message
contents. Debug timings are diagnostic, not desktop release benchmarks.

## Remaining costs

Source readers may still retain/normalize a broader window than the selected
projection. Initial retained source delivery covers the latest user turn and required context. Earlier
source-window expansion can resend the retained window between source service
and viewer-core. Projection still rebuilds retained timelines and tool assembly
before comparison, under the shared subscription lock; this work has not been
made incremental. Embedded source transport still uses JSON framing. This
change removes automatic history backfill, unrelated browser session traffic,
and repeated data-fetching heartbeats; it does not eliminate all backend scans.
