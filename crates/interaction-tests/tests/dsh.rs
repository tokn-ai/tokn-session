//! Real DSH ApiProxy capture and deterministic replay. Native runs are opt-in.
use serde_json::{Value, json};
use std::{
  path::{Path, PathBuf},
  process::Stdio,
  time::Duration,
};
use tokio::{
  io::{AsyncBufReadExt, BufReader},
  process::{Child, Command as ProcessCommand},
  sync::broadcast,
};
use tokn_dsh_adapter::{ApiProxyConfig, DshAdapter};
use tokn_interaction_core::*;
use tokn_interaction_tests::{Fixture, ModelGate, ModelServer, ModelStep, RecordingProxy, TestResult, Trace};

const REVISION: &str = "99f6f02fecdb7dff40c3fbc9470f5907c29f74ca";
const MODEL: &str = "rpc-test-model";
const PROVIDER: &str = "tokn-test";
const FIXTURE: &str = "fixtures/dsh/interaction.json";

struct NativeProcess {
  child: Child,
  _directory: tempfile::TempDir,
  endpoint: String,
  directory: PathBuf,
  install_root: PathBuf,
}
impl NativeProcess {
  async fn start(model_url: &str) -> TestResult<Self> {
    let root = PathBuf::from(
      std::env::var("TOKN_DSH_RPC_ROOT")
        .map_err(|_| "Set TOKN_DSH_RPC_ROOT to a prepared pinned DSH tree (scripts/rpc/prepare-dsh.sh)")?,
    );
    let node = PathBuf::from(
      std::env::var("TOKN_RPC_NODE").map_err(|_| "Set TOKN_RPC_NODE to an absolute Node >=24 executable")?,
    );
    if !root.is_absolute() || !node.is_absolute() {
      return Err("Native DSH and Node paths must be absolute".into());
    }
    if std::fs::read_to_string(root.join(".tokn-rpc-source-revision"))?.trim() != REVISION {
      return Err("DSH source revision does not match the fixture pin".into());
    }
    let directory = tempfile::tempdir()?;
    let canonical_directory = directory.path().canonicalize()?;
    let home = canonical_directory.join("home");
    let project = canonical_directory.join("project");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&project)?;
    let patch = directory.path().join("local.patch.yml");
    std::fs::write(
      &patch,
      format!(
        r#"- id: session-title-llm
  disabled: true
- id: llm-deepseek
  disabled: true
- id: agent-default-model
  config:
    provider: {PROVIDER}
    model: {MODEL}
- id: llm-pi-ai
  config:
    providers:
      {PROVIDER}:
        api: openai-completions
        apiKeyEnv: TOKN_TEST_API_KEY
        baseURL: {model_url}
        models:
          - id: {MODEL}
            name: RPC test model
            reasoning: false
            input: [text]
            contextWindow: 32768
            maxTokens: 4096
"#
      ),
    )?;
    let stderr_path = directory.path().join("stderr.log");
    let stderr = std::fs::File::create(&stderr_path)?;
    let mut command = ProcessCommand::new(&node);
    command
      .env_clear()
      .env(
        "PATH",
        format!("{}:/usr/bin:/bin", node.parent().ok_or("node parent")?.display()),
      )
      .env("HOME", &home)
      .env("DSH_HOME", &home)
      .env("DSH_TELEMETRY_DISABLED", "1")
      .env("TOKN_TEST_API_KEY", "tokn-local-test-key")
      .arg(root.join("apps/cli/lib/bin.js"))
      .arg("--patch")
      .arg(&patch)
      .args(["--profile", "web", "--host", "127.0.0.1", "--port", "0"])
      .current_dir(&project)
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(stderr)
      .kill_on_drop(true);
    let mut child = command.spawn()?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or("No native stdout")?).lines();
    let endpoint = tokio::time::timeout(Duration::from_secs(45), async {
      while let Some(line) = stdout.next_line().await? {
        if let Some(url) = line
          .strip_prefix("dsh web: ")
          .and_then(|line| line.split_whitespace().next())
        {
          if url.starts_with("http://127.0.0.1:") {
            return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(url.to_owned());
          }
        }
      }
      Err(
        format!(
          "DSH exited before readiness: {}",
          std::fs::read_to_string(&stderr_path).unwrap_or_default()
        )
        .into(),
      )
    })
    .await??;
    tokio::spawn(async move { while matches!(stdout.next_line().await, Ok(Some(_))) {} });
    Ok(Self {
      child,
      directory: canonical_directory,
      _directory: directory,
      endpoint,
      install_root: root,
    })
  }
  async fn close(&mut self) -> TestResult {
    self.child.kill().await?;
    let _ = self.child.wait().await?;
    Ok(())
  }
}

struct Driver<'a> {
  adapter: &'a DshAdapter,
  endpoint: &'a str,
  count: u32,
}
impl Driver<'_> {
  async fn execute(&mut self, session: Option<&str>, command: Command) -> TestResult<Reply> {
    self.count += 1;
    let descriptor = self.adapter.descriptor();
    Ok(
      self
        .adapter
        .execute(InteractionRequest {
          request_id: format!("dsh-fixture-{}", self.count),
          target: SessionTarget {
            runtime_id: descriptor.runtime_id,
            instance_id: descriptor.instance_id,
            session_id: session.map(str::to_owned),
          },
          command,
        })
        .await?,
    )
  }
  async fn create(&self, session: &str) -> TestResult {
    let result: Value = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(45)).build()?.post(format!("{}/api/session.create", self.endpoint.trim_end_matches('/')))
      .json(&json!({"type":"client-request","rpcId":format!("create-{session}"),"method":"session.create","payload":{"sessionId":session,"agentPreset":"standard"}}))
      .send().await?.error_for_status()?.json().await?;
    assert_eq!(result["result"]["ok"], true, "native create: {result}");
    assert_eq!(result["result"]["value"]["sessionId"], session);
    Ok(())
  }
  async fn send(&mut self, session: &str, text: &str, delivery: DeliveryMode) -> TestResult {
    assert!(matches!(
      self
        .execute(
          Some(session),
          Command::Message {
            input: MessageInput {
              text: text.into(),
              ..Default::default()
            },
            delivery
          }
        )
        .await?,
      Reply::Admission(_)
    ));
    Ok(())
  }
}

async fn event_matching(
  events: &mut broadcast::Receiver<RuntimeEvent>,
  predicate: impl Fn(&RuntimeEvent) -> bool,
) -> TestResult<RuntimeEvent> {
  tokio::time::timeout(Duration::from_secs(30), async {
    loop {
      let event = events.recv().await?;
      if let RuntimeEventKind::Disconnected { reason } = &event.kind {
        return Err(format!("native downlink lost: {reason}").into());
      }
      if predicate(&event) {
        return Ok(event);
      }
    }
  })
  .await?
}
async fn assistant_message(events: &mut broadcast::Receiver<RuntimeEvent>, session: &str, text: &str) -> TestResult {
  event_matching(events, |event| event.session_id.as_deref() == Some(session) && matches!(&event.kind, RuntimeEventKind::Native(frame) if frame["method"] == "session/event" && frame["payload"]["event"]["type"] == "assistant/message" && frame["payload"]["event"]["data"].to_string().contains(text))).await?;
  Ok(())
}
async fn turn_ended(events: &mut broadcast::Receiver<RuntimeEvent>, session: &str) -> TestResult {
  event_matching(events, |event| event.session_id.as_deref() == Some(session) && matches!(&event.kind, RuntimeEventKind::Native(frame) if frame["method"] == "session/event" && frame["payload"]["event"]["type"] == "turn/end")).await?;
  Ok(())
}

async fn scenario(adapter: &DshAdapter, endpoint: &str, model: Option<(&ModelServer, &ModelGate)>) -> TestResult {
  let mut driver = Driver {
    adapter,
    endpoint,
    count: 0,
  };
  let mut events = adapter.subscribe();
  let Reply::Metadata(metadata) = driver.execute(None, Command::Metadata).await? else {
    panic!("metadata reply")
  };
  assert_eq!(metadata.name.as_deref(), Some("DeepSeek Harness ApiProxy"));
  let Reply::Models(models) = driver.execute(None, Command::Models).await? else {
    panic!("models reply")
  };
  assert!(
    models
      .iter()
      .any(|item| item.model.model_provider.as_deref() == Some(PROVIDER) && item.model.model_id == MODEL)
  );
  let session = "tokn-fixture-message";
  driver.create(session).await?;
  let Reply::State(state) = driver.execute(Some(session), Command::State).await? else {
    panic!("state reply")
  };
  assert_eq!(state.status, ExecutionState::Idle);
  driver
    .send(
      session,
      "Reply with the deterministic greeting.",
      DeliveryMode::StartOrSteer,
    )
    .await?;
  assistant_message(&mut events, session, "Hello from the local DSH model.").await?;
  turn_ended(&mut events, session).await?;

  let session = "tokn-fixture-question";
  driver.create(session).await?;
  driver
    .send(
      session,
      "Ask the deterministic choice question.",
      DeliveryMode::StartOrSteer,
    )
    .await?;
  let requested = event_matching(&mut events, |event| {
    event.session_id.as_deref() == Some(session) && matches!(&event.kind, RuntimeEventKind::InteractionRequested(_))
  })
  .await?;
  let RuntimeEventKind::InteractionRequested(question) = requested.kind else {
    unreachable!()
  };
  assert_eq!(question.questions[0].question_id, "choice");
  assert_eq!(question.questions[0].options[0].label, "Blue");
  assert!(question.questions[0].multiple);
  let Reply::Interactions(pending) = driver.execute(Some(session), Command::PendingInteractions).await? else {
    panic!("pending reply")
  };
  assert_eq!(pending.len(), 1);
  assert!(matches!(
    driver
      .execute(
        Some(session),
        Command::Respond {
          interaction_id: question.interaction_id,
          response: InteractionResponse::Answers {
            answers: vec![QuestionAnswer {
              question_id: "choice".into(),
              answers: vec!["Blue".into()],
              custom: Some("Keep it concise".into())
            }]
          }
        }
      )
      .await?,
    Reply::Acknowledged
  ));
  assistant_message(&mut events, session, "Selected Blue with the supplied note.").await?;
  turn_ended(&mut events, session).await?;

  let session = "tokn-fixture-steer";
  driver.create(session).await?;
  driver
    .send(session, "Start the gated task.", DeliveryMode::StartOrSteer)
    .await?;
  event_matching(&mut events, |event| {
    event.session_id.as_deref() == Some(session)
      && matches!(&event.kind, RuntimeEventKind::State(state) if state.status == ExecutionState::Running)
  })
  .await?;
  if let Some((model, _)) = model {
    model.wait_for_requests(4).await?;
  }
  driver
    .send(session, "Steer the current task.", DeliveryMode::StartOrSteer)
    .await?;
  driver
    .send(session, "Run the queued follow-up.", DeliveryMode::Queue)
    .await?;
  if let Some((_, gate)) = model {
    gate.release();
  }
  assistant_message(&mut events, session, "The steering instruction was consumed.").await?;
  turn_ended(&mut events, session).await?;
  assistant_message(&mut events, session, "The queued follow-up completed.").await?;
  turn_ended(&mut events, session).await?;

  let session = "tokn-fixture-cancel";
  driver.create(session).await?;
  driver
    .send(session, "Start the cancellable task.", DeliveryMode::StartOrSteer)
    .await?;
  event_matching(&mut events, |event| {
    event.session_id.as_deref() == Some(session)
      && matches!(&event.kind, RuntimeEventKind::State(state) if state.status == ExecutionState::Running)
  })
  .await?;
  if let Some((model, _)) = model {
    model.wait_for_requests(7).await?;
  }
  assert!(matches!(
    driver.execute(Some(session), Command::CancelTurn).await?,
    Reply::Acknowledged
  ));
  turn_ended(&mut events, session).await?;
  let Reply::State(state) = driver.execute(Some(session), Command::State).await? else {
    panic!("final state reply")
  };
  assert_eq!(state.status, ExecutionState::Idle);
  Ok(())
}

#[tokio::test]
#[ignore = "launches pinned DSH; run scripts/rpc/prepare-dsh.sh and set TOKN_DSH_RPC_ROOT/TOKN_RPC_NODE"]
async fn capture_real_dsh_rpc() -> TestResult {
  let (gated, gate) = ModelStep::gated_text("Gated task reached its boundary.");
  let model = ModelServer::start(vec![
    ModelStep::text("Hello from the local DSH model."),
    ModelStep::tool_call("ask_user_question", json!({"questions":[{"id":"choice","question":"Choose a color","multi_select":true,"options":[{"label":"Blue","description":"The deterministic choice"},{"label":"Red"}]}]})),
    ModelStep::text("Selected Blue with the supplied note."),
    gated,
    ModelStep::text("The steering instruction was consumed."),
    ModelStep::text("The queued follow-up completed."),
    ModelStep::stall(),
  ]).await?;
  let mut native = NativeProcess::start(&model.url()).await?;
  let trace = Trace::default();
  let proxy = RecordingProxy::start(&native.endpoint, trace.clone()).await?;
  let adapter = DshAdapter::attach(ApiProxyConfig::new(proxy.url())).await?;
  let result = scenario(&adapter, &proxy.url(), Some((&model, &gate))).await;
  let records = trace.records();
  eprintln!(
    "DSH local model requests: {}; native records: {}",
    model.requests().len(),
    records.len()
  );
  if result.is_err() {
    eprintln!(
      "DSH native diagnostics: {}",
      std::fs::read_to_string(native.directory.join("stderr.log")).unwrap_or_default()
    );
  }
  adapter.close().await?;
  proxy.close().await;
  native.close().await?;
  result?;
  assert_eq!(model.requests().len(), 7);
  let requests = model.requests();
  assert!(requests[4].to_string().contains("Steer the current task."));
  assert!(requests[5].to_string().contains("Run the queued follow-up."));
  assert!(requests[2].to_string().contains("Keep it concise"));
  let mut fixture = trace.fixture("dsh", REVISION, "metadata-models-message-question-steer-queue-cancel");
  fixture.sanitize(&[
    (native.directory.to_str().ok_or("invalid temp path")?, "/fixture/dsh"),
    (
      native._directory.path().to_str().ok_or("invalid temp alias")?,
      "/fixture/dsh",
    ),
    (&native.endpoint, "http://127.0.0.1:19001"),
    (&proxy.url(), "http://127.0.0.1:19002"),
    (&model.url(), "http://127.0.0.1:19003/v1"),
    (
      native.install_root.to_str().ok_or("invalid install path")?,
      "/fixture/dsh-install",
    ),
  ]);
  if std::env::var("TOKN_CAPTURE_RPC_FIXTURES").as_deref() == Ok("1") {
    fixture.save(Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE))?;
  }
  Ok(())
}

#[tokio::test]
async fn replay_captured_dsh_rpc() -> TestResult {
  let fixture = Fixture::load(Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE))?;
  assert_eq!(fixture.provider, "dsh");
  assert_eq!(fixture.upstream_revision, REVISION);
  let server = tokn_interaction_tests::ReplayServer::start(fixture).await?;
  let adapter = DshAdapter::attach(ApiProxyConfig::new(server.url())).await?;
  scenario(&adapter, &server.url(), None).await?;
  adapter.close().await?;
  server.assert_consumed()?;
  Ok(())
}
