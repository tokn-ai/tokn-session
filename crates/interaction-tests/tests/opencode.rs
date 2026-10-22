//! Real native OpenCode V2 process, deterministic loopback model, and captured replay.
use serde_json::json;
use std::{
  collections::BTreeMap,
  path::{Path, PathBuf},
  process::Stdio,
  time::Duration,
};
use tokio::{
  io::{AsyncBufReadExt, BufReader},
  process::{Child, Command as ProcessCommand},
};
use tokn_interaction_core::*;
use tokn_interaction_tests::{
  Fixture, ModelGate, ModelServer, ModelStep, RecordingProxy, ReplayServer, TestResult, Trace,
};
use tokn_opencode_adapter::{ConnectionOptions, OpenCodeAdapter};

const REVISION: &str = "017a5977d2107092007623e507fc5c6eb337d3b2";
const MODEL: &str = "rpc-test-model";

struct NativeProcess {
  child: Child,
  root: PathBuf,
  endpoint: String,
}
impl NativeProcess {
  async fn start(model_url: &str) -> TestResult<Self> {
    let source_root = PathBuf::from(
      std::env::var_os("TOKN_OPENCODE_RPC_ROOT")
        .ok_or("Set TOKN_OPENCODE_RPC_ROOT using scripts/rpc/prepare-opencode.sh")?,
    );
    if std::fs::read_to_string(source_root.join(".tokn-rpc-source-revision"))?.trim() != REVISION {
      return Err("OpenCode source revision does not match the fixture provenance".into());
    }
    let executable = source_root.join(".tokn-rpc-opencode");
    let root = std::env::temp_dir().join(format!("tokn-rpc-opencode-{}", uuid::Uuid::new_v4()));
    let project = root.join("project");
    std::fs::create_dir_all(&project)?;
    // V2 migrates this supported legacy config. The adapter's conservative V1
    // history guard also invokes the legacy config parser, which currently
    // rejects a V2-only `providers`/`permissions` config before reading history.
    let config = json!({
      "model":format!("local/{MODEL}"),"autoupdate":false,"share":"disabled","snapshot":false,
      "provider":{"local":{"name":"Local RPC fixture model","npm":"@ai-sdk/openai-compatible","options":{"baseURL":model_url,"apiKey":"local-test-key"},"models":{MODEL:{"name":"RPC fixture model","tool_call":true,"modalities":{"input":["text"],"output":["text"]},"limit":{"context":32768,"output":1024}}}}},
      "permission":{"*":"deny","question":"allow","bash":"ask"}
    });
    std::fs::write(project.join("opencode.json"), serde_json::to_vec_pretty(&config)?)?;
    let catalog = root.join("models.json");
    std::fs::write(&catalog, b"{}")?;
    let mut environment = BTreeMap::new();
    for (name, relative) in [
      ("HOME", "home"),
      ("XDG_CONFIG_HOME", "config"),
      ("XDG_DATA_HOME", "data"),
      ("XDG_CACHE_HOME", "cache"),
      ("XDG_STATE_HOME", "state"),
    ] {
      let path = root.join(relative);
      std::fs::create_dir_all(&path)?;
      environment.insert(name.to_owned(), path.to_string_lossy().into_owned());
    }
    for flag in [
      "OPENCODE_DISABLE_MODELS_FETCH",
      "OPENCODE_DISABLE_DEFAULT_PLUGINS",
      "OPENCODE_DISABLE_AUTOUPDATE",
      "OPENCODE_DISABLE_EMBEDDED_WEB_UI",
      "OPENCODE_DISABLE_EXTERNAL_SKILLS",
      "OPENCODE_DISABLE_LSP_DOWNLOAD",
      "OPENCODE_EXPERIMENTAL_DISABLE_FILEWATCHER",
      "OPENCODE_DISABLE_FFF",
    ] {
      environment.insert(flag.into(), "1".into());
    }
    environment.insert("OPENCODE_MODELS_PATH".into(), catalog.to_string_lossy().into_owned());
    environment.insert(
      "OPENCODE_TEST_HOME".into(),
      root.join("home").to_string_lossy().into_owned(),
    );
    // This process receives neither real provider credentials nor user config.
    environment.insert("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into());
    let mut child = ProcessCommand::new(executable)
      .args(["serve", "--hostname", "127.0.0.1", "--port", "0"])
      .current_dir(&project)
      .env_clear()
      .envs(environment)
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .kill_on_drop(true)
      .spawn()?;
    let stderr = child.stderr.take().unwrap();
    let log = root.join("stderr.log");
    tokio::spawn(async move {
      let mut reader = BufReader::new(stderr);
      let mut file = tokio::fs::File::create(log).await.unwrap();
      let _ = tokio::io::copy(&mut reader, &mut file).await;
    });
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let readiness = tokio::time::timeout(Duration::from_secs(45), async {
      while let Some(line) = lines.next_line().await? {
        if let Some(url) = line.strip_prefix("opencode server listening on ") {
          return Ok::<_, std::io::Error>(url.to_owned());
        }
      }
      Err(std::io::Error::other(
        "OpenCode exited before readiness; inspect its isolated stderr.log",
      ))
    })
    .await;
    let endpoint_result: TestResult<String> = match readiness {
      Ok(result) => result.map_err(Into::into),
      Err(error) => Err(error.into()),
    };
    let endpoint = match endpoint_result {
      Ok(endpoint) => endpoint,
      Err(error) => {
        let _ = child.kill().await;
        return Err(error);
      }
    };
    tokio::spawn(async move { while matches!(lines.next_line().await, Ok(Some(_))) {} });
    Ok(Self { child, root, endpoint })
  }
  fn directory(&self) -> String {
    self.root.join("project").to_string_lossy().into_owned()
  }
  async fn stop(&mut self) -> TestResult {
    if self.child.try_wait()?.is_none() {
      self.child.kill().await?;
    }
    Ok(())
  }
  async fn close(mut self) -> TestResult {
    self.stop().await?;
    std::fs::remove_dir_all(&self.root)?;
    Ok(())
  }
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
      ..Default::default()
    },
    delivery,
  }
}
async fn execute(adapter: &OpenCodeAdapter, target: &SessionTarget, id: &str, command: Command) -> TestResult<Reply> {
  adapter
    .execute(request(target, id, command))
    .await
    .map_err(|error| format!("OpenCode operation {id}: {error}").into())
}
async fn wait_idle(adapter: &OpenCodeAdapter, target: &SessionTarget) -> TestResult {
  tokio::time::timeout(Duration::from_secs(25), async {
    loop {
      if matches!(
        execute(adapter, target, "state", Command::State).await?,
        Reply::State(SessionState {
          status: ExecutionState::Idle,
          ..
        })
      ) {
        return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
      }
      tokio::time::sleep(Duration::from_millis(40)).await;
    }
  })
  .await??;
  Ok(())
}
async fn wait_pending(
  adapter: &OpenCodeAdapter,
  target: &SessionTarget,
  kind: InteractionKind,
) -> TestResult<PendingInteraction> {
  tokio::time::timeout(Duration::from_secs(25), async {
    loop {
      if let Reply::Interactions(items) = execute(adapter, target, "pending", Command::PendingInteractions).await? {
        if let Some(item) = items.into_iter().find(|i| i.kind == kind) {
          return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(item);
        }
      }
      tokio::time::sleep(Duration::from_millis(40)).await;
    }
  })
  .await?
}

/// Requires only the pinned native executable and local dependencies, never a provider key.
#[tokio::test]
#[ignore = "requires pinned OpenCode executable; run scripts/test-real-rpc.sh opencode"]
async fn real_opencode_rpc_local_model_capture() -> TestResult {
  let (hold, gate) = ModelStep::gated_text("first stream finished");
  let model=ModelServer::start(vec![
    ModelStep::text("Hello from the local fixture model."),
    ModelStep::tool_call("question",json!({"questions":[{"question":"Choose a local test option","header":"Fixture","options":[{"label":"Alpha","description":"First option"},{"label":"Beta","description":"Second option"}]}]})),
    ModelStep::text("The selected answer was received."),
    ModelStep::tool_call("bash",json!({"command":"printf rpc-local-only"})),
    ModelStep::text("The permission response was received."),
    hold,ModelStep::text("Steered and queued messages were received."),ModelStep::text("Queued follow-up completed."),
    ModelStep::stall(),
  ]).await?;
  let mut native = NativeProcess::start(&model.url()).await?;
  let trace = Trace::default();
  let proxy = RecordingProxy::start(&native.endpoint, trace.clone()).await?;
  let adapter = OpenCodeAdapter::connect(ConnectionOptions {
    runtime_id: "real-opencode".into(),
    endpoint: proxy.url(),
    directory: native.directory(),
    workspace_id: None,
    credentials: None,
  })
  .await?;
  if let Err(error) = exercise(&adapter, Some(&model), Some(&gate)).await {
    let saved = trace
      .fixture("opencode", REVISION, "failed_real_run_debug_only")
      .save(std::env::temp_dir().join("tokn-opencode-failed.json"));
    let _ = adapter.close().await;
    proxy.close().await;
    let _ = native.stop().await;
    saved?;
    return Err(error);
  }
  adapter.close().await?;
  proxy.close().await;
  let mut fixture = trace.fixture(
    "opencode",
    REVISION,
    "native_v2_local_model_messages_questions_permissions_steer_queue_cancel",
  );
  let native_root = native.root.to_string_lossy().into_owned();
  let canonical_root = std::fs::canonicalize(&native.root)?.to_string_lossy().into_owned();
  fixture.sanitize(&[
    (&canonical_root, "/tmp/tokn-rpc-opencode"),
    (&native_root, "/tmp/tokn-rpc-opencode"),
    (native_root.trim_start_matches('/'), "tmp/tokn-rpc-opencode"),
    (&model.url(), "http://127.0.0.1:12345/v1"),
    (&std::env::temp_dir().to_string_lossy(), "/tmp"),
  ]);
  if std::env::var("TOKN_CAPTURE_RPC_FIXTURES").as_deref() == Ok("1") {
    fixture.save(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/opencode/native-v2.json"))?;
  }
  native.close().await?;
  Ok(())
}

async fn wait_model(model: Option<&ModelServer>, count: usize) -> TestResult {
  if let Some(model) = model {
    model.wait_for_requests(count).await?;
  }
  Ok(())
}
async fn exercise(adapter: &OpenCodeAdapter, model: Option<&ModelServer>, gate: Option<&ModelGate>) -> TestResult {
  let mut events = adapter.subscribe();
  let target = adapter
    .create_session(
      Some("build".into()),
      Some(ModelIdentity {
        model_provider: Some("local".into()),
        model_id: MODEL.into(),
        display_name: None,
      }),
    )
    .await?;
  // Native /api/health precedes asynchronous location plugin/catalog boot.
  // Wait for the actual native metadata rather than sleeping a guessed delay.
  tokio::time::timeout(Duration::from_secs(20), async {
    loop {
      if matches!(execute(&adapter,&target,"metadata",Command::Metadata).await?,Reply::Metadata(value) if value.agents.iter().any(|agent|agent=="build")) { return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(()); }
      tokio::time::sleep(Duration::from_millis(40)).await;
    }
  }).await??;
  assert!(
    matches!(execute(&adapter,&target,"models",Command::Models).await?,Reply::Models(models) if models.iter().any(|choice|choice.model.model_id==MODEL))
  );
  execute(
    &adapter,
    &target,
    "text",
    message("Return the deterministic greeting.", DeliveryMode::StartOrSteer),
  )
  .await?;
  wait_model(model, 1).await?;
  wait_idle(&adapter, &target).await?;
  execute(
    &adapter,
    &target,
    "question",
    message("Ask the scripted question.", DeliveryMode::StartOrSteer),
  )
  .await?;
  let question = wait_pending(&adapter, &target, InteractionKind::Question).await?;
  tokio::time::timeout(Duration::from_secs(10), async {
    loop { if matches!(events.recv().await?, RuntimeEvent {kind: RuntimeEventKind::InteractionRequested(ref item), ..} if item.interaction_id == question.interaction_id) {return Ok::<_,tokio::sync::broadcast::error::RecvError>(());} }
  }).await??;
  assert_eq!(question.questions[0].options[0].label, "Alpha");
  execute(
    &adapter,
    &target,
    "answer",
    Command::Respond {
      interaction_id: question.interaction_id,
      response: InteractionResponse::Answers {
        answers: vec![QuestionAnswer {
          question_id: question.questions[0].question_id.clone(),
          answers: vec!["Beta".into()],
          custom: None,
        }],
      },
    },
  )
  .await?;
  wait_model(model, 3).await?;
  wait_idle(&adapter, &target).await?;
  execute(
    &adapter,
    &target,
    "permission",
    message(
      "Ask permission for the harmless shell command.",
      DeliveryMode::StartOrSteer,
    ),
  )
  .await?;
  let permission = wait_pending(&adapter, &target, InteractionKind::Permission).await?;
  tokio::time::timeout(Duration::from_secs(10), async {
    loop { if matches!(events.recv().await?, RuntimeEvent {kind: RuntimeEventKind::InteractionRequested(ref item), ..} if item.interaction_id == permission.interaction_id) {return Ok::<_,tokio::sync::broadcast::error::RecvError>(());} }
  }).await??;
  execute(
    &adapter,
    &target,
    "allow",
    Command::Respond {
      interaction_id: permission.interaction_id,
      response: InteractionResponse::Permission {
        decision: PermissionDecision::AllowOnce,
      },
    },
  )
  .await?;
  wait_model(model, 5).await?;
  wait_idle(&adapter, &target).await?;
  execute(
    &adapter,
    &target,
    "running",
    message("Hold a streaming response for steering.", DeliveryMode::StartOrSteer),
  )
  .await?;
  wait_model(model, 6).await?;
  assert!(matches!(
    execute(
      &adapter,
      &target,
      "steer",
      message("Steering input.", DeliveryMode::StartOrSteer)
    )
    .await?,
    Reply::Admission(_)
  ));
  assert!(matches!(
    execute(
      &adapter,
      &target,
      "queue",
      message("Queued input.", DeliveryMode::Queue)
    )
    .await?,
    Reply::Admission(InputReceipt {
      disposition: InputDisposition::Queued,
      ..
    })
  ));
  if let Some(gate) = gate {
    gate.release();
  }
  // Wait for both native follow-up requests rather than accepting a transient
  // idle snapshot between turns. Cancellation must reach the scripted stall.
  wait_model(model, 8).await?;
  wait_idle(&adapter, &target).await?;
  let before = model.map(|model| model.requests().len()).unwrap_or(0);
  execute(
    &adapter,
    &target,
    "cancel-start",
    message("Start a cancellable response.", DeliveryMode::StartOrSteer),
  )
  .await?;
  wait_model(model, before + 1).await?;
  assert!(matches!(
    execute(&adapter, &target, "cancel", Command::CancelTurn).await?,
    Reply::Acknowledged
  ));
  wait_idle(&adapter, &target).await?;
  if let Some(model) = model {
    let requests = model.requests();
    assert_eq!(
      requests.len(),
      9,
      "one text, two question, two permission, three busy/follow-up, one stalled model request"
    );
    assert!(requests[2].to_string().contains("Beta"));
    assert!(requests[6].to_string().contains("Steering input."));
    assert!(requests[7].to_string().contains("Queued input."));
    assert!(requests.iter().all(|request| request["model"] == MODEL));
  }
  Ok(())
}

#[tokio::test]
async fn replay_captured_native_opencode_v2() -> TestResult {
  let fixture = Fixture::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/opencode/native-v2.json"))?;
  assert_eq!(fixture.provider, "opencode");
  assert_eq!(fixture.upstream_revision, REVISION);
  let replay = ReplayServer::start(fixture).await?;
  let adapter = OpenCodeAdapter::connect(ConnectionOptions {
    runtime_id: "replay-opencode".into(),
    endpoint: replay.url(),
    directory: "/tmp/tokn-rpc-opencode/project".into(),
    workspace_id: None,
    credentials: None,
  })
  .await?;
  exercise(&adapter, None, None).await?;
  replay.assert_consumed()?;
  adapter.close().await?;
  replay.close().await;
  Ok(())
}
