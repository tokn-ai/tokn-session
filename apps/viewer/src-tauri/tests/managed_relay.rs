//! Exercise the bundled stdio child without a GUI or the user's provider roots.
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
  io::{AsyncBufReadExt, BufReader},
  process::{Child, ChildStdout, Command},
};
use tokn_session_relay::stdio::{MAX_LINE_BYTES, VERSION};

const BINARY: &str = env!("CARGO_BIN_EXE_tokn-session-viewer");
const HEADER: &str =
  "{\"type\":\"session\",\"id\":\"managed-fixture\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}\n";
const MESSAGE: &str = "{\"type\":\"message\",\"id\":\"one\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n";
fn command(root: &Path, native: bool) -> Command {
  std::fs::create_dir_all(root.join("codex/sessions")).unwrap();
  std::fs::create_dir_all(root.join("pi")).unwrap();
  let mut command = Command::new(std::env::var_os("TOKN_VIEWER_TEST_BINARY").unwrap_or_else(|| BINARY.into()));
  command
    .arg(tokn_session_relay::stdio::CHILD_FLAG)
    .env("CODEX_HOME", root.join("codex"))
    .env("PI_CODING_AGENT_SESSION_DIR", root.join("pi"))
    .env("OPENCODE_DB", root.join("missing-opencode.db"))
    .env("ZCODE_STORAGE_DIR", root.join("zcode"))
    .env("WORKBUDDY_CONFIG_DIR", root.join("workbuddy"))
    .env("DSH_HOME", root.join("dsh"))
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit())
    .kill_on_drop(true);
  if native {
    command.arg("--native");
  }
  command
}
async fn ready(child: &mut Child) -> BufReader<ChildStdout> {
  let mut output = BufReader::new(child.stdout.take().unwrap());
  let mut line = String::new();
  tokio::time::timeout(Duration::from_secs(10), output.read_line(&mut line))
    .await
    .unwrap()
    .unwrap();
  let value: serde_json::Value = serde_json::from_str(&line).unwrap();
  assert_eq!(value, serde_json::json!({"type":"ready", "version":VERSION}));
  output
}
async fn start(root: &Path, native: bool) -> (Child, BufReader<ChildStdout>) {
  let mut child = command(root, native).spawn().unwrap();
  let output = ready(&mut child).await;
  (child, output)
}
async fn stop(child: &mut Child) {
  drop(child.stdin.take());
  assert!(
    tokio::time::timeout(Duration::from_secs(5), child.wait())
      .await
      .expect("Relay did not exit on stdin EOF")
      .unwrap()
      .success()
  );
}
#[tokio::test]
async fn packaged_child_streams_compact_session_hints_with_optional_native_flag_and_exits_on_eof() {
  for native in [false, true] {
    let root = tempfile::tempdir().unwrap();
    let (mut child, output) = start(root.path(), native).await;
    let mut lines = output.lines();
    // `ready` acknowledges the transport before initial discovery/EOF seeding.
    // Files created during that seed can legitimately become baseline history.
    // Establish live delivery using fresh probe files before testing a new file.
    tokio::time::timeout(Duration::from_secs(10), async {
      let mut tick = tokio::time::interval(Duration::from_millis(100));
      let mut probe = 0;
      loop {
        tokio::select! {
          line = lines.next_line() => {
            let value: serde_json::Value = serde_json::from_str(&line.unwrap().expect("Relay exited during live probe")).unwrap();
            if value["sessions"].as_array().is_some_and(|sessions| {
              sessions.iter().any(|hint| hint["provider"] == "pi")
            }) { break; }
          }
          _ = tick.tick() => {
            let header = HEADER.replace("managed-fixture", &format!("probe-{probe}"));
            std::fs::write(root.path().join(format!("pi/probe-{probe}.jsonl")), format!("{header}{MESSAGE}")).unwrap();
            probe += 1;
          }
        }
      }
    })
    .await
    .expect("Relay did not begin live delivery after transport readiness");
    let path = root.path().join("pi/session.jsonl");
    std::fs::write(&path, format!("{HEADER}{MESSAGE}")).unwrap();
    let (changes, hint) = tokio::time::timeout(Duration::from_secs(10), async {
      loop {
        let line = lines
          .next_line()
          .await
          .unwrap()
          .expect("Relay exited before delivering the new file");
        assert!(line.len() < MAX_LINE_BYTES);
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        if let Some(hint) = value["sessions"].as_array().and_then(|sessions| {
          sessions
            .iter()
            .find(|hint| hint["provider"] == "pi" && hint["session_id"] == "managed-fixture")
        }) {
          break (value.clone(), hint.clone());
        }
      }
    })
    .await
    .unwrap();
    assert_eq!(
      changes.as_object().unwrap().len(),
      1,
      "managed pipe carries only session hints"
    );
    assert_eq!(
      hint.as_object().unwrap().len(),
      3,
      "native records and events stay out of the pipe"
    );
    assert_eq!(hint["path"], serde_json::json!(path));
    stop(&mut child).await;
  }
}
#[tokio::test]
async fn independent_children_have_independent_lifetimes() {
  let root = tempfile::tempdir().unwrap();
  let (mut first, _first_output) = start(root.path(), false).await;
  let (mut second, _second_output) = start(root.path(), false).await;
  stop(&mut first).await;
  assert!(second.try_wait().unwrap().is_none());
  stop(&mut second).await;
}
#[tokio::test]
async fn invalid_configuration_fails_before_readiness() {
  let root = tempfile::tempdir().unwrap();
  let mut child = command(root.path(), false)
    .env("OPENCODE_DB", ":memory:")
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let _lifetime = child.stdin.take();
  let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
    .await
    .unwrap()
    .unwrap();
  assert!(!output.status.success());
  assert!(output.stdout.is_empty());
  assert!(String::from_utf8_lossy(&output.stderr).contains(":memory:"));
}
#[tokio::test]
async fn unsupported_child_arguments_fail_before_readiness() {
  let root = tempfile::tempdir().unwrap();
  let output = tokio::time::timeout(
    Duration::from_secs(5),
    command(root.path(), false).arg("--unexpected").output(),
  )
  .await
  .unwrap()
  .unwrap();
  assert_eq!(output.status.code(), Some(2));
  assert!(output.stdout.is_empty());
}
#[cfg(unix)]
#[test]
fn parent_fixture() {
  use std::{io::Write, os::fd::AsFd};
  let Some(root) = std::env::var_os("TOKN_RELAY_PARENT_FIXTURE_ROOT") else {
    return;
  };
  let runtime = tokio::runtime::Runtime::new().unwrap();
  // Keep the helper's output pipe open in the child too. EOF then proves that
  // both processes exited, without relying on platform-specific zombie checks.
  let (_child, _output) = runtime.block_on(async {
    let mut child = command(Path::new(&root), false)
      .stderr(Stdio::from(std::io::stdout().as_fd().try_clone_to_owned().unwrap()))
      .spawn()
      .unwrap();
    let output = ready(&mut child).await;
    (child, output)
  });
  println!("CHILD_READY");
  std::io::stdout().flush().unwrap();
  loop {
    std::thread::park();
  }
}
#[cfg(unix)]
#[tokio::test]
async fn relay_exits_after_parent_death_without_cleanup() {
  let root = tempfile::tempdir().unwrap();
  let mut parent = Command::new(std::env::current_exe().unwrap())
    .args(["--exact", "parent_fixture", "--nocapture"])
    .env("TOKN_RELAY_PARENT_FIXTURE_ROOT", root.path())
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit())
    .kill_on_drop(true)
    .spawn()
    .unwrap();
  let mut lines = BufReader::new(parent.stdout.take().unwrap()).lines();
  tokio::time::timeout(Duration::from_secs(10), async {
    while lines.next_line().await.unwrap().expect("parent fixture exited") != "CHILD_READY" {}
  })
  .await
  .unwrap();
  parent.kill().await.unwrap();
  tokio::time::timeout(Duration::from_secs(5), async {
    while lines.next_line().await.unwrap().is_some() {}
  })
  .await
  .expect("orphaned Relay kept the pipe open");
}
