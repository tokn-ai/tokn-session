use std::path::PathBuf;
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
  Command::new(env!("CARGO_BIN_EXE_tokn-session"))
    .args(args)
    .output()
    .unwrap()
}

fn assert_help(output: Output) {
  assert_eq!(output.status.code(), Some(0));
  assert!(output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
  let stdout = String::from_utf8(output.stdout).unwrap();
  assert!(stdout.starts_with("usage:\n"));
  for command in ["list", "show", "browse", "create", "append"] {
    assert!(stdout.contains(&format!("tokn-session {command} ")));
  }
}

fn assert_error(output: Output, expected: &str) {
  assert_eq!(output.status.code(), Some(1));
  assert!(output.stdout.is_empty());
  let stderr = String::from_utf8(output.stderr).unwrap();
  assert!(stderr.starts_with("error: "));
  assert!(stderr.contains(expected), "{stderr}");
}

#[test]
fn top_level_help_succeeds_on_stdout() {
  for argument in ["--help", "-h", "help"] {
    assert_help(run(&[argument]));
  }
}

#[test]
fn every_subcommand_supports_help_without_required_arguments() {
  for command in ["list", "show", "browse", "create", "append"] {
    for argument in ["--help", "-h"] {
      assert_help(run(&[command, argument]));
    }
  }
}

#[test]
fn help_after_valid_options_skips_execution_and_required_arguments() {
  assert_help(run(&["show", "--source", "dsh", "--format", "jsonl", "--help"]));
  assert_help(run(&["append", "--continue", "-h"]));
  assert_help(run(&["create", "--executor", "nonexistent-executor", "--help"]));
}

#[test]
fn empty_and_invalid_invocations_remain_errors() {
  for (args, expected) in [
    (vec![], "usage:"),
    (vec!["unknown", "--help"], "unknown command `unknown`"),
    (vec!["show"], "show requires exactly one session id or path"),
    (vec!["list", "--unknown", "--help"], "unknown option `--unknown`"),
    (vec!["list", "--source"], "--source requires a value"),
  ] {
    assert_error(run(&args), expected);
  }
}

#[test]
fn help_shaped_typed_flag_values_keep_their_validation_errors() {
  for (args, expected) in [
    (vec!["list", "--source", "--help"], "unknown source `--help`"),
    (vec!["show", "--format", "-h"], "unknown format `-h`"),
    (vec!["list", "--limit", "--help"], "invalid --limit value `--help`"),
    (vec!["show", "--scope", "-h"], "unknown show scope `-h`"),
  ] {
    assert_error(run(&args), expected);
  }
}

#[test]
fn help_shaped_string_flag_values_do_not_trigger_help() {
  let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dsh");
  for (flag, value) in [("--executor", "--help"), ("--cwd", "-h")] {
    let output = Command::new(env!("CARGO_BIN_EXE_tokn-session"))
      .args(["show", "--source", "dsh", flag, value, "dsh-fixture", "--session-dir"])
      .arg(&fixtures)
      .output()
      .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("All done."));
    assert!(!stdout.contains("usage:"));
  }

  let output = Command::new(env!("CARGO_BIN_EXE_tokn-session"))
    .args(["list", "--source", "dsh", "--session-dir", "--help"])
    .current_dir(fixtures)
    .output()
    .unwrap();
  assert_eq!(output.status.code(), Some(0));
  assert!(output.stderr.is_empty());
  let stdout = String::from_utf8(output.stdout).unwrap();
  assert!(stdout.starts_with("id "));
  assert!(!stdout.contains("usage:"));
}

#[test]
fn session_values_and_help_positionals_reach_normal_command_validation() {
  assert_error(
    run(&["append", "--source", "dsh", "--session", "--help", "prompt"]),
    "create/append are not implemented",
  );
  assert_error(
    run(&["create", "--source", "dsh", "help"]),
    "create/append are not implemented",
  );
}
