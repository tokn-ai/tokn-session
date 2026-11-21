#![cfg(unix)]

use serde_json::{Value, json};
use std::{
  fs,
  os::unix::fs::PermissionsExt,
  path::{Path, PathBuf},
  process::{Command, Output, Stdio},
  time::{Duration, Instant},
};

const SESSION: &str = "11111111-1111-4111-8111-111111111111";
struct Fixture {
  directory: tempfile::TempDir,
  executable: PathBuf,
  project: PathBuf,
  config: PathBuf,
}
impl Fixture {
  fn new() -> Self {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project");
    let config = directory.path().join("config");
    fs::create_dir_all(&project).unwrap();
    let executable = directory.path().join("qoder-fixture");
    fs::write(&executable, SCRIPT.replace("@SESSION@", SESSION)).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    Self {
      directory,
      executable,
      project: project.canonicalize().unwrap(),
      config,
    }
  }
  fn run(&self, mode: &str, arguments: &[&str]) -> Output {
    self.run_with_executor(mode, arguments, self.executable.to_str().unwrap(), &self.config)
  }
  fn run_with_executor(&self, mode: &str, arguments: &[&str], executor: &str, config: &Path) -> Output {
    let (command, arguments) = arguments.split_first().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tokn-session"))
      .arg(command)
      .args(["--source", "qoder", "--executor"])
      .arg(executor)
      .arg("--cwd")
      .arg(&self.project)
      .args(arguments)
      .env("QODER_CONFIG_DIR", config)
      .env("TOKN_QODER_CASE", mode)
      .env("TOKN_QODER_CAPTURE", self.directory.path().join("calls"))
      .env("TOKN_QODER_ARGS", self.directory.path().join("args"))
      .env("TOKN_QODER_STORAGE", self.directory.path().join("storage"))
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
      if Instant::now() > deadline {
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        panic!("ACP CLI hung: {}", String::from_utf8_lossy(&output.stderr));
      }
      std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
  }
  fn calls(&self) -> Vec<Value> {
    fs::read_to_string(self.directory.path().join("calls"))
      .unwrap()
      .lines()
      .map(|line| serde_json::from_str(line).unwrap())
      .collect()
  }
  fn persist(&self) {
    self.persist_at(&self.config);
  }
  fn persist_at(&self, config: &Path) {
    // Native project encoding for this short ASCII temporary test path.
    let project_id: String = self
      .project
      .to_str()
      .unwrap()
      .chars()
      .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
      .collect();
    let folder = config.join("projects").join(project_id);
    fs::create_dir_all(&folder).unwrap();
    let row = json!({"type":"user","uuid":"user-fixture","parentUuid":null,"isSidechain":false,"sessionId":SESSION,"cwd":self.project,"timestamp":"2026-11-20T10:00:00Z","message":{"role":"user","content":"Earlier prompt"}});
    fs::write(folder.join(format!("{SESSION}.jsonl")), format!("{row}\n")).unwrap();
  }
}

#[test]
fn create_uses_acp_and_flag_shaped_prompt_stays_json_content() {
  let fixture = Fixture::new();
  let output = fixture.run("normal", &["create", "--", "--hyphen-prompt"]);
  assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
  let calls = fixture.calls();
  assert_eq!(calls.iter().filter(|call| call["method"] == "session/new").count(), 1);
  let prompt = calls.iter().find(|call| call["method"] == "session/prompt").unwrap();
  assert_eq!(prompt["params"]["prompt"][0]["text"], "--hyphen-prompt");
  assert_eq!(
    fs::read_to_string(fixture.directory.path().join("args")).unwrap(),
    "--acp\n"
  );
  let stdout = String::from_utf8(output.stdout).unwrap();
  assert!(stdout.contains("ACP fixture response"));
  let last: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
  assert_eq!(last["type"], "result");
  assert_eq!(last["stop_reason"], "end_turn");
}

#[test]
fn zero_output_completion_and_explicit_native_termination_do_not_hang_or_claim_success() {
  for mode in ["zero", "cancelled", "refusal", "disconnect", "auth"] {
    let fixture = Fixture::new();
    let output = fixture.run(mode, &["create", "fixture prompt"]);
    assert_eq!(
      output.status.success(),
      mode == "zero",
      "{mode}: {}",
      String::from_utf8_lossy(&output.stderr)
    );
    let calls = fixture.calls();
    assert_eq!(calls.iter().filter(|call| call["method"] == "session/new").count(), 1);
    assert!(calls.iter().filter(|call| call["method"] == "session/prompt").count() <= 1);
    assert_eq!(
      fs::read_to_string(fixture.directory.path().join("args")).unwrap(),
      "--acp\n"
    );
  }
}

#[test]
fn exact_and_latest_append_resume_the_verified_persisted_root() {
  for target in [
    vec!["append", "--session", SESSION, "fixture prompt"],
    vec!["append", "--continue", "fixture prompt"],
  ] {
    let fixture = Fixture::new();
    fixture.persist();
    let output = fixture.run("normal", &target);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let calls = fixture.calls();
    assert!(!calls.iter().any(|call| call["method"] == "session/new"));
    let resume = calls.iter().find(|call| call["method"] == "session/load").unwrap();
    assert_eq!(resume["params"]["sessionId"], SESSION);
    assert_eq!(resume["params"]["cwd"], fixture.project.to_str().unwrap());
    assert_eq!(
      calls.iter().filter(|call| call["method"] == "session/prompt").count(),
      1
    );
  }
}

#[test]
fn non_terminal_permission_is_denied_then_command_exits_without_approval_or_replay() {
  let fixture = Fixture::new();
  let output = fixture.run("permission", &["create", "fixture prompt"]);
  assert!(!output.status.success());
  assert!(String::from_utf8_lossy(&output.stderr).contains("request was denied"));
  let calls = fixture.calls();
  let response = calls
    .iter()
    .find(|call| call["id"] == "permission-fixture" && call.get("result").is_some())
    .unwrap();
  assert_eq!(response["result"]["outcome"]["optionId"], "deny-fixture");
  assert_eq!(
    calls.iter().filter(|call| call["method"] == "session/prompt").count(),
    1
  );
}

#[test]
fn executor_storage_override_is_pinned_in_environment_and_removed_from_native_arguments() {
  for equals_form in [false, true] {
    let fixture = Fixture::new();
    let expected = fixture.project.join("private config");
    let flag = if equals_form {
      "--config-dir=\"private config\""
    } else {
      "--config-dir \"private config\""
    };
    let executor = format!("\"{}\" --verbose {flag}", fixture.executable.display());
    let created = fixture.run_with_executor("normal", &["create", "fixture prompt"], &executor, &fixture.config);
    assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
    assert_eq!(
      fs::read_to_string(fixture.directory.path().join("storage"))
        .unwrap()
        .trim_end(),
      expected.to_str().unwrap()
    );
    assert_eq!(
      fs::read_to_string(fixture.directory.path().join("args")).unwrap(),
      "--verbose\n--acp\n"
    );
    fixture.persist_at(&expected);
    let resumed = fixture.run_with_executor(
      "normal",
      &["append", "--continue", "another prompt"],
      &executor,
      &fixture.config,
    );
    assert!(resumed.status.success(), "{}", String::from_utf8_lossy(&resumed.stderr));
    assert_eq!(
      fs::read_to_string(fixture.directory.path().join("storage"))
        .unwrap()
        .trim_end(),
      expected.to_str().unwrap()
    );
    let calls = fixture.calls();
    assert_eq!(calls.iter().filter(|call| call["method"] == "session/new").count(), 1);
    assert_eq!(calls.iter().filter(|call| call["method"] == "session/load").count(), 1);
    assert_eq!(
      calls.iter().filter(|call| call["method"] == "session/prompt").count(),
      2
    );
  }
}

#[test]
fn inherited_relative_storage_is_pinned_to_the_same_absolute_root_for_create_and_append() {
  let fixture = Fixture::new();
  let expected = fixture.project.join("inherited config");
  let executor = fixture.executable.to_str().unwrap();
  let created = fixture.run_with_executor(
    "zero",
    &["create", "fixture prompt"],
    executor,
    Path::new("inherited config"),
  );
  assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
  assert_eq!(
    fs::read_to_string(fixture.directory.path().join("storage"))
      .unwrap()
      .trim_end(),
    expected.to_str().unwrap()
  );
  fixture.persist_at(&expected);
  let resumed = fixture.run_with_executor(
    "zero",
    &["append", "--session", SESSION, "another prompt"],
    executor,
    Path::new("inherited config"),
  );
  assert!(resumed.status.success(), "{}", String::from_utf8_lossy(&resumed.stderr));
  assert_eq!(
    fs::read_to_string(fixture.directory.path().join("storage"))
      .unwrap()
      .trim_end(),
    expected.to_str().unwrap()
  );
  assert_eq!(
    fs::read_to_string(fixture.directory.path().join("args")).unwrap(),
    "--acp\n"
  );
}

const SCRIPT: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "$TOKN_QODER_ARGS"
printf '%s\n' "$QODER_CONFIG_DIR" > "$TOKN_QODER_STORAGE"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$TOKN_QODER_CAPTURE"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentInfo":{"name":"qoder","version":"1.1.94"},"agentCapabilities":{"loadSession":true}}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      if [ "$TOKN_QODER_CASE" = auth ]; then
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":"Authentication required"}}\n' "$id"
      else
        printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"@SESSION@"}}\n' "$id"
      fi
      ;;
    *'"method":"session/load"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      prompt_id=$id
      if [ "$TOKN_QODER_CASE" = disconnect ]; then exit 0; fi
      if [ "$TOKN_QODER_CASE" = permission ]; then
        printf '{"jsonrpc":"2.0","id":"permission-fixture","method":"session/request_permission","params":{"sessionId":"@SESSION@","toolCall":{"toolCallId":"tool-fixture","title":"Disposable fixture permission","kind":"execute","rawInput":{"command":"fixture only"},"_meta":{"qoder":{"toolName":"Bash"}}},"options":[{"optionId":"allow-fixture","name":"Allow once","kind":"allow_once"},{"optionId":"deny-fixture","name":"Deny","kind":"reject_once"}]}}\n'
      else
        if [ "$TOKN_QODER_CASE" = normal ]; then
          printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"@SESSION@","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"ACP fixture response"}}}}\n'
        fi
        reason=end_turn
        if [ "$TOKN_QODER_CASE" = cancelled ]; then reason=cancelled; fi
        if [ "$TOKN_QODER_CASE" = refusal ]; then reason=refusal; fi
        printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"%s"}}\n' "$prompt_id" "$reason"
      fi
      ;;
    *'"id":"permission-fixture"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"cancelled"}}\n' "$prompt_id"
      ;;
  esac
done
"#;
