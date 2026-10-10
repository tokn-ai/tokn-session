# Relay I/O reduction after the first PR

This compares merged `main` at `065f49b1` with the next Relay I/O changes.
Both are debug builds on macOS. Three paired runs per workload used fresh,
synthetic provider storage. Steady-state workloads observed two seconds after
initial discovery; the cold-start workload counted process launch through
readiness, a 20 ms signal-handler grace period, and shutdown. Append and
database workloads relied on native notifications with a 30-second recovery
poll. Idle discovery used a 200 ms poll over 32 Pi or 128 Codex files.

| Workload | Logical bytes read, before → after | Read calls | Tracked file calls |
| --- | ---: | ---: | ---: |
| Idle, 32 Pi files | 0 → 0 | 0 → 0 | 1,290 → 970 |
| One Pi append | 165 → 165 | 2 → 2 | 28 → 27 |
| 128-record Pi burst | 21,168 → 21,168 | 32 → 32 | 214 → 198 |
| Idle, 128 Codex files | 0 → 0 | 0 → 0 | 4,870 → 3,590 |
| One Codex append | 116 → 116 | 2 → 2 | 28 → 27 |
| 128-record Codex burst | 14,896 → 14,896 | 32 → 32 | 214 → 216 |
| One new Codex file | 296 → 296 | 2 → 2 | 46 → 45 |
| OpenCode part edit with updated session summary | 49,352 → 32,868 | 14 → 9 | 96 → 64 |
| OpenCode part edit with unchanged session summary | 3,708,788 → 36,964 | 1,004 → 10 | 3,760 → 64 |

Values are medians of three runs per version. All 30 original and 30 added
Codex trials delivered the expected records with zero warnings. The
unchanged-summary case has 100 sessions with one message and one part each.
Relay must inspect every session
to find a part edit when OpenCode does not update its session summary. Before
this change, each session load opened another SQLite connection and reread
many of the same database pages. Relay now reuses one connection across the
catalog and selected session loads. Each session still has a separate read
transaction so a large scan does not hold one WAL snapshot throughout. This
cut measured logical read bytes by 99.0% and read calls by 99.0% in that
fixture. The ordinary summary-updated case also used fewer reads and bytes.

The JSONL change removes a duplicate metadata lookup for each tracked file on
polls and watcher scans. In the idle workload, metadata calls fell from 910 to
590 for Pi and from 3,030 to 1,750 for Codex; directory calls remained 380
and 1,840 respectively. Codex's 128 nested rollouts show the same shared-path
gain. Directory watcher events also skip a second metadata probe for files
already discovered, while retaining tracked files for retry on errors other
than `NotFound`. A full OpenCode record load also derives
message count and untitled preview from messages it already loaded, removing
the count query and, for untitled sessions, the preview query. OpenCode and
ZCode share this reader.

Cold startup used 128 newline-terminated Codex rollouts, each with a header and
a 16 KiB message. Logical bytes read fell from 2,097,155 to 1,048,707; opens
fell from 398 to 270, while read calls stayed at 258. Relay now reuses the
header file handle and checks the final byte before reading a trailing partial
line. This saved one open and 8,191 returned bytes per completed rollout in
the fixture. The cold-start counters also include launch and shutdown, so they
are not a trace of startup syscalls alone. Startup wall times varied too much
across these samples to claim a latency change.

Delivery times and burst notification counts varied between runs; three
pairs are insufficient for a general latency claim. The Codex burst's two
extra tracked calls in its median are within that watcher variation. The
benchmark counts
selected libc calls and bytes returned to userspace, not physical disk
requests or disk bytes. SQLite caching, memory mapping, and unhooked APIs are
outside these counters. SQLite's page cache lives for one synchronous scan
and is released afterward. Real provider data and long-running memory behavior
remain to be measured.

## Reproduce

On macOS with Rust, Clang, and Python 3:

```sh
baseline_dir=$(mktemp -d /tmp/tokn-relay-merged.XXXXXX)
git archive 065f49b1 | tar -x -C "$baseline_dir"
(cd "$baseline_dir" && cargo build -p tokn-session-relay)
cargo build -p tokn-session-relay
clang -dynamiclib -std=c11 -Wall -Wextra -Werror -O2 \
  tools/relay-io-bench/interpose.c -o /tmp/libtokn-relay-io.dylib
python3 tools/relay-io-bench/run.py \
  --before "$baseline_dir/target/debug/tokn-session-relay" \
  --after target/debug/tokn-session-relay \
  --interposer /tmp/libtokn-relay-io.dylib \
  --output /tmp/tokn-relay-io-reduction.json --repetitions 3
```

The driver verifies record topics, counts, and unique record keys. Its JSON
output contains every trial and all raw counters. The provider fixtures use
only temporary synthetic data. See [counter details](relay-performance.md#what-the-counters-mean)
for the measurement window and interception limits.

## Event stream follow-up

Managed viewers now exchange version-2 batches of source identities. A
synthetic 128-record Pi burst with 128-byte assistant messages serialized
76,562 bytes as full Relay records and 82 bytes as one managed hint batch
(99.9% fewer pipe bytes). This measures serialized IPC traffic, not disk I/O.
Reproduce with `cargo test -p tokn-session-relay
stdio::tests::managed_message_burst_reduces_wire_bytes -- --nocapture`.

A two-session follow regression appends to both sources, then sends 100 hints
for one source. It observes one poll for that reader and zero for the unrelated
reader. Quiet batching is 50 ms, bounded at 200 ms for continuous updates;
500 ms polling still recovers omitted hints. Run `cargo test -p tokn-viewer-core
service_server::tests` for those checks.

Cold Codex startup still reads only its header and trailing partial line.
The first append to a preexisting rollout now restores its original prefix
silently to recover pending question/tool/compaction context and current cwd.
That first active append costs a prefix read and normalization pass; subsequent
appends stay incremental. The earlier append byte counts above predate this
correctness repair and are not the current first-append baseline.
