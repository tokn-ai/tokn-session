//! Captured native Pi RPC traffic is replayed without Node or model credentials.
//! The ignored capture test starts the exact pinned upstream agent against the
//! local scripted model server; it never reads a user's agent configuration.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
  collections::BTreeMap,
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
use tokn_interaction_core::*;
use tokn_interaction_tests::{ModelGate, ModelServer, ModelStep, TestResult};
use tokn_pi_adapter::{EndpointOptions, PiAdapter};

const REVISION: &str = "fdbedcad8aeb7c1e2ebd3b85f16e60e5fab0b2c3";
fn fixture_path() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/pi/session.json")
}
const INSTANCE: &str = "pi-fixture-instance";
const MODEL_DATA_SHA256: &str = "2f9df9522808b621cd3449876537f03d8a8df8b8d7ec2d5b18c6a910aa85b490";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Frame {
  direction: String,
  body: Value,
}
#[derive(Serialize, Deserialize)]
struct Fixture {
  provider: String,
  revision: String,
  agent_version: String,
  model_data_artifact_sha256: String,
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
      let body = serde_json::from_slice(&bytes).expect("real Pi emitted non-JSON stdout");
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
  let mut options = EndpointOptions::new("pi-fixture");
  options.instance_id = INSTANCE.into();
  options.request_timeout = Duration::from_secs(15);
  options.prompt_timeout = Duration::from_secs(30);
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
async fn working(events: &mut broadcast::Receiver<RuntimeEvent>) -> TestResult<()> {
  next_matching(events, |event| matches!(&event.kind, RuntimeEventKind::Native(value) if value.get("type").and_then(Value::as_str) == Some("message_update") && value.to_string().contains("working"))).await?;
  Ok(())
}

/// This identical conversation is exercised against a real native process and
/// the captured wire transcript. Every request shape/order must match replay.
async fn exercise(adapter: PiAdapter, gate: Option<ModelGate>) -> TestResult<()> {
  let mut target = SessionTarget {
    runtime_id: "pi-fixture".into(),
    instance_id: INSTANCE.into(),
    session_id: None,
  };
  let mut events = adapter.subscribe();
  let Reply::Metadata(metadata) = adapter.execute(request(&target, "metadata", Command::Metadata)).await? else {
    panic!("missing metadata")
  };
  assert_eq!(metadata.model.unwrap().model_id, "fixture-model");
  let Reply::State(state) = adapter.execute(request(&target, "state", Command::State)).await? else {
    panic!("missing state")
  };
  assert_eq!(state.status, ExecutionState::Idle);
  target.session_id = state.session_id;
  assert!(target.session_id.is_some());
  let Reply::Models(models) = adapter.execute(request(&target, "models", Command::Models)).await? else {
    panic!("missing models")
  };
  assert!(models.iter().any(|model| model.model.model_id == "fixture-model"));

  assert!(matches!(
    adapter
      .execute(request(&target, "hello", message("fixture hello", DeliveryMode::Start)))
      .await?,
    Reply::Admission(_)
  ));
  next_matching(&mut events, |event| matches!(&event.kind, RuntimeEventKind::Native(value) if value.get("type").and_then(Value::as_str) == Some("message_end") && value.to_string().contains("fixture hello complete"))).await?;

  let ask_adapter = adapter.clone();
  let ask_target = target.clone();
  let ask = tokio::spawn(async move {
    ask_adapter
      .execute(request(
        &ask_target,
        "question",
        message("fixture ask question", DeliveryMode::Start),
      ))
      .await
  });
  let event = next_matching(&mut events, |event| {
    matches!(&event.kind, RuntimeEventKind::InteractionRequested(_))
  })
  .await?;
  let RuntimeEventKind::InteractionRequested(question) = event.kind else {
    unreachable!()
  };
  assert_eq!(question.kind, InteractionKind::Select);
  assert_eq!(question.title, "Choose a fixture option");
  assert_eq!(
    question.questions[0]
      .options
      .iter()
      .map(|option| option.label.as_str())
      .collect::<Vec<_>>(),
    ["Alpha", "Beta"]
  );
  let Reply::Interactions(pending) = adapter
    .execute(request(&target, "pending", Command::PendingInteractions))
    .await?
  else {
    panic!("missing pending interactions")
  };
  assert_eq!(pending.len(), 1);
  assert!(matches!(
    adapter
      .execute(request(
        &target,
        "answer",
        Command::Respond {
          interaction_id: question.interaction_id,
          response: InteractionResponse::Dialog {
            value: Some("Beta".into()),
            confirmed: None,
            cancelled: false
          }
        }
      ))
      .await?,
    Reply::Acknowledged
  ));
  assert!(matches!(ask.await??, Reply::Admission(_)));
  next_matching(&mut events, |event| matches!(&event.kind, RuntimeEventKind::Native(value) if value.get("type").and_then(Value::as_str) == Some("message_end") && value.to_string().contains("fixture question complete"))).await?;

  let run_adapter = adapter.clone();
  let run_target = target.clone();
  let running = tokio::spawn(async move {
    run_adapter
      .execute(request(
        &run_target,
        "running",
        message("fixture run until released", DeliveryMode::Start),
      ))
      .await
  });
  working(&mut events).await?;
  assert!(matches!(
    adapter
      .execute(request(
        &target,
        "steer",
        message("fixture steer", DeliveryMode::StartOrSteer)
      ))
      .await?,
    Reply::Admission(_)
  ));
  assert!(matches!(
    adapter
      .execute(request(&target, "queue", message("fixture queue", DeliveryMode::Queue)))
      .await?,
    Reply::Admission(_)
  ));
  if let Some(gate) = gate {
    gate.release();
  }
  assert!(matches!(running.await??, Reply::Admission(_)));
  next_matching(&mut events, |event| matches!(&event.kind, RuntimeEventKind::Native(value) if value.get("type").and_then(Value::as_str) == Some("message_end") && value.to_string().contains("fixture queue complete"))).await?;

  let cancel_adapter = adapter.clone();
  let cancel_target = target.clone();
  let cancelled = tokio::spawn(async move {
    cancel_adapter
      .execute(request(
        &cancel_target,
        "cancelled-message",
        message("fixture cancel this", DeliveryMode::Start),
      ))
      .await
  });
  working(&mut events).await?;
  assert!(matches!(
    adapter.execute(request(&target, "cancel", Command::CancelTurn)).await?,
    Reply::Acknowledged
  ));
  assert!(matches!(cancelled.await??, Reply::Admission(_)));
  let Reply::State(state) = adapter.execute(request(&target, "settled", Command::State)).await? else {
    panic!("missing settled state")
  };
  assert_eq!(state.status, ExecutionState::Idle);
  adapter.close().await?;
  Ok(())
}

#[tokio::test]
async fn replay_real_pi_rpc_conversation() -> TestResult<()> {
  let fixture: Fixture = serde_json::from_str(&std::fs::read_to_string(fixture_path())?)?;
  assert_eq!(fixture.revision, REVISION);
  assert_eq!(fixture.agent_version, "0.82.1");
  assert_eq!(fixture.model_data_artifact_sha256, MODEL_DATA_SHA256);
  let (client, server) = tokio::io::duplex(1024 * 1024);
  let (reader, writer) = tokio::io::split(client);
  let (server_reader, mut server_writer) = tokio::io::split(server);
  let replay = tokio::spawn(async move {
    let mut lines = BufReader::new(server_reader).lines();
    for (index, frame) in fixture.frames.into_iter().enumerate() {
      if frame.direction == "request" {
        let actual: Value =
          serde_json::from_str(&lines.next_line().await?.ok_or("adapter closed before capture ended")?)?;
        assert_eq!(actual, frame.body, "captured Pi request {index} changed");
      } else {
        server_writer
          .write_all(serde_json::to_string(&frame.body)?.as_bytes())
          .await?;
        server_writer.write_all(b"\n").await?;
      }
    }
    // Keep the stream alive until the adapter detaches; EOF would race its final
    // authoritative get_state and change availability semantics.
    assert!(lines.next_line().await?.is_none());
    Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
  });
  exercise(PiAdapter::attach(options(), reader, writer)?, None).await?;
  replay.await??;
  Ok(())
}

#[tokio::test]
#[ignore = "requires pinned Pi built with scripts/rpc/prepare-pi.sh; local model server only"]
async fn capture_real_pi_rpc_conversation() -> TestResult<()> {
  let root = PathBuf::from(std::env::var("TOKN_PI_RPC_ROOT")?);
  assert_eq!(
    std::fs::read_to_string(root.join(".tokn-rpc-source-revision"))?.trim(),
    REVISION
  );
  assert_eq!(
    std::fs::read_to_string(root.join(".tokn-rpc-model-data-sha256"))?.trim(),
    MODEL_DATA_SHA256
  );
  let node = PathBuf::from(std::env::var("TOKN_RPC_NODE").unwrap_or_else(|_| "node".into()));
  let home = tempfile::tempdir()?;
  let agent_dir = home.path().join("agent");
  std::fs::create_dir(&agent_dir)?;
  let work = home.path().join("workspace");
  std::fs::create_dir(&work)?;
  let (busy_step, gate) = ModelStep::gated_text("fixture busy complete");
  let server = ModelServer::start(vec![
    ModelStep::text("fixture hello complete"),
    ModelStep::tool_call("fixture_question", json!({})),
    ModelStep::text("fixture question complete"),
    busy_step,
    ModelStep::text("fixture steer complete"),
    ModelStep::text("fixture queue complete"),
    ModelStep::stall(),
  ])
  .await?;
  std::fs::write(
    agent_dir.join("models.json"),
    serde_json::to_vec(
      &json!({"providers":{"fixture":{"baseUrl":server.url(),"api":"openai-completions","apiKey":"tokn-dummy-local-key","models":[{"id":"fixture-model","name":"Fixture Model","reasoning":false,"input":["text"],"contextWindow":32768,"maxTokens":1024,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}}]}}}),
    )?,
  )?;
  std::fs::write(agent_dir.join("settings.json"), br#"{"defaultProvider":"fixture","defaultModel":"fixture-model","defaultThinkingLevel":"off","compaction":{"enabled":false},"retry":{"enabled":false}}"#)?;
  let extension = home.path().join("question.mjs");
  std::fs::write(
    &extension,
    r#"export default function(pi) {
  pi.registerTool({ name: "fixture_question", label: "Fixture question", description: "Ask a deterministic test question", parameters: {type:"object",properties:{},required:[]},
    async execute(_id, _params, _signal, _update, ctx) {
      const answer = await ctx.ui.select("Choose a fixture option", ["Alpha", "Beta"]);
      return {content:[{type:"text",text:`Selected ${answer}`}],details:{answer}};
    }
  });
}"#,
  )?;
  let mut child = tokio::process::Command::new(node)
    .arg(root.join("packages/coding-agent/dist/cli.js"))
    .args([
      "--mode",
      "rpc",
      "--provider",
      "fixture",
      "--model",
      "fixture-model",
      "--thinking",
      "off",
      "--no-extensions",
      "--no-skills",
      "--no-prompt-templates",
      "--no-themes",
    ])
    .arg("--extension")
    .arg(&extension)
    .current_dir(&work)
    .env_clear()
    .env("HOME", home.path())
    .env("PI_CODING_AGENT_DIR", &agent_dir)
    .env("PATH", "/usr/bin:/bin")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .spawn()?;
  let stderr = child.stderr.take().unwrap();
  let stderr_task = tokio::spawn(async move {
    let mut lines = BufReader::new(stderr).lines();
    let mut output = String::new();
    while let Ok(Some(line)) = lines.next_line().await {
      output.push_str(&line);
      output.push('\n');
    }
    output
  });
  let frames: Frames = Arc::default();
  let reader = Capture::new(child.stdout.take().unwrap(), frames.clone(), "event");
  let writer = Capture::new(child.stdin.take().unwrap(), frames.clone(), "request");
  let result = exercise(PiAdapter::attach(options(), reader, writer)?, Some(gate)).await;
  let _ = child.kill().await;
  let stderr = stderr_task.await?;
  if let Err(error) = result {
    let captured = frames.lock().unwrap();
    let tail = &captured[captured.len().saturating_sub(8)..];
    return Err(
      format!(
        "{error}; Pi stderr: {stderr}; model requests: {}; native tail: {}",
        server.requests().len(),
        serde_json::to_string(tail)?
      )
      .into(),
    );
  }
  let requests = server.requests();
  assert_eq!(requests.len(), 7);
  assert!(requests[2]["messages"].to_string().contains("Selected Beta"));
  assert!(requests[4]["messages"].to_string().contains("fixture steer"));
  assert!(requests[5]["messages"].to_string().contains("fixture queue"));
  let mut frames = frames.lock().unwrap().clone();
  sanitize(&mut frames, home.path(), &server.url());
  let fixture = Fixture {
    provider: "pi".into(),
    revision: REVISION.into(),
    agent_version: "0.82.1".into(),
    model_data_artifact_sha256: MODEL_DATA_SHA256.into(),
    scenario: "metadata/state/models, prompt, tool question/reply, busy steer/queue, cancel".into(),
    frames,
  };
  let output = fixture_path();
  if std::env::var("TOKN_CAPTURE_RPC_FIXTURES").as_deref() == Ok("1") {
    std::fs::write(output, format!("{}\n", serde_json::to_string_pretty(&fixture)?))?;
  }
  Ok(())
}

fn sanitize(frames: &mut [Frame], home: &Path, base_url: &str) {
  fn walk(value: &mut Value, home: &str, base_url: &str, ids: &mut BTreeMap<String, String>) {
    match value {
      Value::Object(object) => {
        for (key, value) in object {
          if key == "sessionFile" && value.is_string() {
            *value = json!("/fixture/home/session.jsonl");
          } else if matches!(key.as_str(), "timestamp" | "timestampMs" | "duration" | "durationMs") && value.is_number()
          {
            *value = json!(0);
          } else {
            walk(value, home, base_url, ids);
          }
        }
      }
      Value::Array(array) => {
        for value in array {
          walk(value, home, base_url, ids);
        }
      }
      Value::String(text) => {
        *text = text
          .replace(home, "/fixture/home")
          .replace(base_url, "http://127.0.0.1:1/v1");
        // Native UUIDs identify sessions, dialogs and tool records. Preserve
        // equality while replacing entropy; request IDs remain unchanged.
        if text.len() == 36
          && text.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
              b == b'-'
            } else {
              b.is_ascii_hexdigit()
            }
          })
        {
          let next = format!("00000000-0000-4000-8000-{:012}", ids.len() + 1);
          *text = ids.entry(text.clone()).or_insert(next).clone();
        }
      }
      _ => {}
    }
  }
  let mut ids = BTreeMap::new();
  for frame in frames {
    walk(&mut frame.body, &home.to_string_lossy(), base_url, &mut ids);
  }
}
