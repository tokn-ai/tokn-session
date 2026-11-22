//! Opt-in published DSH Gateway verification. This is not a replay capture.
//! The inspected source and the separately published package have distinct provenance.
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use tokio::sync::broadcast;
use tokn_dsh_adapter::{CreateSessionOptions, DshAdapter, ManagedOptions, new_session_id};
use tokn_interaction_core::*;
use tokn_interaction_tests::{ModelGate, ModelServer, ModelStep, TestResult};

const INSPECTED_REVISION: &str = "2e39e70496aa02986ca6a820c5d497e816b69d7e";
const PACKAGE_VERSION: &str = "0.2.2-rc.5";
const PROVIDER: &str = "tokn-test";
const MODEL: &str = "rpc-test-model";

struct NativeProfile {
  directory: tempfile::TempDir,
  project: PathBuf,
  options: ManagedOptions,
}
impl NativeProfile {
  fn prepare(model_url: &str) -> TestResult<Self> {
    let package = PathBuf::from(
      std::env::var("TOKN_DSH_CURRENT_ROOT")
        .map_err(|_| "Set TOKN_DSH_CURRENT_ROOT to the extracted, dependency-installed @deepseek-ai/dsh package")?,
    );
    let node = PathBuf::from(
      std::env::var("TOKN_RPC_NODE").map_err(|_| "Set TOKN_RPC_NODE to an absolute Node >=24 executable")?,
    );
    if !package.is_absolute() || !node.is_absolute() {
      return Err("The native package and Node paths must be absolute".into());
    }
    let metadata: Value = serde_json::from_slice(&std::fs::read(package.join("package.json"))?)?;
    if metadata["name"] != "@deepseek-ai/dsh" || metadata["version"] != PACKAGE_VERSION {
      return Err("The installed DSH package does not match the tested package version".into());
    }
    let directory = tempfile::Builder::new().prefix("tokn-dsh-current-").tempdir()?;
    let root = directory.path().canonicalize()?;
    let home = root.join("home");
    let project = root.join("project");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&project)?;
    let patch = root.join("local.patch.yml");
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
- insert:
    - id: preset-tokn-test
      name: '@deepseek-ai/dsh-agent-preset'
      config:
        id: tokn-test
        order: 0
        plugins:
          - id: tool-ask-user
            name: '@deepseek-ai/dsh-tool-ask-user'
            config:
              mode: timed
              timeout: 10
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
    let mut options = ManagedOptions::new(&project);
    // env executes Node in place, so the adapter still owns the actual native
    // process. Its -i prevents inherited account credentials and configuration.
    options.program = "/usr/bin/env".into();
    options.launcher_args = vec![
      "-i".into(),
      format!(
        "PATH={}:/usr/bin:/bin",
        node.parent().ok_or("Node parent missing")?.display()
      ),
      format!("HOME={}", home.display()),
      format!("DSH_HOME={}", home.display()),
      "DSH_TELEMETRY_DISABLED=1".into(),
      "TOKN_TEST_API_KEY=tokn-local-test-key".into(),
      node.to_str().ok_or("Node path is not UTF-8")?.into(),
      package
        .join("lib/bin.js")
        .to_str()
        .ok_or("Package path is not UTF-8")?
        .into(),
      "--patch".into(),
      patch.to_str().ok_or("Patch path is not UTF-8")?.into(),
    ];
    options.startup_timeout = Duration::from_secs(45);
    options.request_timeout = Duration::from_secs(45);
    Ok(Self {
      directory,
      project,
      options,
    })
  }
}

async fn execute(adapter: &DshAdapter, session: Option<&str>, command: Command) -> TestResult<Reply> {
  let descriptor = adapter.descriptor();
  Ok(
    adapter
      .execute(InteractionRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
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

async fn create(adapter: &DshAdapter, project: &std::path::Path) -> TestResult<String> {
  let session_id = new_session_id();
  let created = adapter
    .create_session(CreateSessionOptions {
      session_id: session_id.clone(),
      working_directory: project.to_owned(),
      agent_preset: Some("tokn-test".into()),
    })
    .await?;
  assert_eq!(created.target.session_id.as_deref(), Some(session_id.as_str()));
  assert_eq!(created.working_directory, project);
  let Reply::State(state) = execute(adapter, Some(&session_id), Command::State).await? else {
    panic!("Expected state reply");
  };
  assert_eq!(state.status, ExecutionState::Idle);
  eprintln!("Created disposable native DSH session {session_id}");
  Ok(session_id)
}

async fn send(adapter: &DshAdapter, session: &str, text: &str, delivery: DeliveryMode) -> TestResult {
  let Reply::Admission(receipt) = execute(
    adapter,
    Some(session),
    Command::Message {
      input: MessageInput {
        text: text.into(),
        ..Default::default()
      },
      delivery,
    },
  )
  .await?
  else {
    panic!("Expected native input admission");
  };
  assert_eq!(receipt.disposition, InputDisposition::Accepted);
  assert!(receipt.native_id.is_some());
  Ok(())
}

async fn event_matching(
  events: &mut broadcast::Receiver<RuntimeEvent>,
  predicate: impl Fn(&RuntimeEvent) -> bool,
) -> TestResult<RuntimeEvent> {
  tokio::time::timeout(Duration::from_secs(30), async {
    loop {
      let event = events.recv().await?;
      if let RuntimeEventKind::Disconnected { reason } = &event.kind {
        return Err(format!("Native DSH event continuity lost: {reason}").into());
      }
      if predicate(&event) {
        return Ok(event);
      }
    }
  })
  .await?
}

fn session_event(event: &RuntimeEvent) -> Option<&Value> {
  match &event.kind {
    RuntimeEventKind::Native(frame) if frame["type"] == "item" && frame["value"]["type"] == "event" => {
      frame["value"].get("event")
    }
    _ => None,
  }
}

async fn completed(events: &mut broadcast::Receiver<RuntimeEvent>, session: &str, expected: &str) -> TestResult {
  event_matching(events, |event| {
    event.session_id.as_deref() == Some(session)
      && session_event(event)
        .is_some_and(|event| event["type"] == "assistant/message" && event["data"].to_string().contains(expected))
  })
  .await?;
  event_matching(events, |event| {
    event.session_id.as_deref() == Some(session)
      && session_event(event).is_some_and(|event| event["type"] == "turn/end")
  })
  .await?;
  Ok(())
}

async fn scenario(
  adapter: &DshAdapter,
  project: &std::path::Path,
  model: &ModelServer,
  gate: &ModelGate,
) -> TestResult {
  let mut events = adapter.subscribe();
  let Reply::Metadata(metadata) = execute(adapter, None, Command::Metadata).await? else {
    panic!("Expected metadata");
  };
  assert_eq!(metadata.name.as_deref(), Some("DeepSeek Harness Gateway"));
  let Reply::Models(models) = execute(adapter, None, Command::Models).await? else {
    panic!("Expected model catalog");
  };
  assert!(
    models
      .iter()
      .any(|choice| choice.model.model_provider.as_deref() == Some(PROVIDER) && choice.model.model_id == MODEL)
  );

  let session = create(adapter, project).await?;
  send(
    adapter,
    &session,
    "Reply with the deterministic greeting.",
    DeliveryMode::StartOrSteer,
  )
  .await?;
  completed(&mut events, &session, "Hello from the current DSH model.").await?;

  let session = create(adapter, project).await?;
  send(
    adapter,
    &session,
    "Ask the deterministic timed choice question.",
    DeliveryMode::StartOrSteer,
  )
  .await?;
  let request = event_matching(&mut events, |event| {
    event.session_id.as_deref() == Some(&session) && matches!(event.kind, RuntimeEventKind::InteractionRequested(_))
  })
  .await?;
  let RuntimeEventKind::InteractionRequested(question) = request.kind else {
    unreachable!()
  };
  assert_eq!(question.questions[0].question_id, "choice");
  assert_eq!(question.questions[0].options[0].label, "Blue");
  assert!(question.questions[0].multiple);
  if question.native["wire"]["value"]["request"]["wait"]["timed"] != true {
    return Err(format!("Expected a native timed question: {}", question.native).into());
  }
  // Answer inside the native foreground duration. A real client must run its
  // countdown while attachWait holds the host's unattended timer.
  tokio::time::sleep(Duration::from_secs(3)).await;
  assert_eq!(model.requests().len(), 2);
  let Reply::Interactions(pending) = execute(adapter, Some(&session), Command::PendingInteractions).await? else {
    panic!("Expected pending questions");
  };
  assert_eq!(pending.len(), 1);
  assert!(matches!(
    execute(
      adapter,
      Some(&session),
      Command::Respond {
        interaction_id: question.interaction_id,
        response: InteractionResponse::Answers {
          answers: vec![QuestionAnswer {
            question_id: "choice".into(),
            answers: vec!["Blue".into()],
            custom: Some("Keep it concise".into()),
          }]
        },
      }
    )
    .await?,
    Reply::Acknowledged
  ));
  completed(&mut events, &session, "Selected Blue with the supplied note.").await?;
  assert!(model.requests()[2].to_string().contains("Keep it concise"));

  let session = create(adapter, project).await?;
  send(adapter, &session, "Start the gated task.", DeliveryMode::StartOrSteer).await?;
  model.wait_for_requests(4).await?;
  send(adapter, &session, "Steer the current task.", DeliveryMode::StartOrSteer).await?;
  send(adapter, &session, "Run the queued follow-up.", DeliveryMode::Queue).await?;
  gate.release();
  completed(&mut events, &session, "The steering instruction was consumed.").await?;
  completed(&mut events, &session, "The queued follow-up completed.").await?;
  let requests = model.requests();
  assert!(requests[4].to_string().contains("Steer the current task."));
  assert!(requests[5].to_string().contains("Run the queued follow-up."));

  let session = create(adapter, project).await?;
  send(
    adapter,
    &session,
    "Start the cancellable task.",
    DeliveryMode::StartOrSteer,
  )
  .await?;
  model.wait_for_requests(7).await?;
  assert!(matches!(
    execute(adapter, Some(&session), Command::CancelTurn).await?,
    Reply::Acknowledged
  ));
  event_matching(&mut events, |event| {
    event.session_id.as_deref() == Some(&session)
      && matches!(&event.kind, RuntimeEventKind::State(state) if state.status == ExecutionState::Idle)
  })
  .await?;
  let Reply::State(state) = execute(adapter, Some(&session), Command::State).await? else {
    panic!("Expected final state")
  };
  assert_eq!(state.status, ExecutionState::Idle);
  assert_eq!(model.requests().len(), 7);
  Ok(())
}

#[tokio::test]
#[ignore = "launches published DSH with a loopback model; set TOKN_DSH_CURRENT_ROOT and TOKN_RPC_NODE"]
async fn verify_published_dsh_gateway() -> TestResult {
  let (gated, gate) = ModelStep::gated_text("Gated task reached its boundary.");
  let model = ModelServer::start(vec![
    ModelStep::text("Hello from the current DSH model."),
    ModelStep::tool_call("ask_user_question", json!({"timeout":10,"questions":[{"id":"choice","question":"Choose a color","multi_select":true,"options":[{"label":"Blue","description":"The deterministic choice"},{"label":"Red"}]}]})),
    ModelStep::text("Selected Blue with the supplied note."),
    gated,
    ModelStep::text("The steering instruction was consumed."),
    ModelStep::text("The queued follow-up completed."),
    ModelStep::stall(),
  ]).await?;
  let NativeProfile {
    directory,
    project,
    options,
  } = NativeProfile::prepare(&model.url())?;
  let _directory = if std::env::var_os("TOKN_DSH_CURRENT_KEEP").is_some() {
    eprintln!("Retained disposable native DSH profile: {}", directory.keep().display());
    None
  } else {
    Some(directory)
  };
  let adapter = DshAdapter::launch(options).await?;
  assert_eq!(adapter.descriptor().ownership, Ownership::Managed);
  let result = scenario(&adapter, &project, &model, &gate).await;
  let closed = adapter.close().await;
  eprintln!(
    "Published DSH {PACKAGE_VERSION}; source inspection {INSPECTED_REVISION}; model requests {}",
    model.requests().len()
  );
  result?;
  closed?;
  Ok(())
}
