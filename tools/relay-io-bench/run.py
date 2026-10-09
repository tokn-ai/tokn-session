#!/usr/bin/env python3
"""Compare two relay binaries on isolated, repeatable I/O workloads.

Build both binaries and the macOS interposer before running this driver. Each
trial gets fresh provider storage. Most trials reset counters after startup;
codex_startup counts launch through readiness and process shutdown. The
counters describe intercepted libc calls and returned bytes, not physical disk
operations or SQLite statement counts.
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
SCENARIOS = (
  "idle", "append", "burst", "opencode_edit", "opencode_inplace_edit",
  "codex_idle", "codex_append", "codex_burst", "codex_new_file", "codex_startup",
)
PI_FILES = 32
BURST_FILES = 16
BURST_LINES_PER_FILE = 8
CODEX_FILES = 128
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
  parser.add_argument("--scenarios", nargs="+", choices=SCENARIOS, default=SCENARIOS, help="workloads to run")
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


def codex_rollout_path(sessions: Path, index: int) -> Path:
  day = index % 8 + 1
  return sessions / "2026" / "01" / f"{day:02d}" / f"rollout-2026-01-{day:02d}T00-00-00-bench-{index:03d}.jsonl"


def codex_session_header(index: int) -> str:
  return json.dumps({
    "timestamp": "2026-01-01T00:00:00Z",
    "type": "session_meta",
    "payload": {
      "id": f"bench-{index:03d}",
      "timestamp": "2026-01-01T00:00:00Z",
      "cwd": "/tmp/relay-io-bench",
      "history_mode": "paginated",
    },
  }, separators=(",", ":")) + "\n"


def codex_message(index: int, ordinal: int) -> str:
  return json.dumps({
    "timestamp": "2026-01-01T00:00:01Z",
    "type": "event_msg",
    "payload": {"type": "user_message", "message": f"benchmark {index}:{ordinal}"},
  }, separators=(",", ":")) + "\n"


def create_codex_fixture(root: Path, scenario: str) -> Path:
  codex_home = root / "codex"
  sessions = codex_home / "sessions"
  # An existing Desktop catalog avoids measuring repeated missing-file checks.
  (codex_home / ".codex-global-state.json").parent.mkdir(parents=True)
  (codex_home / ".codex-global-state.json").write_text("{}\n", encoding="utf-8")
  for index in range(CODEX_FILES):
    path = codex_rollout_path(sessions, index)
    path.parent.mkdir(parents=True, exist_ok=True)
    content = codex_session_header(index)
    if scenario == "codex_startup":
      # A long complete rollout makes both header seeding and tail reads visible.
      content += json.dumps({
        "timestamp": "2026-01-01T00:00:01Z",
        "type": "event_msg",
        "payload": {"type": "user_message", "message": "x" * (16 * 1024)},
      }, separators=(",", ":")) + "\n"
    path.write_text(content, encoding="utf-8")
  return sessions


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
  if scenario in ("opencode_edit", "opencode_inplace_edit"):
    database = root / "opencode.db"
    return None, create_opencode_fixture(database)
  if scenario.startswith("codex_"):
    return create_codex_fixture(root, scenario), None
  pi_dir = root / "pi"
  pi_dir.mkdir()
  for index in range(PI_FILES):
    (pi_dir / f"session_{index:03d}.jsonl").write_text(pi_session_header(index), encoding="utf-8")
  return pi_dir, None


def relay_command(binary: Path, root: Path, jsonl_dir: Path | None, scenario: str) -> list[str]:
  missing = root / "unused"
  opencode_database = root / "opencode.db" if scenario in ("opencode_edit", "opencode_inplace_edit") else missing / "opencode.db"
  codex_dir = jsonl_dir if scenario.startswith("codex_") else missing / "codex"
  pi_dir = jsonl_dir if scenario in ("idle", "append", "burst") else missing / "pi"
  return [
    str(binary), "stdout", "--format", "json",
    "--poll-interval", "200ms" if scenario in ("idle", "codex_idle") else "30s",
    "--codex-dir", str(codex_dir),
    "--pi-dir", str(pi_dir),
    "--opencode-dir", str(opencode_database),
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
  if scenario in ("idle", "codex_idle", "codex_startup"):
    return Counter()
  if scenario == "append":
    return Counter({"pi.bench-000": 1})
  if scenario == "burst":
    return Counter({f"pi.bench-{index:03d}": BURST_LINES_PER_FILE for index in range(BURST_FILES)})
  if scenario in ("opencode_edit", "opencode_inplace_edit"):
    return Counter({"opencode.ses_000": 1})
  if scenario == "codex_append":
    return Counter({"codex.bench-000": 1})
  if scenario == "codex_burst":
    return Counter({f"codex.bench-{index:03d}": BURST_LINES_PER_FILE for index in range(BURST_FILES)})
  if scenario == "codex_new_file":
    return Counter({f"codex.bench-{CODEX_FILES:03d}": 2})
  raise ValueError(f"unknown scenario: {scenario}")


def observed_topics(lines: list[OutputLine], start_index: int) -> Counter[str]:
  return Counter(record.get("topic") for _, record in records_since(lines, start_index))


def expected_codex_records(scenario: str) -> Counter[tuple[str, str, str]]:
  indices = range(BURST_FILES) if scenario == "codex_burst" else (CODEX_FILES if scenario == "codex_new_file" else 0,)
  ordinals = range(BURST_LINES_PER_FILE) if scenario == "codex_burst" else (0,)
  expected = Counter()
  for index in indices:
    offset = len(codex_session_header(index).encode("utf-8"))
    for ordinal in ordinals:
      expected[(f"codex.bench-{index:03d}", f"jsonl:{offset}", f"benchmark {index}:{ordinal}")] += 1
      offset += len(codex_message(index, ordinal).encode("utf-8"))
  return expected


def verify_codex_records(scenario: str, entries: list[tuple[OutputLine, dict]]):
  if scenario not in ("codex_append", "codex_burst", "codex_new_file"):
    return
  observed = Counter()
  header_seen = False
  for _, record in entries:
    events = record.get("events")
    if record.get("operation") != "upsert" or not isinstance(events, list) or len(events) != 1:
      raise RuntimeError(f"unexpected Codex record: {record}")
    if scenario == "codex_new_file" and events[0].get("type") == "session_started":
      if header_seen or record["record_id"] != "jsonl:0" or record["topic"] != f"codex.bench-{CODEX_FILES:03d}":
        raise RuntimeError(f"unexpected Codex session header: {record}")
      header_seen = True
      continue
    if events[0].get("type") != "message" or events[0].get("role") != "user":
      raise RuntimeError(f"unexpected Codex message: {record}")
    observed[(record["topic"], record["record_id"], events[0].get("text"))] += 1
  expected = expected_codex_records(scenario)
  if observed != expected or (scenario == "codex_new_file" and not header_seen):
    raise RuntimeError(f"unexpected Codex records: {observed}; expected {expected}")


def perform_workload(scenario: str, jsonl_dir: Path | None, connection: sqlite3.Connection | None):
  if scenario == "append":
    assert jsonl_dir is not None
    with (jsonl_dir / "session_000.jsonl").open("a", encoding="utf-8") as file:
      file.write(pi_message(0, 0))
  elif scenario == "burst":
    assert jsonl_dir is not None
    for index in range(BURST_FILES):
      with (jsonl_dir / f"session_{index:03d}.jsonl").open("a", encoding="utf-8") as file:
        file.write("".join(pi_message(index, ordinal) for ordinal in range(BURST_LINES_PER_FILE)))
  elif scenario == "codex_append":
    assert jsonl_dir is not None
    with codex_rollout_path(jsonl_dir, 0).open("a", encoding="utf-8") as file:
      file.write(codex_message(0, 0))
  elif scenario == "codex_burst":
    assert jsonl_dir is not None
    for index in range(BURST_FILES):
      with codex_rollout_path(jsonl_dir, index).open("a", encoding="utf-8") as file:
        file.write("".join(codex_message(index, ordinal) for ordinal in range(BURST_LINES_PER_FILE)))
  elif scenario == "codex_new_file":
    assert jsonl_dir is not None
    index = CODEX_FILES
    codex_rollout_path(jsonl_dir, index).write_text(
      codex_session_header(index) + codex_message(index, 0), encoding="utf-8",
    )
  elif scenario in ("opencode_edit", "opencode_inplace_edit"):
    assert connection is not None
    with connection:
      connection.execute(
        "update part set data = ? where id = 'part_000'",
        ('{"type":"text","text":"after 000"}',),
      )
      if scenario == "opencode_edit":
        # The session summary identifies the changed session on the usual path.
        connection.execute(
          "update session set time_updated = ? where id = 'ses_000'",
          (OPENCODE_SESSIONS + 1,),
        )
      # The in-place variant leaves all session summaries unchanged. Relay
      # must inspect message/part content to detect this edit.


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
    jsonl_dir, connection = create_fixture(root, scenario)
    metrics_path = root / "metrics.json"
    env = os.environ.copy()
    env["DYLD_INSERT_LIBRARIES"] = str(interposer)
    env["TOKN_IO_METRICS_PATH"] = str(metrics_path)
    command = relay_command(binary, root, jsonl_dir, scenario)
    before_cpu = resource.getrusage(resource.RUSAGE_CHILDREN)
    process_start_ns = time.perf_counter_ns()
    process = subprocess.Popen(
      command, env=env, stdin=subprocess.DEVNULL,
      stdout=subprocess.PIPE, stderr=subprocess.PIPE,
      text=True, bufsize=1,
    )
    collector = OutputCollector(process)
    try:
      startup_lines = collector.wait_until(
        lambda lines: any(line.stream == "stderr" and line.text.startswith("following ") for line in lines),
        STARTUP_TIMEOUT_SECONDS, "relay startup banner",
      )
      startup_banner_ns = next(
        line.time_ns for line in startup_lines if line.stream == "stderr" and line.text.startswith("following ")
      )
      # In particular, let SQLite's first SHM callback settle before reset.
      # Cold startup only needs time for the CLI's SIGINT handler to install
      # after it prints the ready banner.
      time.sleep(0.02 if scenario == "codex_startup" else 0.5)
      startup_lines = collector.snapshot()
      startup_warnings = [line.text for line in startup_lines if line.stream == "stderr" and line.text.startswith("warning:")]
      if startup_warnings:
        raise RuntimeError(f"native watcher or startup failed: {startup_warnings}")
      if records_since(startup_lines, 0):
        raise RuntimeError("existing fixture unexpectedly emitted records during startup")

      if scenario == "codex_startup":
        # No reset: count process launch, recursive watch registration, header
        # seeding, and tail reads. The banner follows completed initial seeding.
        start_index = 0
        measurement_start_ns = process_start_ns
      else:
        process.send_signal(signal.SIGUSR1)
        time.sleep(0.02)
        start_index = len(collector.snapshot())
        measurement_start_ns = time.perf_counter_ns()
      write_start_ns = None
      write_end_ns = None
      if scenario in ("idle", "codex_idle"):
        time.sleep(2)
      elif scenario == "codex_startup":
        pass
      else:
        write_start_ns = time.perf_counter_ns()
        perform_workload(scenario, jsonl_dir, connection)
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
      verify_codex_records(scenario, entries)
      if scenario in ("opencode_edit", "opencode_inplace_edit") and entries[0][1].get("record_id") != "message:msg_000":
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
        "counter_window": "launch-through-ready-plus-shutdown" if scenario == "codex_startup" else "post-startup-through-shutdown",
        "records": len(entries),
        "warnings": len(warnings),
        "warning_texts": warnings,
        "active_window_s": (stop_ns - measurement_start_ns) / 1e9,
        "measurement_elapsed_s": (exit_ns - measurement_start_ns) / 1e9,
        "startup_ready_ms": (startup_banner_ns - process_start_ns) / 1e6,
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


def print_summary(trials: list[dict], scenarios: list[str] | tuple[str, ...]):
  columns = (
    ("wall_s", lambda row: row["measurement_elapsed_s"]),
    ("startup_ms", lambda row: row["startup_ready_ms"]),
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
  for scenario in scenarios:
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
  for scenario in args.scenarios:
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
      "scenarios": args.scenarios,
      "trials": trials,
    }, indent=2) + "\n", encoding="utf-8")
  print_summary(trials, args.scenarios)


if __name__ == "__main__":
  try:
    main()
  except (OSError, RuntimeError, ValueError) as error:
    print(f"error: {error}", file=sys.stderr)
    sys.exit(1)
