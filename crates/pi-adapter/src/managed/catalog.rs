//! Disposable Pi model discovery. No session file or prompt is admitted.
use super::protocol::{parse_state, pi_choices};
use crate::{EndpointOptions, LaunchOptions, PiAdapter, SupervisedSession};
use serde_json::Value;
use std::{path::PathBuf, time::Duration};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokn_interaction_core::ModelChoice;
use tokn_pi_rpc_protocol as wire;

const DEADLINE: Duration = Duration::from_secs(15);
static PROBE_SLOTS: Semaphore = Semaphore::const_new(2);

pub(super) async fn models(executable: PathBuf, working_directory: PathBuf) -> Result<Vec<ModelChoice>, String> {
  models_with_timeout(executable, working_directory, DEADLINE).await
}

pub(super) async fn models_with_timeout(
  executable: PathBuf,
  working_directory: PathBuf,
  deadline: Duration,
) -> Result<Vec<ModelChoice>, String> {
  let permit = PROBE_SLOTS
    .try_acquire()
    .map_err(|_| "Pi model discovery is busy; retry when the current query finishes")?;
  let cancelled = CancellationToken::new();
  let _cancel_on_drop = cancelled.clone().drop_guard();
  // A dropped UI request cancels the query but leaves this task responsible
  // for stopping and reaping its process before releasing the probe slot.
  tokio::spawn(async move {
    let _permit = permit;
    let mut options = EndpointOptions::new(format!("pi-model-catalog:{}", uuid::Uuid::new_v4()));
    options.request_timeout = deadline;
    let mut session = PiAdapter::spawn_supervised(
      options,
      LaunchOptions {
        executable,
        working_directory,
        arguments: vec!["--no-session".into()],
        environment: Default::default(),
        session_file: None,
      },
    )
    .await
    .map_err(|error| error.to_string())?;
    let result = tokio::select! {
      biased;
      _ = cancelled.cancelled() => Err("Pi model discovery was cancelled".into()),
      result = tokio::time::timeout(deadline, query(&mut session)) => {
        result.unwrap_or_else(|_| Err("Pi model discovery timed out".into()))
      }
    };
    session.close().await.map_err(|error| error.to_string())?;
    result
  })
  .await
  .map_err(|_| "Pi model discovery was interrupted".to_owned())?
}

async fn query(session: &mut SupervisedSession) -> Result<Vec<ModelChoice>, String> {
  let state = response(session, wire::Command::GetState).await?;
  if parse_state(&state)?.session_file.is_some() {
    return Err("Pi model discovery unexpectedly opened a saved session".into());
  }
  let catalog = response(session, wire::Command::GetAvailableModels).await?;
  // Native frames and the supervised queue are bounded by the adapter; the
  // protocol parser additionally caps catalog count and identity lengths.
  pi_choices(catalog.get("models").ok_or("Pi model catalog is missing")?)
}

async fn response(session: &mut SupervisedSession, command: wire::Command) -> Result<Value, String> {
  let id = uuid::Uuid::new_v4().to_string();
  session
    .send(id.clone(), command)
    .await
    .map_err(|error| error.to_string())?;
  for _ in 0..64 {
    let value = session.recv().await.ok_or("Pi closed model discovery")??;
    if value.get("id").and_then(Value::as_str) == Some(id.as_str()) && value["type"] == "response" {
      if value["success"] != true {
        return Err(value["error"].as_str().unwrap_or("Pi rejected model discovery").into());
      }
      return value
        .get("data")
        .cloned()
        .ok_or("Pi model discovery returned no data".into());
    }
    if value["type"] == "extension_ui_request"
      && !value["method"].as_str().is_some_and(wire::is_cosmetic_extension_method)
    {
      return Err("Pi model discovery requires extension input; complete its setup before retrying".into());
    }
  }
  Err("Pi model discovery emitted too many unrelated events".into())
}
