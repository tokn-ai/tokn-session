#![cfg(target_os = "macos")]

use std::{
  fs::OpenOptions,
  io::{BufRead, BufReader, Read, Write},
  path::Path,
  process::{Child, Command, Stdio},
  sync::mpsc::{self, Receiver, Sender},
  thread::{self, JoinHandle},
  time::{Duration, Instant},
};

use tempfile::TempDir;

#[test]
fn exhausted_native_watches_keep_following_through_polling() {
  let fixture = TempDir::new().unwrap();
  let sessions = fixture.path().join("pi sessions");
  std::fs::create_dir(&sessions).unwrap();
  // Recursive kqueue watching opens every entry, exceeding the descriptor
  // limit inherited by a typical macOS GUI process without touching real data.
  for index in 0..320 {
    let header = serde_json::json!({
      "type": "session",
      "id": format!("watch-limit-{index}"),
      "timestamp": "2026-01-01T00:00:00Z",
    });
    std::fs::write(sessions.join(format!("session_{index}.jsonl")), format!("{header}\n")).unwrap();
  }

  let mut command = Command::new("/bin/sh");
  command
    .args(["-c", "ulimit -n 256 || exit; exec \"$@\"", "relay-watch-limit"])
    .arg(env!("CARGO_BIN_EXE_tokn-session-relay"))
    .args(["stdout", "--format", "json", "--poll-interval", "100ms", "--pi-dir"])
    .arg(&sessions);
  // Every provider override is explicit, so neither startup nor recovery can
  // resolve a developer's normal session storage through environment defaults.
  for provider in ["codex", "opencode", "zcode", "qoder", "workbuddy", "dsh"] {
    command
      .arg(format!("--{provider}-dir"))
      .arg(fixture.path().join(format!("missing-{provider}")));
  }
  let mut relay = RelayProcess::spawn(command);

  // The startup diagnostic follows cursor seeding; waiting for it and the
  // fallback warning makes the first append unambiguously new session data.
  relay.wait_until(Duration::from_secs(10), |output| {
    output
      .iter()
      .any(|line| line.stderr && line.text.starts_with("following "))
      && output.iter().any(is_polling_warning)
  });

  let path = sessions.join("session_0.jsonl");
  for (id, text) in [
    ("first", "first append after watch exhaustion"),
    ("second", "polling still follows later appends"),
  ] {
    append_message(&path, id, text);
    relay.wait_until(Duration::from_secs(5), |output| {
      output.iter().any(|line| {
        if line.stderr {
          return false;
        }
        let record: serde_json::Value = serde_json::from_str(&line.text).expect("stdout must contain JSON records");
        record["topic"] == "pi.watch-limit-0"
          && record["events"].as_array().is_some_and(|events| {
            events
              .iter()
              .any(|event| event["type"] == "message" && event["text"] == text)
          })
      })
    });
  }

  // Observe several more fallback ticks: a failed native registration must
  // not restart or emit the same warning on every polling scan.
  relay.collect_for(Duration::from_millis(450));
  assert!(
    relay.child.try_wait().unwrap().is_none(),
    "Relay exited unexpectedly: {:?}",
    relay.output
  );
  relay.stop();
  assert_eq!(
    relay.output.iter().filter(|line| is_polling_warning(line)).count(),
    1,
    "{:?}",
    relay.output
  );
  assert!(
    relay
      .output
      .iter()
      .any(|line| { line.stderr && (line.text.contains("Too many open files") || line.text.contains("os error 24")) }),
    "the test must exercise descriptor exhaustion: {:?}",
    relay.output
  );
  assert!(
    !relay.output.iter().any(|line| line.text.contains("Relay stopped")),
    "{:?}",
    relay.output
  );
}

fn append_message(path: &Path, id: &str, text: &str) {
  let message = serde_json::json!({
    "type": "message",
    "id": id,
    "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
  });
  let mut file = OpenOptions::new().append(true).open(path).unwrap();
  writeln!(file, "{message}").unwrap();
  file.flush().unwrap();
}

fn is_polling_warning(line: &OutputLine) -> bool {
  line.stderr && line.text.contains("polling") && line.text.contains("watch")
}

#[derive(Debug)]
struct OutputLine {
  stderr: bool,
  text: String,
}

struct RelayProcess {
  child: Child,
  lines: Receiver<OutputLine>,
  readers: Vec<JoinHandle<()>>,
  output: Vec<OutputLine>,
}

impl RelayProcess {
  fn spawn(mut command: Command) -> Self {
    let (sender, lines) = mpsc::channel();
    let child = command
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .unwrap();
    let mut process = Self {
      child,
      lines,
      readers: Vec::new(),
      output: Vec::new(),
    };
    let stdout = process.child.stdout.take().unwrap();
    let stderr = process.child.stderr.take().unwrap();
    process.readers.push(read_lines(stdout, false, sender.clone()));
    process.readers.push(read_lines(stderr, true, sender));
    process
  }

  fn wait_until(&mut self, timeout: Duration, predicate: impl Fn(&[OutputLine]) -> bool) {
    let deadline = Instant::now() + timeout;
    while !predicate(&self.output) {
      assert!(
        Instant::now() < deadline,
        "timed out waiting for Relay output: {:?}",
        self.output
      );
      match self
        .lines
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
      {
        Ok(line) => self.output.push(line),
        Err(error) => panic!("Relay output did not meet the expectation: {error}; {:?}", self.output),
      }
    }
  }

  fn collect_for(&mut self, duration: Duration) {
    let deadline = Instant::now() + duration;
    while let Ok(line) = self
      .lines
      .recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
      self.output.push(line);
      if Instant::now() >= deadline {
        break;
      }
    }
  }

  fn stop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
    for reader in self.readers.drain(..) {
      let _ = reader.join();
    }
    self.output.extend(self.lines.try_iter());
  }
}

impl Drop for RelayProcess {
  fn drop(&mut self) {
    self.stop();
  }
}

fn read_lines(reader: impl Read + Send + 'static, stderr: bool, sender: Sender<OutputLine>) -> JoinHandle<()> {
  thread::spawn(move || {
    for line in BufReader::new(reader).lines() {
      let text = line.unwrap_or_else(|error| format!("failed to read Relay output: {error}"));
      if sender.send(OutputLine { stderr, text }).is_err() {
        break;
      }
    }
  })
}
