# Relay I/O measurements

This compares the relay before the watcher and error-handling changes
(`f55b6a0`) with the updated relay. Both were debug builds on macOS, measured
with three paired runs per workload on 2026-10-09. Each run used fresh synthetic
provider storage. The benchmark reset counters after initial discovery and
observed two seconds of subsequent activity. The file workloads used native
watching and a 30-second recovery poll; the idle workload used a 200 ms poll.

| Workload | Bytes read, before → after | Read calls | Tracked file calls | Last record latency |
| --- | ---: | ---: | ---: | ---: |
| Idle: 32 Pi files | 0 → 0 | 0 → 0 | 1,600 → 1,290 | — |
| Append one Pi record | 165 → 165 | 2 → 2 | 27 → 28 | 0.90 → 3.16 ms |
| Append 128 records across 16 Pi files | 21,168 → 21,168 | 32 → 32 | 211 → 214 | 15.75 → 13.42 ms |
| Edit one OpenCode part in a 100-session WAL database | 49,352 → 49,352 | 14 → 14 | 94 → 123 | 8.24 → 2.13 ms |

Values are medians of the three runs for each version. All 24 runs emitted the
expected records without warnings. For the idle workload, metadata calls fell
from 1,220 to 910; directory calls stayed at 380. Avoiding a separate `stat`
for each directory entry accounts for that repeatable reduction. The update
workloads did not reduce bytes read or read calls. File-call and latency samples
vary with watcher callback timing, especially for OpenCode, so these small
samples do not establish a throughput or latency improvement. The new bounded
watcher inbox primarily limits queued path work under bursts.

An initial version waited 20 ms after a watcher wake to combine callbacks.
The same benchmark showed median last-record latency rising from 1.69 to
22.92 ms for the single append, and from 9.38 to 37.89 ms for the burst.
The fixed wait was removed before the results above.

## What the counters mean

`tools/relay-io-bench/interpose.c` intercepts successful `read`, `pread`,
`readv`, and `preadv` calls and sums their returned bytes. It also counts
`open`/`openat`, `stat`/`lstat`/`fstat`/`fstatat`, and `opendir`/`readdir`
calls. “Tracked file calls” is the sum of those categories. These are calls
through interposed libc symbols, **not physical disk I/O requests or disk
bytes**. A `readdir` call represents one API invocation, often one directory
entry, rather than one kernel request. Memory-mapped reads, direct syscalls,
and other library entry points are outside these counters. OS caching and
filesystem scheduling also affect latency. The benchmark does not measure
peak memory, provider query counts, or a real user session mix. Counting starts
about 20 ms before the printed wall interval and ends on process exit, shortly
after the two-second window and shutdown signal; the same procedure applies to
both versions.

## Reproduce

On macOS with Rust, Clang, and Python 3:

```sh
baseline_dir=$(mktemp -d /tmp/tokn-relay-baseline.XXXXXX)
git archive f55b6a0 | tar -x -C "$baseline_dir"
(cd "$baseline_dir" && cargo build -p tokn-session-relay)
cargo build -p tokn-session-relay
clang -dynamiclib -std=c11 -Wall -Wextra -Werror -O2 \
  tools/relay-io-bench/interpose.c -o /tmp/libtokn-relay-io.dylib
python3 tools/relay-io-bench/run.py \
  --before "$baseline_dir/target/debug/tokn-session-relay" \
  --after target/debug/tokn-session-relay \
  --interposer /tmp/libtokn-relay-io.dylib \
  --output /tmp/tokn-relay-io-results.json --repetitions 3
```

The driver checks exact output topics and record counts and retains every
trial's raw counters in its JSON output. It uses only temporary synthetic
provider data. To measure physical disk requests and bytes, use an OS-level
filesystem tracer or disk instrumentation in a controlled environment.
