use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

fn fixture() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workbuddy")
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
fn lists_workbuddy_and_renders_pretty_and_jsonl() {
  let listing = run(&["list", "--source", "workbuddy"]);
  assert!(listing.contains("wb-chat-basic"));
  assert!(listing.contains("/fixture/workspace"));

  let pretty = run(&["show", "--source", "workbuddy", "wb-chat-basic"]);
  assert!(pretty.contains("provider-agnostic agent session layer"));

  let jsonl = run(&["show", "--source", "workbuddy", "wb-chat-basic", "--format", "jsonl"]);
  let events: Vec<Value> = jsonl.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
  assert!(!events.is_empty());
  assert!(events.iter().all(|event| event["provider"] == "workbuddy"));
}

#[test]
fn explicit_workbuddy_history_path_works() {
  let path = fixture().join("projects/fixture-workspace/wb-chat-basic.jsonl");
  let pretty = run(&[
    "show",
    "--source",
    "workbuddy",
    path.to_str().unwrap(),
    "--scope",
    "tree",
  ]);
  assert!(pretty.contains("wb-chat-basic"));
  assert!(pretty.contains("provider-agnostic agent session layer"));
}

#[test]
fn rejects_workbuddy_create_and_append_without_launching_an_executor() {
  for args in [vec!["create", "hello"], vec!["append", "--continue", "hello"]] {
    let output = Command::new(env!("CARGO_BIN_EXE_tokn-session"))
      .args(args)
      .args(["--source", "workbuddy", "--executor", "nonexistent-executor"])
      .output()
      .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("create/append are not implemented"));
  }
}
