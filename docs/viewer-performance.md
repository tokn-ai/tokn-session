# Viewer read performance

## Investigation

A five-second macOS stack sample of the running debug viewer on 2026-09-20
showed catalog refreshes, SQLite row decoding, and metadata/key construction
on active worker stacks. One process CPU reading was about 130%. The local
index contained roughly 4,600 sessions. This reading includes a live workload;
it is not an idle baseline or a release-build measurement.

Two paths amplified small changes:

- A changed-file notification loaded all indexed sessions, built catalog maps,
  and opened the file's header before recognizing a duplicate revision.
  Periodic scans of other providers also decoded unrelated Codex rows.
- A linked Codex rollout rediscovered its lineage, read and normalized all
  retained history, then serialized old and new records to compare them after
  each append. Local cache hits rediscovered the same lineage too.

Relay emits a hint per normalized record. Those hints previously bypassed the
filesystem watcher's batching, multiplying the catalog work while streaming.

## Current boundaries

The durable index owns discovery and bounded presentation metadata. Known file
updates query only their source and sole present session, check the file
revision before opening the header, and use an indexed duplicate-identity
lookup. Membership or identity uncertainty still requests a full catalog.
Full provider scans retain that provider's tombstones for relocation but do
not decode other providers. Revision/generation preconditions still protect
concurrent writers.

Relay hints share a 200 ms quiet window with a one-second maximum batch age.
Explicit retry, recovery, and scheduled deadlines retain priority. This batches
index work; it does not delay the authoritative snapshot reader's live feed.

`CodexHistoryReader` owns the resolved physical history, normalizer, byte
cursor, incomplete final row, and ordinal state. Cold reads verify inherited
cutoffs. Successful warm reads return an append delta; resets return a complete
replacement. The viewer retains historical record allocations and serializes
only new records. Errors discard the reader's tentative state while the viewer
keeps its last published snapshot, so retries cannot skip a rejected batch.
An inherited parent can keep appending beyond the saved cutoff without
rebuilding the child; the reader checks the unchanged bounded prefix guards.

Local snapshot cache hits stat their already resolved files. They do not scan
directories or parse headers. Changed or missing dependencies force a fresh
read. A cached lineage stays bound to those physical files until invalidation;
new competing candidates are detected on the next discovery.

## Append-only contract

Native Codex JSONL growth is treated as append-only within one physical file.
Replacement, truncation, same-size edits, changed history configuration, and
changed header/tail guards invalidate the reader. Inherited references remain
bounded by their saved exclusive byte and ordinal cutoffs. Partial final rows
are buffered until a newline arrives.

The reader compares the complete owning metadata header and a bounded tail
guard without reparsing history. These guards catch common rewrite-and-grow
cases, but cannot prove that arbitrary middle bytes were not edited while the
file grew. Detecting every such edit requires rereading the prefix or a stronger
provider revision protocol. This tradeoff is explicit rather than claiming
metadata alone proves immutability.

Append-only source records also do not imply immutable rendered cards. Tool
results, compaction pairs, and terminal usage can revise earlier projections.
Any future projection cache must preserve those dependencies.

## Measurements

Reproducible manual debug benchmarks use temporary fixtures and do not write
provider sessions. The catalog comparison ran the same fixture on merged
commit `110c509` and the updated code. Linked history compares ten complete
reloads with ten warm reader polls in the same executable.
The catalog fixture uses an in-memory SQLite index; real disk commits add I/O
latency beyond the measured lookup and metadata work.

| Workload | Before | After |
| --- | ---: | ---: |
| 5,001 sessions; 200 duplicate file hints | 6.75 s | 14 ms |
| 5,001 sessions; 100 actual file appends | 3.19 s | 24 ms |
| About 1 MiB inherited history; 10 appends | 137 ms | 0.97 ms |
| About 10 MiB inherited history; 10 appends | 1.34 s | 2.16 ms |

The catalog benchmark reduced header reads from 300 to 100. Warm history
appends parsed exactly ten rows, read about 2 KiB of new body data, and performed
zero lineage discoveries at both sizes. Header/tail guard bytes are counted
separately. Unchanged polls parse no rows. These component timings do not imply
the same proportional reduction in whole-app CPU.

Run the manual benchmarks with:

```sh
cargo test -p tokn-viewer-core targeted_catalog_benchmark --lib -- --ignored --nocapture
cargo test -p tokn-session-codex benchmark_linked_history_appends --test history -- --ignored --nocapture
```

Use separate target directories when comparing separate source checkouts.
Normal regression tests assert operation counts, state continuity, and reset
behavior; they do not impose machine-dependent timing thresholds.

## Follow-up profiling after session windows

Two five-second samples on 2026-09-21, 102 seconds apart, both caught a
worker in initial history journaling and another in a complete catalog scan.
The running binary contained the LRU code. About 58% of the journal worker's
samples were in fingerprint serialization/hashing; the rest were principally
journal encoding/writes. Codex metadata SQLite reads and repeated sidebar
relation/key construction also appeared. The Relay child was mostly sleeping.
These are wall-stack samples: SQLite read waits are not CPU measurements, and
they do not establish which notification triggered the full scan.

The follow-up changes address repeated work at its source:

- Three writing candidates cannot rotate through the two background slots on
  every change. Loading/active preloads retain their slots; idle replacement
  and per-session retry cooldowns use 30 seconds. Explicit access is immediate.
- Normalized payloads are encoded once for journal writes and byte charging.
  Append-only Codex/Pi readers no longer hash records they never compare;
  mutable providers hash the same encoded payload that is written.
- Embedded subscriptions own their server handlers. Initial loads share one
  reservation per session without holding the global session lock during I/O.
  The last subscriber cancels work; provider decodes check cancellation around
  their blocking operation, while journaling also checks between records.
- Sidebar requests coalesce index notifications into one trailing refresh,
  including during pagination. Query changes still start immediately.
- Routine source hints skip rebuilding candidate sets when no view expired.

A manual debug benchmark alternates the previous triple-encoding algorithm
and the updated path three times in one executable, using the same disk
journal and index work. Median timings for 160 records with 64 KiB responses:

| Journal workload | Previous encoding | Updated encoding |
| --- | ---: | ---: |
| 10 MiB initial load | 1.625 s | 342 ms |
| Ten 64 KiB appends | 106 ms | 19.7 ms |

This measures journal construction, not provider decoding, catalog scans, or
whole-app CPU. The live development viewer restarted during implementation,
so these results are not a controlled before/after process-CPU comparison.
Run it with:

```sh
cargo test -p tokn-viewer-core benchmark_journal_initial_load_and_appends --lib -- --ignored --nocapture
```

## Compact background publication

Two live macOS stack samples on 2026-10-18 caught the debug viewer repeatedly
reconciling compact metadata for all remembered sessions during ordinary chat
updates. The index held 14,555 rows. One shared-reconciliation worker appeared
in 2,652/3,651 and 5,231/6,369 wall sample units, principally in whole-provider
metadata reads, session-key construction, and JSON/SHA-256 fingerprints. These
are stack exposure counts, not CPU percentages.

Known-file staging set the sidebar's `catalog_refresh_required` flag, which also
triggered full compact recovery. The default All scope expanded that recovery
to every startup fingerprint. Sidebar refresh and compact recovery now have
separate signals: ordinary header staging does no compact work, and completed
bodies publish only their changed sessions and canonical ancestors. Membership,
relationship, relocation, and shared-index recovery retain full reconciliation.
Notifications derive present inventory, attention, messages, and tombstones from
one provider row snapshot instead of decoding provider metadata twice.

A synthetic debug benchmark compares the former All-fanout policy with targeted
publication in the same executable. Both arms use the updated one-snapshot
projector, identical cached fingerprints, and one append to a temporary source;
fixture setup and body indexing are excluded. Five trials alternate arm order:

| Indexed sessions | Former All fanout, median | Targeted publication, median |
| --- | ---: | ---: |
| 1,004 | 222 ms | 24.8 ms |
| 5,004 | 1,215 ms | 134 ms |

The regression asserts three fingerprint checks (child, parent, grandparent)
at both catalog sizes, zero compact work during header staging, one provider
metadata snapshot per body notification batch, and no historical replay.
Separate recovery cases preserve reparenting and removal behavior. This is a
publication-component comparison, not a whole-app CPU or release-build result.
The metadata snapshot and relation graph still scale with provider size, as do
sidebar listings and existing body-job metadata reads.

```sh
cargo test -p tokn-viewer-core benchmark_background_append_publication --lib -- --ignored --nocapture
```

## Remaining work

- Session residency and turn windows now bound retained histories; see
  [session cache](viewer-session-cache.md). Payloads outside the retained window
  live in temporary disk journals. Initial/replacement parsing can still have
  full-source transient allocations, and the selected-session memory target is
  soft so explicitly loaded history does not disappear.
- Automatic mode still frames its in-process snapshot transport as JSON and
  materializes the retained window as `LoadedSession` after each append. Shared immutable
  chunks and a direct subscription could remove that copying.
- Timeline projection and usage classification repeat across page/detail
  requests. Cache by snapshot identity first; incremental projection needs
  explicit dependencies for cards that can change after an append.
- Indexed Local and Automatic now share the windowed provider reader. Ordinary
  standalone historical Codex/Pi loads still perform separate metadata/count
  and normalization passes.
- Managed Relay normalizes full payloads that Automatic consumes as path hints.
  A compact invalidation feed would avoid that duplicate work.
- Sidebar relation rebuilding and unwatched-provider recovery scans remain
  candidates for profiling after the current changes are loaded.
