#!/usr/bin/env python3
"""Compare two relay binaries on isolated, repeatable I/O workloads.

Build both binaries and the macOS interposer before running this driver. Each
trial gets fresh provider storage, and only work after startup is counted.
The counters describe intercepted libc calls and returned bytes, not physical
disk operations or SQLite statement counts.
"""

import argparse
from collections import Counter
from dataclasses import dataclass
import json
import os
from pathlib import Path
import resource
import signal
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import threading
import time


COUNTER_KEYS = (
  "read_calls", "read_bytes", "pread_calls", "pread_bytes",
  "readv_calls", "readv_bytes", "preadv_calls", "preadv_bytes",
  "open_calls", "openat_calls", "stat_calls", "lstat_calls",
  "fstat_calls", "fstatat_calls", "opendir_calls", "readdir_calls",
)
SCENARIOS = ("idle", "append", "burst", "opencode_edit")
PI_FILES = 32
BURST_FILES = 16
BURST_LINES_PER_FILE = 8
OPENCODE_SESSIONS = 100
STARTUP_TIMEOUT_SECONDS = 30
DELIVERY_TIMEOUT_SECONDS = 10
POST_WRITE_WINDOW_SECONDS = 2


@dataclass(frozen=True)
class OutputLine:
  stream: str
  text: str
  time_ns: int


class OutputCollector:
  def __init__(self, process: subprocess.Popen[str]):
    self.process = process
    self.lines: list[OutputLine] = []
    self.finished: set[str] = set()
    self.condition = threading.Condition()
    self.threads = [
      threading.Thread(target=self._read, args=("stdout", process.stdout), daemon=True),
      threading.Thread(target=self._read, args=("stderr", process.stderr), daemon=True),
    ]
    for thread in self.threads:
      thread.start()

  def _read(self, stream_name: str, stream):
    assert stream is not None
    for line in stream:
      entry = OutputLine(stream_name, line.rstrip("\r\n"), time.perf_counter_ns())
      with self.condition:
        self.lines.append(entry)
        self.condition.notify_all()
    with self.condition:
      self.finished.add(stream_name)
      self.condition.notify_all()

  def snapshot(self) -> list[OutputLine]:
    with self.condition:
      return list(self.lines)

  def wait_until(self, predicate, timeout: float, description: str) -> list[OutputLine]:
    deadline = time.monotonic() + timeout
    while True:
      with self.condition:
        lines = list(self.lines)
        streams_finished = len(self.finished) == 2
      # Parsing 128 burst records can take time; keep the collector lock free
      # while checking so the reader threads can timestamp new records.
      if predicate(lines):
        return lines
      remaining = deadline - time.monotonic()
      if remaining <= 0 or streams_finished:
        tail = "\n".join(f"{line.stream}: {line.text}" for line in lines[-12:])
        raise RuntimeError(f"timed out waiting for {description}; exit={self.process.poll()}\n{tail}")
      with self.condition:
        if len(self.lines) == len(lines) and len(self.finished) != 2:
          self.condition.wait(min(remaining, 0.1))

  def join(self):
    for thread in self.threads:
      thread.join(timeout=2)


def parse_args():
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument("--before", type=Path, required=True, help="baseline relay executable")
  parser.add_argument("--after", type=Path, required=True, help="PR relay executable")
  parser.add_argument("--interposer", type=Path, required=True, help="compiled macOS I/O counter dylib")
  parser.add_argument("--output", type=Path, help="write full JSON results to this path")
  parser.add_argument("--repetitions", type=int, default=3, help="paired repetitions per workload (default: 3)")
  return parser.parse_args()


def validate_inputs(args):
  if sys.platform != "darwin":
    raise ValueError("this driver requires macOS DYLD interposition")
  if args.repetitions < 1:
    raise ValueError("--repetitions must be positive")
  for name in ("before", "after"):
    path = getattr(args, name).expanduser().resolve()
    if not path.is_file() or not os.access(path, os.X_OK):
      raise ValueError(f"--{name} is not an executable file: {path}")
    setattr(args, name, path)
  args.interposer = args.interposer.expanduser().resolve()
  if not args.interposer.is_file():
    raise ValueError(f"--interposer is not a file: {args.interposer}")


def pi_session_header(index: int) -> str:
  return json.dumps({
    "type": "session",
    "version": 3,
    "id": f"bench-{index:03d}",
    "timestamp": "2026-01-01T00:00:00Z",
    "cwd": "/tmp/relay-io-bench",
  }, separators=(",", ":")) + "\n"


def pi_message(index: int, ordinal: int) -> str:
  return json.dumps({
    "type": "message",
    "id": f"bench-message-{index:03d}-{ordinal:03d}",
    "timestamp": "2026-01-01T00:00:01Z",
    "message": {
      "role": "assistant",
      "content": [{"type": "text", "text": f"benchmark {index}:{ordinal}"}],
    },
  }, separators=(",", ":")) + "\n"


def create_opencode_fixture(database: Path) -> sqlite3.Connection:
  connection = sqlite3.connect(database)
  connection.execute("pragma journal_mode = wal")
  connection.execute("pragma wal_autocheckpoint = 0")
  connection.executescript("""
    create table session (
      id text primary key, parent_id text, directory text not null,
      time_created integer not null, time_updated integer not null
    );
    create table message (
      id text primary key, session_id text not null,
      time_created integer, data text not null
    );
    create index message_session_id on message(session_id);
    create table part (
      id text primary key, message_id text not null, session_id text not null,
      time_created integer, data text not null
    );
    create index part_session_message on part(session_id, message_id);
  """)
  with connection:
    for index in range(OPENCODE_SESSIONS):
      session_id = f"ses_{index:03d}"
      message_id = f"msg_{index:03d}"
      connection.execute(
        "insert into session values (?, null, ?, ?, ?)",
        (session_id, "/tmp/relay-io-bench", index + 1, index + 1),
      )
      connection.execute(
        "insert into message values (?, ?, ?, ?)",
        (message_id, session_id, index + 1, '{"role":"user"}'),
      )
      connection.execute(
        "insert into part values (?, ?, ?, ?, ?)",
        (f"part_{index:03d}", message_id, session_id, index + 1,
         json.dumps({"type": "text", "text": f"before {index:03d}"}, separators=(",", ":"))),
      )
  return connection


def create_fixture(root: Path, scenario: str) -> tuple[Path | None, sqlite3.Connection | None]:
  if scenario == "opencode_edit":
    database = root / "opencode.db"
    return None, create_opencode_fixture(database)
  pi_dir = root / "pi"
  pi_dir.mkdir()
  for index in range(PI_FILES):
    (pi_dir / f"session_{index:03d}.jsonl").write_text(pi_session_header(index), encoding="utf-8")
  return pi_dir, None


def relay_command(binary: Path, root: Path, pi_dir: Path | None, scenario: str) -> list[str]:
  missing = root / "unused"
  return [
    str(binary), "stdout", "--format", "json",
    "--poll-interval", "200ms" if scenario == "idle" else "30s",
    "--codex-dir", str(missing / "codex"),
    "--pi-dir", str(pi_dir if pi_dir is not None else missing / "pi"),
    "--opencode-dir", str(root / "opencode.db" if scenario == "opencode_edit" else missing / "opencode.db"),
    "--zcode-dir", str(missing / "zcode.sqlite"),
    "--workbuddy-dir", str(missing / "workbuddy"),
    "--dsh-dir", str(missing / "dsh"),
  ]


def records_since(lines: list[OutputLine], start_index: int) -> list[tuple[OutputLine, dict]]:
  records = []
  for line in lines[start_index:]:
    if line.stream != "stdout":
      continue
    try:
      record = json.loads(line.text)
    except json.JSONDecodeError as error:
      raise RuntimeError(f"relay emitted invalid JSON: {line.text[:200]}") from error
    if not isinstance(record, dict) or "record_id" not in record:
      raise RuntimeError(f"relay emitted a non-record JSON line: {line.text[:200]}")
    records.append((line, record))
  return records


def expected_topics(scenario: str) -> Counter[str]:
  if scenario == "idle":
    return Counter()
  if scenario == "append":
    return Counter({"pi.bench-000": 1})
  if scenario == "burst":
    return Counter({f"pi.bench-{index:03d}": BURST_LINES_PER_FILE for index in range(BURST_FILES)})
  return Counter({"opencode.ses_000": 1})


def observed_topics(lines: list[OutputLine], start_index: int) -> Counter[str]:
  return Counter(record.get("topic") for _, record in records_since(lines, start_index))


def perform_workload(scenario: str, pi_dir: Path | None, connection: sqlite3.Connection | None):
  if scenario == "append":
    assert pi_dir is not None
    with (pi_dir / "session_000.jsonl").open("a", encoding="utf-8") as file:
      file.write(pi_message(0, 0))
  elif scenario == "burst":
    assert pi_dir is not None
    for index in range(BURST_FILES):
      with (pi_dir / f"session_{index:03d}.jsonl").open("a", encoding="utf-8") as file:
        file.write("".join(pi_message(index, ordinal) for ordinal in range(BURST_LINES_PER_FILE)))
  elif scenario == "opencode_edit":
    assert connection is not None
    with connection:
      connection.execute(
        "update part set data = ? where id = 'part_000'",
        ('{"type":"text","text":"after 000"}',),
      )
      # Move the session summary as OpenCode normally does when a turn changes.
      # This targets one session rather than triggering the all-session
      # correctness fallback for an otherwise invisible part edit.
      connection.execute(
        "update session set time_updated = ? where id = 'ses_000'",
        (OPENCODE_SESSIONS + 1,),
      )


def read_metrics(path: Path) -> dict[str, int]:
  try:
    value = json.loads(path.read_text(encoding="utf-8"))
  except (OSError, json.JSONDecodeError) as error:
    raise RuntimeError(f"missing or invalid interposer metrics at {path}: {error}") from error
  if not isinstance(value, dict):
    raise RuntimeError(f"interposer metrics must be a JSON object: {path}")
  for key in (*COUNTER_KEYS, "cpu_total_ns"):
    if not isinstance(value.get(key), int) or value[key] < 0:
      raise RuntimeError(f"missing or invalid interposer counter {key}: {value}")
  if all(value[key] == 0 for key in COUNTER_KEYS):
    raise RuntimeError("all I/O counters are zero; DYLD interposition may not have loaded")
  return value


def summarize_counters(counters: dict[str, int]) -> dict[str, int]:
  read_calls = sum(counters[key] for key in ("read_calls", "pread_calls", "readv_calls", "preadv_calls"))
  read_bytes = sum(counters[key] for key in ("read_bytes", "pread_bytes", "readv_bytes", "preadv_bytes"))
  opens = counters["open_calls"] + counters["openat_calls"]
  metadata = sum(counters[key] for key in ("stat_calls", "lstat_calls", "fstat_calls", "fstatat_calls"))
  directories = counters["opendir_calls"] + counters["readdir_calls"]
  return {
    "read_calls": read_calls,
    "read_bytes": read_bytes,
    "open_calls": opens,
    "metadata_calls": metadata,
    "directory_calls": directories,
    "tracked_calls": read_calls + opens + metadata + directories,
  }


def run_trial(binary: Path, interposer: Path, scenario: str, repetition: int, version: str) -> dict:
  with tempfile.TemporaryDirectory(prefix="tokn-relay-io-") as directory:
    root = Path(directory)
    pi_dir, connection = create_fixture(root, scenario)
    metrics_path = root / "metrics.json"
    env = os.environ.copy()
    env["DYLD_INSERT_LIBRARIES"] = str(interposer)
    env["TOKN_IO_METRICS_PATH"] = str(metrics_path)
    command = relay_command(binary, root, pi_dir, scenario)
    before_cpu = resource.getrusage(resource.RUSAGE_CHILDREN)
    process = subprocess.Popen(
      command, env=env, stdin=subprocess.DEVNULL,
      stdout=subprocess.PIPE, stderr=subprocess.PIPE,
      text=True, bufsize=1,
    )
    collector = OutputCollector(process)
    try:
      collector.wait_until(
        lambda lines: any(line.stream == "stderr" and line.text.startswith("following ") for line in lines),
        STARTUP_TIMEOUT_SECONDS, "relay startup banner",
      )
      # In particular, let SQLite's first SHM callback settle before reset.
      time.sleep(0.5)
      startup_lines = collector.snapshot()
      startup_warnings = [line.text for line in startup_lines if line.stream == "stderr" and line.text.startswith("warning:")]
      if startup_warnings:
        raise RuntimeError(f"native watcher or startup failed: {startup_warnings}")
      if records_since(startup_lines, 0):
        raise RuntimeError("existing fixture unexpectedly emitted records during startup")

      process.send_signal(signal.SIGUSR1)
      time.sleep(0.02)
      start_index = len(collector.snapshot())
      measurement_start_ns = time.perf_counter_ns()
      write_start_ns = None
      write_end_ns = None
      if scenario == "idle":
        time.sleep(2)
      else:
        write_start_ns = time.perf_counter_ns()
        perform_workload(scenario, pi_dir, connection)
        write_end_ns = time.perf_counter_ns()
        expected = expected_topics(scenario)
        observed_lines = collector.wait_until(
          lambda lines: all(observed_topics(lines, start_index)[topic] >= count for topic, count in expected.items()),
          DELIVERY_TIMEOUT_SECONDS, f"{scenario} records",
        )
        if observed_topics(observed_lines, start_index) != expected:
          raise RuntimeError(f"unexpected records: {observed_topics(observed_lines, start_index)}; expected {expected}")
        remaining = (write_start_ns + int(POST_WRITE_WINDOW_SECONDS * 1e9) - time.perf_counter_ns()) / 1e9
        if remaining > 0:
          time.sleep(remaining)

      stop_ns = time.perf_counter_ns()
      process.send_signal(signal.SIGINT)
      try:
        exit_code = process.wait(timeout=10)
      except subprocess.TimeoutExpired as error:
        raise RuntimeError("relay did not exit after SIGINT") from error
      exit_ns = time.perf_counter_ns()
      collector.join()
      final_lines = collector.snapshot()
      if exit_code != 0:
        tail = "\n".join(f"{line.stream}: {line.text}" for line in final_lines[-12:])
        raise RuntimeError(f"relay exited {exit_code}\n{tail}")
      entries = records_since(final_lines, start_index)
      topics = Counter(record.get("topic") for _, record in entries)
      expected = expected_topics(scenario)
      if topics != expected:
        raise RuntimeError(f"unexpected records: {topics}; expected {expected}")
      record_keys = [(record["topic"], record["record_id"]) for _, record in entries]
      if len(set(record_keys)) != len(record_keys):
        raise RuntimeError(f"duplicate record keys in {scenario}: {record_keys}")
      if scenario == "opencode_edit" and entries[0][1].get("record_id") != "message:msg_000":
        raise RuntimeError(f"unexpected OpenCode edit record: {entries[0][1].get('record_id')}")
      warnings = [line.text for line in final_lines[start_index:] if line.stream == "stderr" and line.text.startswith("warning:")]
      if any("filesystem notifications disabled" in warning for warning in warnings):
        raise RuntimeError(f"native watcher failed during measurement: {warnings}")

      counters = read_metrics(metrics_path)
      after_cpu = resource.getrusage(resource.RUSAGE_CHILDREN)
      cpu_process_total_s = (
        after_cpu.ru_utime + after_cpu.ru_stime - before_cpu.ru_utime - before_cpu.ru_stime
      )
      latency_ms = None
      first_latency_ms = None
      post_write_latency_ms = None
      if entries:
        assert write_start_ns is not None
        assert write_end_ns is not None
        first_latency_ms = (entries[0][0].time_ns - write_start_ns) / 1e6
        latency_ms = (entries[-1][0].time_ns - write_start_ns) / 1e6
        post_write_latency_ms = (entries[-1][0].time_ns - write_end_ns) / 1e6
      result = {
        "scenario": scenario,
        "repetition": repetition,
        "version": version,
        "records": len(entries),
        "warnings": len(warnings),
        "warning_texts": warnings,
        "active_window_s": (stop_ns - measurement_start_ns) / 1e9,
        "measurement_elapsed_s": (exit_ns - measurement_start_ns) / 1e9,
        "first_latency_ms": first_latency_ms,
        "delivery_latency_ms": latency_ms,
        "post_write_latency_ms": post_write_latency_ms,
        "write_duration_ms": None if write_start_ns is None else (write_end_ns - write_start_ns) / 1e6,
        "cpu_total_ns": counters["cpu_total_ns"],
        "cpu_process_total_s": cpu_process_total_s,
        "counters": counters,
        "totals": summarize_counters(counters),
      }
      return result
    finally:
      if process.poll() is None:
        process.kill()
        process.wait(timeout=5)
      collector.join()
      if connection is not None:
        connection.close()


def print_summary(trials: list[dict]):
  columns = (
    ("wall_s", lambda row: row["measurement_elapsed_s"]),
    ("read_bytes", lambda row: row["totals"]["read_bytes"]),
    ("read_calls", lambda row: row["totals"]["read_calls"]),
    ("opens", lambda row: row["totals"]["open_calls"]),
    ("metadata", lambda row: row["totals"]["metadata_calls"]),
    ("directory", lambda row: row["totals"]["directory_calls"]),
    ("tracked_calls", lambda row: row["totals"]["tracked_calls"]),
    ("cpu_ms", lambda row: row["cpu_total_ns"] / 1e6),
    ("latency_ms", lambda row: row["delivery_latency_ms"]),
    ("post_write_ms", lambda row: row["post_write_latency_ms"]),
  )
  print("scenario version repetition records warnings " + " ".join(name for name, _ in columns))
  for row in trials:
    values = ["-" if getter(row) is None else f"{getter(row):.2f}" for _, getter in columns]
    print(f"{row['scenario']} {row['version']} {row['repetition']} {row['records']} {row['warnings']} " + " ".join(values))
  print("\nMedians (paired repetitions):")
  print("scenario version " + " ".join(name for name, _ in columns))
  for scenario in SCENARIOS:
    for version in ("before", "after"):
      rows = [row for row in trials if row["scenario"] == scenario and row["version"] == version]
      values = []
      for _, getter in columns:
        samples = [getter(row) for row in rows if getter(row) is not None]
        values.append("-" if not samples else f"{statistics.median(samples):.2f}")
      print(f"{scenario} {version} " + " ".join(values))


def main():
  args = parse_args()
  validate_inputs(args)
  trials = []
  for scenario in SCENARIOS:
    for repetition in range(args.repetitions):
      # Alternate run order so background thermal drift does not always favor
      # the same version. The two runs still use identical fresh fixtures.
      order = ("before", "after") if repetition % 2 == 0 else ("after", "before")
      for version in order:
        binary = args.before if version == "before" else args.after
        print(f"running {scenario} pair {repetition + 1}/{args.repetitions}: {version}", file=sys.stderr, flush=True)
        trials.append(run_trial(binary, args.interposer, scenario, repetition + 1, version))
  if args.output:
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps({
      "before": str(args.before),
      "after": str(args.after),
      "interposer": str(args.interposer),
      "repetitions": args.repetitions,
      "trials": trials,
    }, indent=2) + "\n", encoding="utf-8")
  print_summary(trials)


if __name__ == "__main__":
  try:
    main()
  except (OSError, RuntimeError, ValueError) as error:
    print(f"error: {error}", file=sys.stderr)
    sys.exit(1)
