# Viewer session cache

The viewer loads history on demand and keeps eight resident sessions at most.
The selected session in each viewer window/browser tab is protected by a
renewable lease. Other explicitly opened sessions are evicted least recently
used first, after background preloads. File activity never updates user-access
recency. A session unload cancels its subscription and releases its history.

Each view reports up to 128 sidebar candidates, scoped to its current filters
and the selected session's full project directory. Debounced source changes can
preload at most two candidates into spare cache space. Preloading neither
selects the session nor acknowledges unread messages. It cannot evict an
explicitly opened session to gain admission. Candidates outside the current
scope are dropped. A new preload can replace a background session only after
its initial load completes and it has been idle for 30 seconds. A session also
waits 30 seconds between preload attempts after eviction or failure. Activity
updates this idle timer without changing user-access recency; three streaming
candidates therefore cannot continuously evict and reload one another. Explicit
opening bypasses the preload cooldown. The cache also considers an estimated 64 MiB memory target;
currently selected sessions may exceed it. These are internal policy constants,
not user settings or a strict process-RSS limit.

## History windows

An initial window starts at the latest visible user prompt and runs
through the latest record, including the current unfinished turn and usage.
The reader includes adjacent turn-start records and widens the boundary when
tool/compaction dependencies require older context. Hidden provider messages do
not count as user prompts. Histories with no user prompts use a 300-event tail,
aligned to complete source records and their required context.

Load earlier extends the window backward by three user turns. The retained
start survives live appends and switching away/back while the session remains
resident. New turns accumulate; this is not a sliding one-turn limit. A
source replacement creates new identities and preserves the loaded turn span
where possible. An evicted session reopens with the initial window.

Loaded normalized source records outside the delivered window live in an anonymous
temporary disk journal, with compact offsets, fingerprints, turn boundaries and
dependency positions in memory. Concurrent subscribers share the reader and
journal. Appends write only new records; old snapshots see their committed
prefix. The last subscriber releasing a generation closes its temporary file.
This is a disposable cache, not another durable copy of the session database.
Original provider files remain authoritative.

Initial loads reserve a slot per session, including while loading, and share
one initializer among subscribers. Unrelated sessions do not wait on that
initializer's I/O. Dropping an embedded subscription releases its handler;
the last subscriber cancels the reader. Cancellation is cooperative around
provider decoding and between journal records. Journal bytes also supply size
accounting, and only mutable-source reconciliation computes eager fingerprints.

Generation-scoped event keys use stable source positions. Lazy Codex ranges
start at an opaque positive anchor, so prepending unchanged normalized history
does not renumber existing cards, details or translations. If earlier context
changes the suffix shape, a new generation clears affected caches. The explicit
`has_earlier` flag controls loading; positive positions do not imply missing history. A separate
presentation slot can restore trajectory disclosure after a replacement; it
must never identify cached content. Inspector/native requests remain tied to
the same displayed snapshot. Malformed or interrupted updates cannot replace
the last committed window.

## Modes and limits

Automatic and indexed Local mode share the same embedded window reader and
native index watches; neither starts a redundant managed Relay child. External mode uses the additive
`follow_window` snapshot request and requires an updated snapshot server.
Legacy `follow` and row-based event-page requests still expose full history.

The memory target estimates retained serialized payloads and their live copies;
it excludes runtime overhead, normalizer state, compact indexes and temporary
parsing allocations. Three turns can themselves be large. Existing 128 MiB
source/journal/window payload limits still apply. Initial reads and some source
rewrites may transiently normalize the entire source. Codex/Pi normalizers keep
their append cursors, while mutable SQLite providers still scan a consistent
row snapshot to detect edits. WorkBuddy/DSH still normalize changed grouped
sources before reconciling them. A hard process-memory cap would need further
provider streaming and disk-backed retention of explicitly loaded pages.

OpenCode/ZCode retain compact row fingerprints and shared normalization
checkpoints instead of decoded message bodies. Unchanged rows reference the
previous disk journal; new or changed rows are decoded, with dependent rows
reprocessed only until normalization state converges. Unrelated WAL writes
still scan/hash the session rows but do not decode the unchanged history.

Views renew every 30 seconds and expire after 90 seconds without a heartbeat.
Monotonic lease revisions prevent a delayed update from reversing a release.
Multiple views can protect the same session; switching machines releases the
old machine's lease using its original transport. No view lease changes unread
state.

Frontend replicas and semantic delivery levels are described in [session updates](viewer-session-updates.md).
