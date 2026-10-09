# Relay I/O reduction after the first PR

This compares merged `main` at `065f49b1` with the next Relay I/O changes.
Both are debug builds on macOS. Three paired runs per workload used fresh,
synthetic provider storage and observed two seconds after initial discovery.
The append and database workloads relied on native notifications with a
30-second recovery poll. Idle discovery used a 200 ms poll over 32 Pi files.

| Workload | Logical bytes read, before → after | Read calls | Tracked file calls |
| --- | ---: | ---: | ---: |
| Idle, 32 Pi files | 0 → 0 | 0 → 0 | 1,290 → 970 |
| One Pi append | 165 → 165 | 2 → 2 | 28 → 27 |
| 128-record Pi burst | 21,168 → 21,168 | 32 → 32 | 214 → 198 |
| OpenCode part edit with updated session summary | 49,352 → 32,868 | 14 → 9 | 96 → 64 |
| OpenCode part edit with unchanged session summary | 3,708,788 → 36,964 | 1,004 → 10 | 3,760 → 64 |

Values are medians of three runs per version. All 30 trials delivered the
expected records with zero warnings. The unchanged-summary case has 100
sessions with one message and one part each. Relay must inspect every session
to find a part edit when OpenCode does not update its session summary. Before
this change, each session load opened another SQLite connection and reread
many of the same database pages. Relay now reuses one connection across the
catalog and selected session loads. Each session still has a separate read
transaction so a large scan does not hold one WAL snapshot throughout. This
cut measured logical read bytes by 99.0% and read calls by 99.0% in that
fixture. The ordinary summary-updated case also used fewer reads and bytes.

The JSONL change removes a duplicate metadata lookup for each tracked file on
polls and watcher scans. In the idle workload, metadata calls fell from 910 to
590; directory calls remained 380. A full OpenCode record load also derives
message count and untitled preview from messages it already loaded, removing
the count query and, for untitled sessions, the preview query. OpenCode and
ZCode share this reader.

Delivery times and burst notification counts varied between runs; three
pairs are insufficient for a general latency claim. The benchmark counts
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
