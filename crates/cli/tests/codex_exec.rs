#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn fake_codex(directory: &Path) -> PathBuf {
  let path = directory.join("Codex fixture executable");
  fs::write(
    &path,
    "#!/bin/sh\nprintf '%s\\n' \"$@\"\nprintf '%s\\n' '[prompt]'\ncat\n",
  )
  .unwrap();
  fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
  path
}

fn run_codex(executable: &Path, executor: Option<&str>, command: &str, target: &[&str], prompt: &str) -> Output {
  let mut cli = Command::new(env!("CARGO_BIN_EXE_tokn-session"));
  cli
    .args([command, "--source", "codex"])
    .env_remove("TOKN_SESSION_CODEX_EXECUTOR")
    .env_remove("TOKN_SESSION_EXECUTOR")
    .env("TOKN_SESSION_CODEX_EXECUTABLE", executable);
  if let Some(executor) = executor {
    cli.args(["--executor", executor]);
  }
  let mut child = cli
    .args(target)
    .args(["--", prompt])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  child.stdin.take().unwrap().write_all(b"caller stdin").unwrap();
  child.wait_with_output().unwrap()
}

#[test]
fn codex_create_and_append_resolve_a_local_executable_and_deliver_only_the_prompt() {
  let directory = tempfile::tempdir().unwrap();
  let executable = fake_codex(directory.path());
  for (command, target, prompt, arguments) in [
    ("create", &[][..], "--help", "exec\n--json\n-\n"),
    ("append", &["--continue"][..], "-", "exec\n--json\nresume\n--last\n-\n"),
    (
      "append",
      &["--session", "fixture session"][..],
      "First line\n  第二行\n",
      "exec\n--json\nresume\nfixture session\n-\n",
    ),
  ] {
    let output = run_codex(&executable, None, command, target, prompt);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, format!("{arguments}[prompt]\n{prompt}").as_bytes());
  }
}

#[test]
fn codex_executor_wrapper_preserves_precedence_and_prompt_placeholders() {
  let directory = tempfile::tempdir().unwrap();
  let executable = fake_codex(directory.path());
  let unavailable = directory.path().join("missing");
  let wrapper = format!("\"{}\" --wrapper", executable.display());
  let output = run_codex(&unavailable, Some(&wrapper), "create", &[], "resume");
  assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
  assert_eq!(output.stdout, b"--wrapper\nexec\n--json\n-\n[prompt]\nresume");

  let wrapper = format!("{wrapper} {{prompt}}");
  let output = run_codex(&unavailable, Some(&wrapper), "create", &[], "resume");
  assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
  assert_eq!(output.stdout, b"--wrapper\nresume\n[prompt]\ncaller stdin");
}

#[test]
fn codex_rejects_a_flag_as_an_explicit_session_before_launching() {
  let directory = tempfile::tempdir().unwrap();
  let executable = fake_codex(directory.path());
  let output = run_codex(&executable, None, "append", &["--session", "--last"], "next");
  assert!(!output.status.success());
  assert!(output.stdout.is_empty());
  assert!(String::from_utf8_lossy(&output.stderr).contains("session ID that does not start with '-'"));
}
