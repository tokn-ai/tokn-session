use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

fn fixture() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dsh")
}

fn run(args: &[&str]) -> String {
  let output = Command::new(env!("CARGO_BIN_EXE_tokn-session"))
    .args(args)
    .arg("--session-dir")
    .arg(fixture())
    .output()
    .unwrap();
  assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
  String::from_utf8(output.stdout).unwrap()
}

#[test]
fn lists_dsh_and_renders_pretty_and_jsonl() {
  let listing = run(&["list", "--source", "dsh", "--limit", "1"]);
  assert!(listing.contains("dsh-fixture"));
  assert!(listing.contains("/project/demo"));
  let pretty = run(&["show", "--source", "dsh", "dsh-fixture"]);
  assert!(pretty.contains("All done."));
  assert!(pretty.contains("guide.md"));
  assert!(pretty.contains("[turn 1] completed"));
  assert!(pretty.contains("[turn 1 step 1] ended"));
  assert!(pretty.contains("[usage] input=10 output=3"));
  assert!(pretty.contains("[session/title] title: Reading a guide"));
  assert!(!pretty.contains("unknown turn/start"));
  assert!(!pretty.contains("assistant/message.usage"));
  assert!(pretty.contains("unknown plugin/future"));
  assert!(pretty.contains("unknown user/message"));
  let jsonl = run(&["show", "--source", "dsh", "dsh-fixture", "--format", "jsonl"]);
  let events: Vec<Value> = jsonl.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
  assert!(events.iter().all(|event| event["provider"] == "dsh"));
  assert_eq!(events.iter().filter(|event| event["type"] == "unknown").count(), 2);
  assert_eq!(events.iter().filter(|event| event["type"] == "usage").count(), 1);
  assert_eq!(events.iter().filter(|event| event["type"] == "lifecycle").count(), 8);
  assert_eq!(
    events
      .iter()
      .filter(|event| event["type"] == "message" && event["text"] == "All done.")
      .count(),
    1
  );
  assert_eq!(
    events
      .iter()
      .filter(|event| event["type"] == "tool_call" && event["phase"] == "started")
      .count(),
    1
  );
}

#[test]
fn explicit_dsh_file_and_tree_scope_work() {
  let path = fixture().join("basic/session.jsonl");
  let pretty = run(&["show", "--source", "dsh", path.to_str().unwrap(), "--scope", "tree"]);
  assert!(pretty.contains("dsh-fixture"));
  assert!(pretty.contains("All done."));
}

#[test]
fn rejects_dsh_create_and_append_without_launching_an_executor() {
  for args in [vec!["create", "hello"], vec!["append", "--continue", "hello"]] {
    let output = Command::new(env!("CARGO_BIN_EXE_tokn-session"))
      .args(args)
      .args(["--source", "dsh", "--executor", "nonexistent-executor"])
      .output()
      .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("create/append are not implemented"));
  }
}
