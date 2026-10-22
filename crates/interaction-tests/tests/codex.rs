//! Genuine Codex app-server RPC against local Responses inference, and replay
//! of the captured bidirectional JSONL protocol without an installed agent.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
  path::{Path, PathBuf},
  pin::Pin,
  process::Stdio,
  sync::{Arc, Mutex},
  task::{Context, Poll},
  time::Duration,
};
use tokio::{
  io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf},
  sync::broadcast,
};
use tokn_codex_adapter::{CodexAdapter, EndpointOptions, ThreadOptions};
use tokn_interaction_core::*;
use tokn_interaction_tests::{ModelGate, ModelServer, ModelStep, TestResult};

const VERSION: &str = "codex-cli 0.163.0-alpha.11";
// This is the upstream protocol source inspected while implementing the
// adapter. It is not asserted to be the proprietary Desktop binary's source.
const PROTOCOL_REVISION: &str = "2351d9e1b608e6f9d9a3699b71d7eb39ee41cfa4";
const INSTANCE: &str = "codex-fixture-instance";

fn fixture_path() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/codex/app-server.json")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Frame {
  direction: String,
  body: Value,
}
#[derive(Serialize, Deserialize)]
struct Fixture {
  schema_version: u32,
  provider: String,
  agent_version: String,
  executable_sha256: String,
  inspected_protocol_revision: String,
  scenario: String,
  frames: Vec<Frame>,
}

type Frames = Arc<Mutex<Vec<Frame>>>;
struct Capture<T> {
  inner: T,
  frames: Frames,
  buffer: Vec<u8>,
  direction: &'static str,
}
impl<T> Capture<T> {
  fn new(inner: T, frames: Frames, direction: &'static str) -> Self {
    Self {
      inner,
      frames,
      buffer: Vec::new(),
      direction,
    }
  }
  fn record(&mut self, bytes: &[u8]) {
    self.buffer.extend_from_slice(bytes);
    while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
      let bytes: Vec<_> = self.buffer.drain(..=end).collect();
      let body = serde_json::from_slice(&bytes).expect("real Codex emitted non-JSON stdout");
      self.frames.lock().unwrap().push(Frame {
        direction: self.direction.into(),
        body,
      });
    }
  }
}
impl<T: AsyncRead + Unpin> AsyncRead for Capture<T> {
  fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
    let before = buf.filled().len();
    match Pin::new(&mut self.inner).poll_read(cx, buf) {
      Poll::Ready(Ok(())) => {
        self.record(&buf.filled()[before..]);
        Poll::Ready(Ok(()))
      }
      result => result,
    }
  }
}
impl<T: AsyncWrite + Unpin> AsyncWrite for Capture<T> {
  fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<std::io::Result<usize>> {
    match Pin::new(&mut self.inner).poll_write(cx, bytes) {
      Poll::Ready(Ok(length)) => {
        self.record(&bytes[..length]);
        Poll::Ready(Ok(length))
      }
      result => result,
    }
  }
  fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
    Pin::new(&mut self.inner).poll_flush(cx)
  }
  fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
    Pin::new(&mut self.inner).poll_shutdown(cx)
  }
}

fn options() -> EndpointOptions {
  let mut options = EndpointOptions::new("codex-fixture");
  options.instance_id = INSTANCE.into();
  options.request_timeout = Duration::from_secs(15);
  options
}
fn request(target: &SessionTarget, id: &str, command: Command) -> InteractionRequest {
  InteractionRequest {
    request_id: id.into(),
    target: target.clone(),
    command,
  }
}
fn message(text: &str, delivery: DeliveryMode) -> Command {
  Command::Message {
    input: MessageInput {
      text: text.into(),
      ..MessageInput::default()
    },
    delivery,
  }
}
async fn next_matching<F>(events: &mut broadcast::Receiver<RuntimeEvent>, predicate: F) -> TestResult<RuntimeEvent>
where
  F: Fn(&RuntimeEvent) -> bool,
{
  Ok(
    tokio::time::timeout(Duration::from_secs(20), async {
      loop {
        let event = events.recv().await?;
        if predicate(&event) {
          return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(event);
        }
      }
    })
    .await??,
  )
}
async fn completed(events: &mut broadcast::Receiver<RuntimeEvent>, text: &str) -> TestResult {
  next_matching(events, |event| matches!(&event.kind, RuntimeEventKind::Native(value)
    if value["method"] == "item/completed" && value["params"]["item"]["text"].as_str().is_some_and(|value| value.contains(text)))).await?;
  next_matching(events, |event| {
    matches!(&event.kind, RuntimeEventKind::Native(value)
    if value["method"] == "turn/completed")
  })
  .await?;
  Ok(())
}
async fn working(events: &mut broadcast::Receiver<RuntimeEvent>) -> TestResult {
  next_matching(events, |event| matches!(&event.kind, RuntimeEventKind::Native(value)
    if value["method"] == "item/agentMessage/delta" && value["params"]["delta"].as_str().is_some_and(|value| value.contains("working")))).await?;
  Ok(())
}
async fn admission(
  adapter: &CodexAdapter,
  target: &SessionTarget,
  id: &str,
  text: &str,
  delivery: DeliveryMode,
) -> TestResult {
  assert!(matches!(
    adapter.execute(request(target, id, message(text, delivery))).await?,
    Reply::Admission(_)
  ));
  Ok(())
}

async fn exercise(adapter: CodexAdapter, work: &Path, gate: Option<ModelGate>) -> TestResult {
  let mut events = adapter.subscribe();
  let target = adapter
    .create_thread(ThreadOptions {
      cwd: Some(work.into()),
      model: Some("fixture-model".into()),
      model_provider: Some("fixture".into()),
      ..ThreadOptions::default()
    })
    .await?;
  let Reply::Metadata(metadata) = adapter.execute(request(&target, "metadata", Command::Metadata)).await? else {
    panic!("missing Codex metadata")
  };
  assert_eq!(metadata.model.unwrap().model_id, "fixture-model");
  let Reply::State(state) = adapter.execute(request(&target, "state", Command::State)).await? else {
    panic!("missing Codex state")
  };
  assert_eq!(state.status, ExecutionState::Idle);
  assert_eq!(state.session_id, target.session_id);
  let Reply::Models(models) = adapter.execute(request(&target, "models", Command::Models)).await? else {
    panic!("missing Codex model catalog")
  };
  assert!(!models.is_empty());

  // Native turn/start is atomic start-or-steer. Do not pretend it guarantees
  // strict idle-only admission, or emulate a queue with transport-local state.
  for (id, mode) in [("strict-start", DeliveryMode::Start), ("queue", DeliveryMode::Queue)] {
    let error = adapter
      .execute(request(&target, id, message("must not send", mode)))
      .await
      .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unsupported);
  }
  admission(&adapter, &target, "hello", "fixture hello", DeliveryMode::StartOrSteer).await?;
  completed(&mut events, "fixture hello complete").await?;
  // Repeating an admission receipt must not deliver the prompt again.
  admission(&adapter, &target, "hello", "fixture hello", DeliveryMode::StartOrSteer).await?;

  admission(
    &adapter,
    &target,
    "question",
    "fixture ask question",
    DeliveryMode::StartOrSteer,
  )
  .await?;
  let event = next_matching(&mut events, |event| {
    matches!(&event.kind, RuntimeEventKind::InteractionRequested(_))
  })
  .await?;
  let RuntimeEventKind::InteractionRequested(question) = event.kind else {
    unreachable!()
  };
  assert_eq!(question.kind, InteractionKind::Question);
  assert_eq!(question.questions.len(), 2);
  assert!(question.questions[0].allows_free_text);
  assert!(question.questions[0].allows_option_notes);
  assert_eq!(question.questions[0].question_id, "fixture_choice");
  assert_eq!(
    question.questions[0]
      .options
      .iter()
      .map(|option| option.label.as_str())
      .collect::<Vec<_>>(),
    ["Alpha", "Beta"]
  );
  let Reply::Interactions(pending) = adapter
    .execute(request(&target, "pending-question", Command::PendingInteractions))
    .await?
  else {
    panic!("missing pending question")
  };
  assert_eq!(pending.len(), 1);
  assert!(matches!(
    adapter
      .execute(request(
        &target,
        "answer",
        Command::Respond {
          interaction_id: question.interaction_id,
          response: InteractionResponse::Answers {
            answers: vec![
              QuestionAnswer {
                question_id: "fixture_choice".into(),
                answers: vec!["Beta".into()],
                custom: Some("fixture option note".into())
              },
              QuestionAnswer {
                question_id: "fixture_notes".into(),
                answers: Vec::new(),
                custom: Some("fixture custom answer".into())
              }
            ]
          }
        }
      ))
      .await?,
    Reply::Acknowledged
  ));
  completed(&mut events, "fixture question complete").await?;

  admission(
    &adapter,
    &target,
    "permission",
    "fixture request harmless command approval",
    DeliveryMode::StartOrSteer,
  )
  .await?;
  let event = next_matching(&mut events, |event| matches!(&event.kind, RuntimeEventKind::InteractionRequested(interaction) if interaction.kind == InteractionKind::Permission)).await?;
  let RuntimeEventKind::InteractionRequested(permission) = event.kind else {
    unreachable!()
  };
  assert!(matches!(
    adapter
      .execute(request(
        &target,
        "allow-command",
        Command::Respond {
          interaction_id: permission.interaction_id,
          response: InteractionResponse::Permission {
            decision: PermissionDecision::AllowOnce
          }
        }
      ))
      .await?,
    Reply::Acknowledged
  ));
  completed(&mut events, "fixture permission complete").await?;

  admission(
    &adapter,
    &target,
    "running",
    "fixture run until released",
    DeliveryMode::StartOrSteer,
  )
  .await?;
  working(&mut events).await?;
  admission(
    &adapter,
    &target,
    "steer",
    "fixture explicit steer",
    DeliveryMode::Steer,
  )
  .await?;
  admission(
    &adapter,
    &target,
    "start-or-steer",
    "fixture atomic steer",
    DeliveryMode::StartOrSteer,
  )
  .await?;
  if let Some(gate) = gate {
    gate.release();
  }
  completed(&mut events, "fixture steer complete").await?;

  admission(
    &adapter,
    &target,
    "cancelled-message",
    "fixture cancel this",
    DeliveryMode::StartOrSteer,
  )
  .await?;
  working(&mut events).await?;
  assert!(matches!(
    adapter.execute(request(&target, "cancel", Command::CancelTurn)).await?,
    Reply::Acknowledged
  ));
  next_matching(&mut events, |event| {
    matches!(&event.kind, RuntimeEventKind::Native(value)
    if value["method"] == "turn/completed" && value["params"]["turn"]["status"] == "interrupted")
  })
  .await?;
  let Reply::State(state) = adapter.execute(request(&target, "settled", Command::State)).await? else {
    panic!("missing final Codex state")
  };
  assert_eq!(state.status, ExecutionState::Idle);
  let resumed = adapter.resume_thread(target.session_id.as_deref().unwrap()).await?;
  assert_eq!(resumed, target);
  adapter.close().await?;
  Ok(())
}

#[tokio::test]
async fn replay_real_codex_rpc_conversation() -> TestResult {
  let fixture: Fixture = serde_json::from_str(&std::fs::read_to_string(fixture_path())?)?;
  assert_eq!(fixture.schema_version, 1);
  assert_eq!(fixture.provider, "codex");
  assert_eq!(fixture.agent_version, VERSION);
  assert_eq!(fixture.inspected_protocol_revision, PROTOCOL_REVISION);
  assert_eq!(fixture.executable_sha256.len(), 64);
  let (client, server) = tokio::io::duplex(1024 * 1024);
  let (reader, writer) = tokio::io::split(client);
  let (server_reader, mut server_writer) = tokio::io::split(server);
  let replay = tokio::spawn(async move {
    let mut lines = BufReader::new(server_reader).lines();
    for (index, frame) in fixture.frames.into_iter().enumerate() {
      match frame.direction.as_str() {
        "request" => {
          let actual: Value =
            serde_json::from_str(&lines.next_line().await?.ok_or("adapter closed before capture ended")?)?;
          assert_eq!(actual, frame.body, "captured Codex request {index} changed");
        }
        "event" => {
          server_writer
            .write_all(serde_json::to_string(&frame.body)?.as_bytes())
            .await?;
          server_writer.write_all(b"\n").await?;
        }
        other => return Err(format!("invalid Codex capture direction {other}").into()),
      }
    }
    assert!(lines.next_line().await?.is_none());
    Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
  });
  exercise(
    CodexAdapter::attach(options(), reader, writer).await?,
    Path::new("/fixture/home/workspace"),
    None,
  )
  .await?;
  replay.await??;
  Ok(())
}

#[tokio::test]
#[ignore = "requires TOKN_CODEX_RPC_EXECUTABLE at the documented native version; local model server only"]
async fn capture_real_codex_rpc_conversation() -> TestResult {
  let executable = PathBuf::from(std::env::var("TOKN_CODEX_RPC_EXECUTABLE")?);
  if !executable.is_absolute() {
    return Err("TOKN_CODEX_RPC_EXECUTABLE must be absolute".into());
  }
  let home = tempfile::tempdir()?;
  let codex_home = home.path().join("codex");
  let work = home.path().join("workspace");
  std::fs::create_dir(&codex_home)?;
  std::fs::create_dir(&work)?;
  let version = tokio::process::Command::new(&executable)
    .arg("--version")
    .env_clear()
    .env("HOME", home.path())
    .env("CODEX_HOME", &codex_home)
    .env("PATH", "/usr/bin:/bin")
    .current_dir(&work)
    .output()
    .await?;
  assert!(version.status.success(), "Codex version probe failed");
  assert_eq!(String::from_utf8(version.stdout)?.trim(), VERSION);
  let executable_sha256 = format!("{:x}", Sha256::digest(std::fs::read(&executable)?));
  let (busy_step, gate) = ModelStep::gated_text("fixture busy complete");
  let server = ModelServer::start(vec![
    ModelStep::text("fixture hello complete"),
    ModelStep::tool_call("request_user_input", json!({"questions":[{"id":"fixture_choice","header":"Fixture","question":"Choose a fixture option","options":[{"label":"Alpha","description":"First option"},{"label":"Beta","description":"Second option"}]},{"id":"fixture_notes","header":"Notes","question":"Any fixture notes?","options":[{"label":"No notes","description":"Continue without notes"},{"label":"Revisit","description":"Revisit the plan"}]}]})),
    ModelStep::text("fixture question complete"),
    ModelStep::tool_call("exec_command", json!({"cmd":"printf fixture-approved","yield_time_ms":1000,"sandbox_permissions":"require_escalated","justification":"Approve the harmless fixture printf command."})),
    ModelStep::text("fixture permission complete"),
    busy_step,
    ModelStep::text("fixture steer complete"),
    ModelStep::stall(),
  ]).await?;
  std::fs::write(
    codex_home.join("config.toml"),
    format!(
      r#"model="fixture-model"
model_provider="fixture"
approval_policy="on-request"
approvals_reviewer="user"
sandbox_mode="workspace-write"
[features]
default_mode_request_user_input=true
[analytics]
enabled=false
[model_providers.fixture]
name="Local fixture"
base_url={base_url:?}
wire_api="responses"
requires_openai_auth=false
request_max_retries=0
stream_max_retries=0
supports_websockets=false
"#,
      base_url = server.url()
    ),
  )?;
  let mut child = tokio::process::Command::new(&executable)
    .arg("app-server")
    .current_dir(&work)
    .env_clear()
    .env("HOME", home.path())
    .env("CODEX_HOME", &codex_home)
    .env("PATH", "/usr/bin:/bin")
    .env("TMPDIR", home.path())
    .env("NO_PROXY", "127.0.0.1,localhost")
    .env("RUST_LOG", "error")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .spawn()?;
  let mut stderr = BufReader::new(child.stderr.take().unwrap()).lines();
  let mut stderr_task = tokio::spawn(async move {
    let mut output = String::new();
    while let Ok(Some(line)) = stderr.next_line().await {
      output.push_str(&line);
      output.push('\n');
    }
    output
  });
  let frames: Frames = Arc::default();
  let reader = Capture::new(child.stdout.take().unwrap(), frames.clone(), "event");
  let writer = Capture::new(child.stdin.take().unwrap(), frames.clone(), "request");
  let result: TestResult = match tokio::time::timeout(Duration::from_secs(90), async {
    let adapter = CodexAdapter::attach(options(), reader, writer).await?;
    exercise(adapter, &work, Some(gate)).await
  })
  .await
  {
    Ok(result) => result,
    Err(error) => Err(error.into()),
  };
  let _ = tokio::time::timeout(Duration::from_secs(5), child.kill()).await;
  let stderr = match tokio::time::timeout(Duration::from_secs(5), &mut stderr_task).await {
    Ok(result) => result?,
    Err(_) => {
      stderr_task.abort();
      "native stderr did not close after process termination".into()
    }
  };
  if let Err(error) = result {
    let captured = frames.lock().unwrap();
    let tail = &captured[captured.len().saturating_sub(8)..];
    return Err(
      format!(
        "{error}; Codex stderr: {stderr}; model requests: {}; native tail: {}",
        server.requests().len(),
        serde_json::to_string(tail)?
      )
      .into(),
    );
  }
  let requests = server.requests();
  assert_eq!(requests.len(), 8);
  assert!(requests[2]["input"].to_string().contains("Beta"));
  assert!(
    requests[2]["input"]
      .to_string()
      .contains("user_note: fixture option note")
  );
  assert!(requests[2]["input"].to_string().contains("fixture custom answer"));
  assert!(requests[4]["input"].to_string().contains("fixture-approved"));
  assert!(requests[6]["input"].to_string().contains("fixture explicit steer"));
  assert!(requests[6]["input"].to_string().contains("fixture atomic steer"));
  let mut fixture = Fixture {
    schema_version: 1, provider: "codex".into(), agent_version: VERSION.into(), executable_sha256,
    inspected_protocol_revision: PROTOCOL_REVISION.into(),
    scenario: "metadata/state/models, duplicate admission, start-or-steer, native choice/free-text answers, command approval, explicit and atomic steering, cancellation, explicit resume; strict start/queue reject".into(),
    frames: frames.lock().unwrap().clone(),
  };
  let mut encoded = serde_json::to_string(&fixture)?;
  encoded = encoded
    .replace(
      &std::fs::canonicalize(home.path())?.to_string_lossy().to_string(),
      "/fixture/home",
    )
    .replace(&home.path().to_string_lossy().to_string(), "/fixture/home")
    .replace(&server.url(), "http://127.0.0.1:1/v1");
  fixture = serde_json::from_str(&encoded)?;
  for frame in &mut fixture.frames {
    if frame.body["method"] == "remoteControl/status/changed" {
      frame.body["params"]["serverName"] = json!("fixture-host");
    }
  }
  if std::env::var("TOKN_CAPTURE_RPC_FIXTURES").as_deref() == Ok("1") {
    std::fs::create_dir_all(fixture_path().parent().unwrap())?;
    std::fs::write(fixture_path(), format!("{}\n", serde_json::to_string_pretty(&fixture)?))?;
  }
  eprintln!(
    "Codex native scenario passed: {} local model requests, {} native frames",
    requests.len(),
    fixture.frames.len()
  );
  Ok(())
}
