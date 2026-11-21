use serde_json::Value;
use std::{
  path::{Path, PathBuf},
  process::Command,
};

fn fixture() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/qoder")
}

#[test]
fn qoder_list_and_show_support_ids_paths_pretty_and_jsonl() {
  for (command, tail, expected) in [
    ("list", vec![], "session-cli"),
    ("show", vec!["session-cli"], "Qoder fixture response."),
  ] {
    let output = Command::new(env!("CARGO_BIN_EXE_tokn-session"))
      .arg(command)
      .args(["--source", "qoder", "--session-dir"])
      .arg(fixture())
      .args(tail)
      .output()
      .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains(expected));
  }
  let output = Command::new(env!("CARGO_BIN_EXE_tokn-session"))
    .args(["show", "--source", "qoder", "--format", "jsonl"])
    .arg(fixture().join("session-cli.jsonl"))
    .output()
    .unwrap();
  assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
  let events: Vec<Value> = String::from_utf8(output.stdout)
    .unwrap()
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect();
  assert!(events.iter().all(|event| event["provider"] == "qoder"));
  assert!(
    events
      .iter()
      .any(|event| event["type"] == "unknown" && event["native_type"] == "future-cli-record")
  );
}

#[test]
fn qoder_help_and_prompt_placeholder_policy_are_explicit() {
  let help = Command::new(env!("CARGO_BIN_EXE_tokn-session"))
    .arg("--help")
    .output()
    .unwrap();
  assert!(String::from_utf8_lossy(&help.stdout).contains("qoder"));
  let output = Command::new(env!("CARGO_BIN_EXE_tokn-session"))
    .args([
      "create",
      "--source",
      "qoder",
      "--executor",
      "nonexistent {prompt}",
      "--",
      "--hyphen-prompt",
    ])
    .output()
    .unwrap();
  assert!(!output.status.success());
  assert!(String::from_utf8_lossy(&output.stderr).contains("prompts over JSON-RPC"));
}
