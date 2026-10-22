//! Ownership of a directly launched native `dsh --profile web` child.
use crate::{Inner, InteractionError, RuntimeEventKind};
use reqwest::Url;
use std::{
  collections::BTreeMap,
  path::PathBuf,
  process::Stdio,
  sync::{Arc, atomic::Ordering},
  time::Duration,
};
use tokio::{
  io::{AsyncRead, AsyncReadExt},
  process::{Child, Command},
  sync::oneshot,
  task::JoinHandle,
};

pub struct ManagedOptions {
  /// Native executable, or a launcher such as Node with its script in launcher_args.
  pub program: PathBuf,
  /// Launcher prefix only. The adapter appends the complete native profile/bind arguments.
  pub launcher_args: Vec<String>,
  pub cwd: PathBuf,
  /// Inherited environment with these explicit overrides; credentials stay local.
  pub env: BTreeMap<String, String>,
  pub runtime_id: String,
  pub startup_timeout: Duration,
  pub shutdown_timeout: Duration,
  pub request_timeout: Duration,
  pub connect_timeout: Duration,
}

impl ManagedOptions {
  pub fn new(cwd: impl Into<PathBuf>) -> Self {
    Self {
      program: "dsh".into(),
      launcher_args: Vec::new(),
      cwd: cwd.into(),
      env: BTreeMap::new(),
      runtime_id: format!("dsh:{}", uuid::Uuid::new_v4()),
      startup_timeout: Duration::from_secs(30),
      shutdown_timeout: Duration::from_secs(7),
      request_timeout: Duration::from_secs(15),
      connect_timeout: Duration::from_secs(5),
    }
  }
}

pub(crate) struct Launch {
  pub(crate) endpoint: String,
  child: Option<Child>,
  pumps: Vec<JoinHandle<()>>,
}

struct OutputPumps(Vec<JoinHandle<()>>);

impl Drop for OutputPumps {
  fn drop(&mut self) {
    for task in &self.0 {
      task.abort();
    }
  }
}

impl Launch {
  pub(crate) async fn start(options: &ManagedOptions) -> Result<Self, InteractionError> {
    if !options.cwd.is_absolute() || !options.cwd.is_dir() {
      return Err(InteractionError::invalid(
        "managed DSH cwd must be an existing absolute directory",
      ));
    }
    if options.startup_timeout.is_zero() || options.shutdown_timeout.is_zero() || options.runtime_id.trim().is_empty() {
      return Err(InteractionError::invalid(
        "managed DSH runtime id and positive lifecycle timeouts are required",
      ));
    }
    let mut command = Command::new(&options.program);
    command
      .args(&options.launcher_args)
      .args(["--profile", "web", "--host", "127.0.0.1", "--port", "0"])
      .current_dir(&options.cwd)
      .envs(&options.env)
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .kill_on_drop(true);
    let mut child = command
      .spawn()
      .map_err(|_| InteractionError::unavailable("could not launch the configured DSH executable"))?;
    let stdout = child
      .stdout
      .take()
      .ok_or_else(|| InteractionError::unavailable("managed DSH stdout was not piped"))?;
    let stderr = child
      .stderr
      .take()
      .ok_or_else(|| InteractionError::unavailable("managed DSH stderr was not piped"))?;
    let (ready, mut receiver) = oneshot::channel();
    let pumps = vec![tokio::spawn(stdout_pump(stdout, ready)), tokio::spawn(drain(stderr))];
    let mut launch = Self {
      endpoint: String::new(),
      child: Some(child),
      pumps,
    };
    let child = launch.child.as_mut().expect("owned DSH child");
    let endpoint = tokio::time::timeout(options.startup_timeout, async {
      tokio::select! {
        ready = &mut receiver => ready.map_err(|_| InteractionError::unavailable("DSH exited or closed stdout before its readiness URL"))?,
        exited = child.wait() => {
          let status = exited.map(|status| status.to_string()).unwrap_or_else(|_| "unknown status".into());
          Err(InteractionError::unavailable(format!("DSH exited before readiness ({status})")))
        }
      }
    }).await.map_err(|_| InteractionError::unavailable("DSH did not publish its native readiness URL before the startup timeout"))??;
    if child
      .try_wait()
      .map_err(|_| InteractionError::unavailable("could not inspect the owned DSH child"))?
      .is_some()
    {
      return Err(InteractionError::unavailable("DSH exited immediately after readiness"));
    }
    launch.endpoint = endpoint;
    Ok(launch)
  }

  pub(crate) fn supervise(&mut self, inner: Arc<Inner>, shutdown_timeout: Duration) -> OwnedRuntime {
    let mut child = self.child.take().expect("owned DSH child");
    let mut pumps = OutputPumps(std::mem::take(&mut self.pumps));
    let (stop, mut receiver) = oneshot::channel();
    let task = tokio::spawn(async move {
      let result = tokio::select! {
        _ = &mut receiver => terminate(&mut child, shutdown_timeout).await,
        exited = child.wait() => exited.map(|_| ()).map_err(|_| InteractionError::unavailable("could not reap the owned DSH child")),
      };
      for pump in std::mem::take(&mut pumps.0) {
        pump.abort();
        let _ = pump.await;
      }
      if !inner.closed.load(Ordering::Acquire) && !inner.disconnected.swap(true, Ordering::AcqRel) {
        let _ = inner.shutdown.send(true);
        inner.emit(
          None,
          RuntimeEventKind::Disconnected {
            reason: "The owned DSH runtime exited".into(),
          },
        );
      }
      result
    });
    OwnedRuntime {
      stop: Some(stop),
      task: Some(task),
      shutdown_timeout,
    }
  }
}

impl Drop for Launch {
  fn drop(&mut self) {
    if let Some(child) = &mut self.child {
      let _ = child.start_kill();
    }
    for pump in self.pumps.drain(..) {
      pump.abort();
    }
  }
}

pub(crate) struct OwnedRuntime {
  stop: Option<oneshot::Sender<()>>,
  task: Option<JoinHandle<Result<(), InteractionError>>>,
  shutdown_timeout: Duration,
}

impl OwnedRuntime {
  pub(crate) async fn close(mut self) -> Result<(), InteractionError> {
    if let Some(stop) = self.stop.take() {
      let _ = stop.send(());
    }
    let mut task = self.task.take().expect("owned DSH supervisor");
    match tokio::time::timeout(self.shutdown_timeout + Duration::from_secs(3), &mut task).await {
      Ok(result) => result.map_err(|_| InteractionError::unavailable("owned DSH supervisor failed"))?,
      Err(_) => {
        task.abort();
        let _ = task.await;
        Err(InteractionError::unavailable("owned DSH shutdown did not settle"))
      }
    }
  }
}

impl Drop for OwnedRuntime {
  fn drop(&mut self) {
    // The supervisor owns Child with kill_on_drop and performs bounded cleanup
    // even when an adapter is dropped without an awaited close.
    if let Some(stop) = self.stop.take() {
      let _ = stop.send(());
    }
  }
}

async fn terminate(child: &mut Child, grace: Duration) -> Result<(), InteractionError> {
  if child
    .try_wait()
    .map_err(|_| InteractionError::unavailable("could not inspect owned DSH process"))?
    .is_some()
  {
    return Ok(());
  }
  #[cfg(unix)]
  if let Some(pid) = child.id() {
    // This PID belongs to the unreaped Child. It cannot be reused until wait.
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if result != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
      return Err(InteractionError::unavailable("could not signal the owned DSH process"));
    }
  }
  #[cfg(not(unix))]
  child
    .start_kill()
    .map_err(|_| InteractionError::unavailable("could not stop the owned DSH process"))?;
  if let Ok(result) = tokio::time::timeout(grace, child.wait()).await {
    result.map_err(|_| InteractionError::unavailable("could not reap the owned DSH process"))?;
    return Ok(());
  }
  child
    .start_kill()
    .map_err(|_| InteractionError::unavailable("could not kill the owned DSH process after its grace period"))?;
  tokio::time::timeout(Duration::from_secs(2), child.wait())
    .await
    .map_err(|_| InteractionError::unavailable("owned DSH process could not be reaped after kill"))?
    .map_err(|_| InteractionError::unavailable("could not reap the owned DSH process"))?;
  Ok(())
}

async fn stdout_pump(mut stdout: impl AsyncRead + Unpin, ready: oneshot::Sender<Result<String, InteractionError>>) {
  let mut ready = Some(ready);
  let mut line = Vec::new();
  let mut discard = false;
  let mut bytes = [0; 4096];
  loop {
    let count = match stdout.read(&mut bytes).await {
      Ok(0) | Err(_) => return,
      Ok(count) => count,
    };
    for byte in &bytes[..count] {
      if *byte == b'\n' {
        if !discard && ready.is_some() {
          if let Ok(text) = std::str::from_utf8(&line) {
            if let Some(endpoint) = readiness_url(text.trim_end_matches('\r')) {
              let _ = ready.take().expect("DSH readiness sender").send(Ok(endpoint));
            }
          }
        }
        line.clear();
        discard = false;
      } else if !discard {
        if line.len() >= 8192 {
          line.clear();
          discard = true;
        } else {
          line.push(*byte);
        }
      }
    }
  }
}

async fn drain(mut reader: impl AsyncRead + Unpin) {
  let mut bytes = [0; 4096];
  while matches!(reader.read(&mut bytes).await, Ok(count) if count > 0) {}
}

fn readiness_url(line: &str) -> Option<String> {
  let value = line.strip_prefix("dsh web: ")?;
  let value = value.split_whitespace().next()?;
  let url = Url::parse(value).ok()?;
  if url.scheme() != "http"
    || url.host_str() != Some("127.0.0.1")
    || url.port().is_none_or(|port| port == 0)
    || !matches!(url.path(), "" | "/")
    || url.query().is_some()
    || url.fragment().is_some()
    || !url.username().is_empty()
    || url.password().is_some()
  {
    return None;
  }
  Some(url.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn readiness_accepts_only_native_loopback_os_assigned_urls() {
    assert_eq!(
      readiness_url("dsh web: http://127.0.0.1:54321"),
      Some("http://127.0.0.1:54321/".into())
    );
    for line in [
      "http://127.0.0.1:54321",
      "log dsh web: http://127.0.0.1:54321",
      "dsh web: http://localhost:54321",
      "dsh web: http://127.0.0.1:0",
      "dsh web: http://example.com:54321",
      "dsh web: http://127.0.0.1:54321/path",
    ] {
      assert!(readiness_url(line).is_none(), "{line}");
    }
  }

  #[tokio::test]
  async fn readiness_parser_is_bounded_and_drains_split_lines_without_launching_a_process() {
    let data = format!("{}\ndsh web: http://127.0.0.1:54321\r\n", "x".repeat(20_000));
    let (sender, receiver) = oneshot::channel();
    stdout_pump(data.as_bytes(), sender).await;
    assert_eq!(receiver.await.unwrap().unwrap(), "http://127.0.0.1:54321/");
  }
}
