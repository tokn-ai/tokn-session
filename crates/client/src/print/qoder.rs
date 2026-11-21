use super::{
  PrintAction, PrintSessionRequest, action_prompt, executor_from_env, executor_required_error, split_command_line,
};
use serde_json::json;
use std::{
  collections::BTreeMap,
  io::{IsTerminal, Write},
  path::{Path, PathBuf},
};
use tokn_interaction_core::{
  Command, DeliveryMode, ExecutionState, InteractionAdapter, InteractionKind, InteractionRequest, InteractionResponse,
  MessageInput, PermissionDecision, Reply, RuntimeEventKind,
};
use tokn_qoder_adapter::{LaunchOptions, QoderAdapter};
use tokn_session_qoder::QoderSessionSource;

pub(super) fn run(request: PrintSessionRequest) -> Result<(), String> {
  let executor = request
    .executor
    .or_else(|| executor_from_env(request.source))
    .ok_or_else(|| executor_required_error(request.source))?;
  let mut argv = split_command_line(&executor)?;
  if argv.is_empty() {
    return Err("Qoder executor cannot be empty".into());
  }
  if argv.iter().any(|argument| argument.contains("{prompt}")) {
    return Err("Qoder ACP sends prompts over JSON-RPC; executor prompt placeholders are not supported".into());
  }
  let executable = PathBuf::from(argv.remove(0));
  let directory = request
    .cwd
    .unwrap_or(std::env::current_dir().map_err(|err| err.to_string())?)
    .canonicalize()
    .map_err(|err| format!("could not resolve Qoder working directory: {err}"))?;
  let launch = prepare_executor(&argv, &directory)?;
  let root = launch.config_dir.join("projects");
  let environment = BTreeMap::from([(
    "QODER_CONFIG_DIR".to_owned(),
    launch
      .config_dir
      .to_str()
      .ok_or("Qoder configuration directory is not UTF-8")?
      .to_owned(),
  )]);
  let resume = resolve_resume(&request.action, &directory, &root)?;
  let prompt = action_prompt(&request.action).to_owned();
  if prompt.trim().is_empty() {
    return Err("Qoder ACP requires a nonempty prompt".into());
  }
  std::thread::spawn(move || {
    let runtime = tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
      .map_err(|err| format!("could not start Qoder ACP client: {err}"))?;
    runtime.block_on(run_acp(
      executable,
      launch.arguments,
      directory,
      environment,
      resume,
      prompt,
    ))
  })
  .join()
  .map_err(|_| "Qoder ACP client stopped unexpectedly".to_owned())?
}

struct PreparedExecutor {
  arguments: Vec<String>,
  config_dir: PathBuf,
}

fn prepare_executor(arguments: &[String], cwd: &Path) -> Result<PreparedExecutor, String> {
  let mut root = std::env::var_os("QODER_CONFIG_DIR")
    .filter(|root| !root.is_empty())
    .map(PathBuf::from);
  let mut index = 0;
  let mut forwarded = Vec::new();
  while index < arguments.len() {
    let argument = &arguments[index];
    if argument == "--config-dir" {
      index += 1;
      root = Some(PathBuf::from(
        arguments
          .get(index)
          .filter(|value| !value.is_empty() && !value.starts_with("--"))
          .ok_or("Qoder --config-dir requires a value")?,
      ));
    } else if let Some(value) = argument.strip_prefix("--config-dir=") {
      if value.is_empty() {
        return Err("Qoder --config-dir requires a value".into());
      }
      root = Some(PathBuf::from(value));
    } else if [
      "--cwd",
      "-w",
      "--continue",
      "-c",
      "--resume",
      "-r",
      "--session-id",
      "--no-session-persistence",
      "--print",
      "-p",
    ]
    .iter()
    .any(|flag| argument == flag || argument.starts_with(&format!("{flag}=")))
      || ["-w", "-c", "-r", "-p"]
        .iter()
        .any(|flag| argument.starts_with(flag) && argument.len() > flag.len())
    {
      return Err(
        "Qoder executor must not override its working directory, session, or ACP mode; use tokn-session options".into(),
      );
    } else {
      forwarded.push(argument.clone());
    }
    index += 1;
  }
  let root = match root {
    Some(root) => root,
    None => tokn_session_qoder::default_session_root()?
      .parent()
      .ok_or("Qoder configuration directory is unavailable")?
      .to_path_buf(),
  };
  Ok(PreparedExecutor {
    arguments: forwarded,
    config_dir: if root.is_absolute() { root } else { cwd.join(root) },
  })
}

fn resolve_resume(action: &PrintAction, cwd: &Path, root: &Path) -> Result<Option<String>, String> {
  let expected = match action {
    PrintAction::Create { .. } => return Ok(None),
    PrintAction::Session { session_id, .. } => Some(session_id.as_str()),
    PrintAction::Continue { .. } => None,
  };
  let source = QoderSessionSource::new(Some(root.to_path_buf()));
  let project = root.join(tokn_session_qoder::project_identifier(
    cwd.to_str().ok_or("Qoder working directory is not UTF-8")?,
  ));
  if let Some(id) = expected {
    if id.is_empty()
      || id.len() > 256
      || id.contains("..")
      || !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
      return Err("Qoder append requires an exact native root session ID".into());
    }
    let header = source.session_header_exact(&project.join(format!("{id}.jsonl")), id)?;
    if header.parent_session_id.is_some()
      || header
        .cwd
        .as_ref()
        .and_then(|cwd| Path::new(cwd).canonicalize().ok())
        .as_deref()
        != Some(cwd)
    {
      return Err("Qoder session does not belong to this working directory".into());
    }
    return Ok(Some(id.to_owned()));
  }
  let catalog = QoderSessionSource::new(Some(project.clone())).list_session_relation_catalog()?;
  if !catalog.complete {
    return Err(
      "Qoder session storage is incomplete; use an exact --session ID rather than guessing the latest session".into(),
    );
  }
  let mut candidates = Vec::new();
  for reference in catalog.references {
    if reference.parent_session_id.is_some() || expected.is_some_and(|id| id != reference.id) {
      continue;
    }
    let header = source.session_header_exact(&reference.path, &reference.id)?;
    let Some(native_cwd) = header.cwd.as_ref() else {
      continue;
    };
    if Path::new(native_cwd).canonicalize().ok().as_deref() != Some(cwd) {
      continue;
    }
    let Some(native_path) = project.join(format!("{}.jsonl", reference.id)).canonicalize().ok() else {
      continue;
    };
    if reference.path.canonicalize().ok().as_deref() != Some(native_path.as_path()) {
      continue;
    }
    candidates.push(header);
  }
  candidates.sort_by(|a, b| {
    b.updated_at_ms
      .cmp(&a.updated_at_ms)
      .then_with(|| b.timestamp.cmp(&a.timestamp))
      .then_with(|| b.id.cmp(&a.id))
  });
  if expected.is_some() && candidates.len() > 1 {
    return Err("Qoder session ID is ambiguous in this local storage".into());
  }
  candidates
    .into_iter()
    .next()
    .map(|header| Some(header.id))
    .ok_or_else(|| "No matching persisted Qoder root session exists in this working directory and configuration".into())
}

async fn run_acp(
  executable: PathBuf,
  arguments: Vec<String>,
  directory: PathBuf,
  environment: BTreeMap<String, String>,
  resume: Option<String>,
  prompt: String,
) -> Result<(), String> {
  let adapter = QoderAdapter::launch(LaunchOptions {
    runtime_id: "cli:qoder".into(),
    executable,
    arguments,
    directory,
    environment,
  })
  .await
  .map_err(|err| err.to_string())?;
  let result = run_session(&adapter, resume, prompt).await;
  let closed = adapter.close().await.map_err(|err| err.to_string());
  result.and(closed)
}

async fn run_session(adapter: &QoderAdapter, resume: Option<String>, prompt: String) -> Result<(), String> {
  let mut events = adapter.subscribe();
  let created = match resume {
    Some(id) => adapter.resume_session(&id).await,
    None => adapter.create_session(None).await,
  }
  .map_err(|err| err.to_string())?;
  let target = created.target;
  emit(&json!({"type":"session_started","provider":"qoder","session_id":target.session_id}))?;
  let request = InteractionRequest {
    request_id: uuid::Uuid::new_v4().to_string(),
    target: target.clone(),
    command: Command::Message {
      input: MessageInput {
        text: prompt,
        ..MessageInput::default()
      },
      delivery: DeliveryMode::Prompt,
    },
  };
  let prompt_id = request.request_id.clone();
  let admission = adapter.execute(request);
  tokio::pin!(admission);
  let mut admitted = false;
  let mut running = false;
  let mut finished = false;
  let mut terminal_status = "idle";
  let mut stop_reason = None;
  let mut terminal_error = None;
  loop {
    tokio::select! {
      result = &mut admission, if !admitted => {
        match result.map_err(|err| err.to_string())? {
          Reply::Admission(_) | Reply::Acknowledged => admitted = true,
          _ => return Err("Qoder ACP did not confirm prompt admission".into()),
        }
      }
      event = events.recv() => {
        let event = event.map_err(|_| "Qoder ACP event stream disconnected or lost updates".to_owned())?;
        if let RuntimeEventKind::Disconnected { reason } = &event.kind { return Err(format!("Qoder ACP disconnected: {reason}")); }
        if event.session_id != target.session_id { continue; }
        match &event.kind {
          RuntimeEventKind::State(state) => {
            if state.status == ExecutionState::Running { running = true; }
            if running && matches!(state.status, ExecutionState::Idle | ExecutionState::Stopped) { finished = true; terminal_status = if state.status == ExecutionState::Stopped { "stopped" } else { "idle" }; }
          }
          RuntimeEventKind::InteractionRequested(interaction) => {
            let terminal = std::io::stdin().is_terminal();
            let allowed = if terminal && interaction.kind == InteractionKind::Permission {
              eprint!("Qoder requests permission: {}\nAllow once? [y/N] ", interaction.title);
              std::io::stderr().flush().map_err(|err| err.to_string())?;
              tokio::task::spawn_blocking(|| { let mut line = String::new(); std::io::stdin().read_line(&mut line).ok(); line.trim().eq_ignore_ascii_case("y") }).await.unwrap_or(false)
            } else { false };
            let response = if interaction.kind == InteractionKind::Permission { InteractionResponse::Permission { decision: if allowed { PermissionDecision::AllowOnce } else { PermissionDecision::Deny } } } else { InteractionResponse::Reject };
            adapter.execute(InteractionRequest { request_id: uuid::Uuid::new_v4().to_string(), target: target.clone(), command: Command::Respond { interaction_id: interaction.interaction_id.clone(), response } }).await.map_err(|err| err.to_string())?;
            if !terminal || interaction.kind != InteractionKind::Permission {
              let _ = adapter.execute(InteractionRequest { request_id: uuid::Uuid::new_v4().to_string(), target: target.clone(), command: Command::CancelTurn }).await;
              return Err("Qoder requested an interaction that this non-interactive ACP command cannot approve; the request was denied. Run Qoder interactively to continue.".into());
            }
          }
          RuntimeEventKind::Native(value) if value["type"] == "qoder.prompt_result" && value["prompt_id"] == prompt_id => {
            stop_reason = value["stop_reason"].as_str().map(str::to_owned);
            terminal_error = match stop_reason.as_deref() {
              Some("cancelled") => Some("Qoder ACP cancelled the prompt".to_owned()),
              Some("refusal") => Some("Qoder ACP refused the prompt".to_owned()),
              _ if value.get("error_kind").is_some() => Some("Qoder ACP prompt failed; its outcome may be uncertain, so it was not retried".to_owned()),
              _ => None,
            };
          }
          RuntimeEventKind::Disconnected { reason } => return Err(format!("Qoder ACP disconnected: {reason}")),
          _ => {}
        }
        emit(&serde_json::to_value(event).map_err(|err| err.to_string())?)?;
        if let Some(error) = terminal_error.take() { return Err(error); }
      }
    }
    if admitted && finished && stop_reason.is_some() {
      emit(
        &json!({"type":"result","provider":"qoder","session_id":target.session_id,"state":terminal_status,"stop_reason":stop_reason}),
      )?;
      return if terminal_status == "stopped" {
        Err("Qoder ACP stopped before completing the prompt".into())
      } else {
        Ok(())
      };
    }
  }
}

fn emit(value: &serde_json::Value) -> Result<(), String> {
  let mut output = std::io::stdout().lock();
  serde_json::to_writer(&mut output, value).map_err(|err| err.to_string())?;
  output
    .write_all(b"\n")
    .and_then(|_| output.flush())
    .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn executor_storage_and_mode_flags_are_resolved_before_launch() {
    let cwd = Path::new("/workspace/demo");
    assert_eq!(
      prepare_executor(&["--config-dir".into(), "private".into()], cwd)
        .unwrap()
        .config_dir,
      cwd.join("private")
    );
    assert_eq!(
      prepare_executor(&["--config-dir=/fixture/config".into()], cwd)
        .unwrap()
        .config_dir,
      Path::new("/fixture/config")
    );
    for flag in ["--resume=elsewhere", "--cwd", "--print", "--no-session-persistence"] {
      assert!(prepare_executor(&[flag.into()], cwd).is_err());
    }
  }
  #[test]
  fn continue_selects_latest_exact_project_root_and_never_a_subagent() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().canonicalize().unwrap();
    let projects = directory.path().join("projects");
    let project = projects.join(tokn_session_qoder::project_identifier(cwd.to_str().unwrap()));
    std::fs::create_dir_all(&project).unwrap();
    let row = |id: &str, scope: &str| json!({"type":"user","uuid":"user","sessionId":id,"cwd":scope,"timestamp":"2026-11-20T00:00:00Z","message":{"role":"user","content":"fixture"}});
    std::fs::write(
      project.join("owned.jsonl"),
      format!("{}\n", row("owned", cwd.to_str().unwrap())),
    )
    .unwrap();
    std::fs::write(
      project.join("other.jsonl"),
      format!("{}\n", row("other", "/another/project")),
    )
    .unwrap();
    let action = PrintAction::Continue {
      prompt: "fixture".into(),
    };
    assert_eq!(
      resolve_resume(&action, &cwd, &projects).unwrap().as_deref(),
      Some("owned")
    );
    assert!(
      resolve_resume(
        &PrintAction::Session {
          session_id: "own".into(),
          prompt: "fixture".into()
        },
        &cwd,
        &projects
      )
      .is_err()
    );
  }
}
