# Relay records

`tokn-session-relay stdout --format json` emits one `RelayRecord` per JSONL
line. ZeroMQ publishes the same JSON as its second frame; the first frame is
the unchanged `provider.session_id` topic. Use `--native` with either output
mode to include provider data alongside the normalized events.

Native filesystem notifications accelerate the feed but are not required for
startup or continued operation. If watcher creation, registration, or its
callback stream fails, Relay releases the backend, reports one warning, and
continues using the configured `--poll-interval` (30 seconds by default).
Runtime failures trigger one recovery scan before regular polling resumes;
restart Relay to retry native watching. This also handles macOS's per-process
file limit when recursive kqueue watches exceed the available descriptors.
Watcher callbacks coalesce changed paths in a bounded inbox. A burst that
exceeds the path limit triggers a full recovery scan. File and provider read
failures produce warnings without discarding successful records from the same
scan. See [Relay I/O reduction](relay-io-reduction.md) for a reproducible
comparison with the merged implementation.
The viewer-managed child uses a five-minute polling interval because the
viewer has its own index watcher and recovery scheduler.

```json
{
  "path": "/sessions/pi.jsonl",
  "topic": "pi.session-1",
  "session": { "provider": "pi", "session_id": "session-1" },
  "operation": "upsert",
  "record_id": "jsonl:128",
  "native": { "type": "message", "id": "message-1", "message": {} },
  "events": []
}
```

The example abbreviates session context and native content. `events` contains
zero or more ordered `AgentEvent` objects. One native record may produce
reasoning, messages, tools, and usage together. An empty batch is valid, even
without native data. Batches follow source records, not a timer or arbitrary
number of messages. Replay windows retain entire records, including events
before the selected message within the same record.

`native` is omitted by default (`RelayConfig.include_native = false`), and is
not added to every `AgentEvent` variant. Existing native/provenance fields in
the IR remain unchanged. Opting in can expose additional provider metadata;
neither mode is a redaction or security boundary.

Compaction observations use the same record batches and snapshot/follow updates;
no extra transport or native subscription is required. Codex, Pi, OpenCode, ZCode, and DSH
normalization supplies these events even with native disabled. The viewer
projects correlated observations into a stable expandable card; terminal-pet
does not treat them as conversation replies or turn completion. All six providers are available through Relay.
This adds an `AgentEvent` variant: strict enum consumers must upgrade alongside
the producer. The transport framing/version is unchanged; older consumers are
not guaranteed to understand the expanded event vocabulary.

## Source boundaries and identity

Record keys are scoped by `(path, topic, record_id)`. An `upsert` replaces the
whole batch at that key, rather than appending every event again.

- Codex/Pi: one complete JSONL line, decoded once. Native data is the parsed
  original JSON value, including unknown fields (not original whitespace).
  `jsonl:<byte-offset>` identifies the line in the current file. Partial lines
  remain buffered until newline; malformed lines produce warnings.
- OpenCode/ZCode: a consistent read-only SQLite session snapshot is normalized into
  a session record and one record per message, with ordered hydrated parts.
  IDs are `session:<id>` and `message:<id>`. Native data is the decoded row
  structure (`data`, `parts`, IDs and timestamps), not every SQL column or a
  SQLite change feed. Any change to the batch, or to requested native data,
  republishes the record. Removed message records in an observed session emit
  `operation: "remove"`, an empty batch, and no native payload.

- WorkBuddy: a synthetic `session:<id>` batch followed by `row:<ordinal>`
  batches for complete nonblank JSONL rows. Native IDs can repeat; row ordinals
  keep those records distinct. Catalog metadata supplies session context.
- DSH: a `session:<id>` header batch followed by `row:<ordinal>` physical
  storage rows, including packed chunks. Native payloads preserve the packed
  row; events flatten its direct logical members. Subagent seed filtering and
  assembled-message/usage reconciliation match historical loading. Plain JSONL
  and concatenated `.jsonl.zstd` frames are supported.

The reader shares its grouped normalization with historical OpenCode/ZCode
loading; historical APIs still flatten to the existing `LoadedSession` shape.
Relay supports Codex, Pi, OpenCode, ZCode, WorkBuddy, and DSH.

WorkBuddy/DSH snapshots reload and normalize changed files; unchanged file
revisions are cached. Unfinished lines wait for a newline. Invalid complete
rows and incomplete/corrupt compressed frames fail without committing partial
snapshots. A later complete source can be followed after reconnecting.

The stdout/ZeroMQ output is a best-effort live feed, not a durable replication protocol: no ack,
resume cursor, or complete history on startup. JSONL offsets are not stable
across file truncation/replacement; whole-session deletion and database/file
replacement do not emit a complete set of tombstones. Viewers should use the
separate snapshot/follow service below instead of treating PUB as authoritative.
JSONL updates only decode appended complete lines. OpenCode still reloads
changed sessions (and can perform its existing correctness fallback); this
wire migration does not add incremental SQLite parsing.

## Local snapshot/follow service

Snapshot ownership lives in `viewer-core`. For browser access use the separate
[viewer HTTP API](viewer-api.md). Desktop External mode can use this local
compatibility endpoint:

```sh
tokn-viewer-api snapshot --bind tcp://127.0.0.1:5557 --native
```

`--native` is optional. Snapshot roots use provider-owned environment
overrides, shared with the viewer's automatic Relay:

| Provider | Default storage | Environment override |
| --- | --- | --- |
| Codex | `~/.codex/sessions`, `~/.codex/archived_sessions` | `CODEX_HOME` |
| Pi | `~/.pi/agent/sessions` | `PI_CODING_AGENT_SESSION_DIR` |
| OpenCode | `~/.local/share/opencode/opencode.db` | `OPENCODE_DB`, `XDG_DATA_HOME` |
| ZCode | `~/.zcode/cli/db/db.sqlite` | `ZCODE_STORAGE_DIR` |
| WorkBuddy | `~/.workbuddy-ai` catalog and `projects` histories | `WORKBUDDY_CONFIG_DIR`, `CODEBUDDY_CONFIG_DIR` |
| DSH | `~/.dsh/sessions` | `DSH_HOME` |

The snapshot polling interval is 500ms
for active sessions; the shared metadata catalog is cached for two seconds.
Legacy `follow` supplies complete history; `follow_window` supplies a retained
turn window. Replay-window CLI flags apply only to live feeds.
It does not start a PUB socket: pets can continue using an independently
configured `zeromq` process on port 5556, without changing their wire format.

This endpoint is **length-prefixed JSON over TCP, not ZeroMQ**. Each frame is
a big-endian u32 byte length followed by UTF-8 JSON. Version 1 requests are
`{"version":1,"action":"catalog"}` or
`{"version":1,"action":"follow","session_key":"<catalog key>"}`.
The server answers `hello` with its version, providers and native capability.
Catalog responses contain `header` frames followed by `catalog_end`, including
discovery warnings. Follow requests accept only keys from the discovered
catalog, never arbitrary paths.

The additive `follow_window` action takes the same `session_key`, optional
`retain_from` generation-scoped event position, and optional `before_event`
for loading three earlier user turns. With both absent it starts at the latest
three user turns. Each transaction includes one `window` frame after `begin`,
containing `event_offset` and `has_earlier`, before any records. Clients retain
that start across appends. Lazy Codex ranges use a positive opaque anchor plus
contiguous event offsets, rather than counting all omitted normalized events.
A fully loaded range can therefore retain a positive `event_offset`:
`has_earlier` is authoritative. Prepending preserves positions when the previous
normalized suffix is unchanged; context that changes its shape starts a new
generation. Upgrade source service and clients together for this window contract.
A late tool/compaction dependency can widen the
window with an atomic reset within the same source generation. An older
server that does not support this action fails explicitly; it does not silently
fall back to loading full history. See [session cache](viewer-session-cache.md).

Catalog discovery returns lightweight headers immediately. A shared background
cache backfills Pi's latest `session_info` name and first user preview, plus
first-prompt fallbacks for untitled OpenCode/ZCode/Codex root sessions and
WorkBuddy/DSH titles and previews. Native titles
take precedence. Names/previews arrive on subsequent catalog polls, independent
of `--native`; a cleared Pi name falls back to its first prompt.
Pi advances a byte cursor through complete appended lines rather than rescanning
history on each update. Other hydration is cached by file/DB+WAL revision.
The cache retains at most 512 characters per title/preview, not events or native
bodies; work runs in bounded batches off the request path. Failed reads preserve
last-good metadata and retry on source changes. It is in-memory and backfills
again after service restart. Followed Pi names can update in header-only commits.

A follow connection receives `begin`, zero or more `record` frames, then
`commit`. Begin and commit carry matching generation and decimal-string
revision values; begin also carries the session header and `reset` flag.
The first transaction has `reset: true`. Subsequent JSONL transactions append
new records in the same generation. File replacement, truncation and detected
same-length rewrites start a fresh generation and replace the entire snapshot.
OpenCode/ZCode DB/WAL changes reconcile raw rows in one read transaction. Unchanged
message records reuse disk-journal positions and compact normalization checkpoints; changes to
model state recompute dependent records until the state converges. SQLite rows
are still scanned because timestamps/counts cannot reliably identify edits.
Unrelated writes and WAL checkpoints publish nothing when the session is
unchanged. A true appended suffix stays in the same generation; edits,
deletions, reordering or database replacement reset the snapshot. Header-only
changes can commit without record frames. With `--native`, changes to existing
native data (including session update timestamps) also require a reset under
the v1 append/reset protocol. This does not change stdout/ZeroMQ loading.

WorkBuddy also invalidates followed snapshots on catalog DB/WAL changes.
WorkBuddy/DSH share the append/reset reconciliation: an unchanged prefix keeps
the generation; edited/deleted records reset it. Appending an assembled DSH
message can suppress earlier stream/usage records, which requires a reset.
DSH's 128 MiB limit applies to both the stored file and decoded source bytes.

Event histories are loaded on demand into temporary disk journals with compact
in-memory indexes. Concurrent subscribers share one reader and normalizer per
session; complete appended JSONL lines decode only once.
An idle session is released after its last subscriber leaves. Source errors,
invalid frames and interrupted transactions never commit partial data.
Slow clients are disconnected and reconnect to a complete snapshot; there is
no durable event journal or resume-after-restart cursor. Disposable journal
files close when the last reference to that history leaves. Heartbeats are sent
every two seconds. Coalesced notifications include every intervening append.

Limits: numeric loopback addresses only (no remote authentication), 64 client
connections, 16 active sessions, 8 MiB per wire frame, 128 MiB serialized
records per history journal/window, 128 MiB per JSONL source and 128 MiB of raw
OpenCode session-row payloads per scan. These are payload limits,
not exact process-RAM caps; decoded objects and snapshots add overhead. Large
sessions fail explicitly rather than silently returning partial history.
Any local process can connect; `--native` may expose sensitive provider data.

### Viewer connection

The indexed viewer defaults to **Automatic Relay**, using viewer-core's shared
source readers and native index watches directly. Local uses the same readers.
Polling recovers missed notifications; a second live-feed child no longer parses
the same append. The shipped app needs no separate Relay installation or PATH
lookup. Codex roots verified as the active home's `sessions` or
`archived_sessions` retain that home's title/preview metadata without parsing
transcript bodies. Unrelated explicit directories do not inherit this metadata.

Unindexed embeddings retain the managed Relay child: versioned stdout JSONL,
stdin EOF lifetime, a ten-second readiness timeout, and up to three startup
attempts. Oversized 8 MiB pipe records are skipped with a stderr warning.
Standalone Relay stdout/ZeroMQ behavior is unchanged. Other Relay processes are
never stopped. See [session update delivery](viewer-session-updates.md) for the
viewer contract and cache ownership.

The panel persists `mode` (`automatic`, `external`, or `local`), external
`endpoint`, and `include_native` in app config `relay.json`. Native is off by
default and optional for Automatic; External uses the service's capability.
Legacy `enabled: true` settings migrate to External with the same endpoint;
explicitly disabled settings migrate to Local. Missing settings use Automatic.
Invalid settings produce a visible error instead of silent fallback.

External connects to a separately started `tokn-viewer-api snapshot` and never
owns that process. Local explicitly clears Relay snapshots, stops any owned
child, and wakes the local catalog/index path. Switching mode, external endpoint,
or native inclusion clears snapshots to avoid mixing data sources.
Catalog polling updates the sidebar; up to eight recently opened sessions share
received snapshots between timeline, trajectories and Inspector. Covered providers
bypass local body indexing/reading. Uncovered providers retain local history.
Connection failures and automatic child restarts retain last-good committed
data without local fallback. Catalog and session connections are cancelled
before reattaching core snapshots (and the feed in unindexed embeddings).

Live updates refresh the loaded event window and expanded trajectory items,
including while reading older events. Scrolling follows only at the bottom;
otherwise the reading position is preserved and “New activity” jumps to the
latest page. Each refresh uses one pinned snapshot for its bounded pages.
Append refreshes preserve expansion keys, while a replacement
generation clears them. Optional native records are available in bounded
Inspector details, excluding records containing redacted reasoning. Local
durable unread/seen indexing is not applied to Relay-backed sessions yet.

## Consumer migration

This intentionally replaces the old wire `event: AgentEvent` field with
`events: AgentEvent[]`; update external consumers together with Relay.
`TailUpdate.records` is the Rust library batch. JSON and ZeroMQ use the same
contract; human output still renders every normalized event separately.

All bundled pets use `apps/shared/relay.ts`. It validates a whole record before
dispatching its events sequentially, retaining path/session context and
awaiting each worker for backpressure. Empty batches do not wake workers.
Workers and rules still use internal single-event `RelayEvent` values; the
shared adapter is used by standalone pets and the supervisor. They ignore
native data and removals and remain activity observers, not record stores or
history-reconciliation clients. A bounded 4,096-record activity cache suppresses
unchanged OpenCode/ZCode/WorkBuddy/DSH event occurrences when a snapshot is updated
(including native-only edits or shifted context events). Additional identical
occurrences remain activity; cache eviction can allow replay and is not durable deduplication.
The default spawned Relay does not request
native data. Legacy wire envelopes are rejected rather than guessed.
