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
  /// Set only for a native generation verified to support --no-open. Older
  /// ApiProxy Web profiles do not open a browser and reject that flag.
  pub prevent_browser: bool,
  pub runtime_id: String,
  pub startup_timeout: Duration,
  pub shutdown_timeout: Duration,
  pub request_timeout: Duration,
  pub connect_timeout: Duration,
}

/// Failure of an owned launch, retaining whether its original child was reaped.
#[derive(Debug)]
pub struct ManagedLaunchError {
  pub error: InteractionError,
  pub cleanup_confirmed: bool,
}

impl ManagedLaunchError {
  fn before_spawn(error: InteractionError) -> Self {
    Self {
      error,
      cleanup_confirmed: true,
    }
  }
}

impl ManagedOptions {
  pub fn new(cwd: impl Into<PathBuf>) -> Self {
    Self {
      program: "dsh".into(),
      launcher_args: Vec::new(),
      cwd: cwd.into(),
      env: BTreeMap::new(),
      prevent_browser: false,
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
  pub(crate) launch_token: Option<String>,
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
  pub(crate) async fn start(options: &ManagedOptions) -> Result<Self, ManagedLaunchError> {
    if !options.cwd.is_absolute() || !options.cwd.is_dir() {
      return Err(ManagedLaunchError::before_spawn(InteractionError::invalid(
        "managed DSH cwd must be an existing absolute directory",
      )));
    }
    if options.startup_timeout.is_zero() || options.shutdown_timeout.is_zero() || options.runtime_id.trim().is_empty() {
      return Err(ManagedLaunchError::before_spawn(InteractionError::invalid(
        "managed DSH runtime id and positive lifecycle timeouts are required",
      )));
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
    if options.prevent_browser {
      command.arg("--no-open");
    }
    let child = command.spawn().map_err(|_| {
      ManagedLaunchError::before_spawn(InteractionError::unavailable(
        "could not launch the configured DSH executable",
      ))
    })?;
    let mut launch = Self {
      endpoint: String::new(),
      launch_token: None,
      child: Some(child),
      pumps: Vec::new(),
    };
    let child = launch.child.as_mut().expect("owned DSH child");
    let Some(stdout) = child.stdout.take() else {
      return Err(
        launch
          .failed(
            InteractionError::unavailable("managed DSH stdout was not piped"),
            options.shutdown_timeout,
          )
          .await,
      );
    };
    let Some(stderr) = child.stderr.take() else {
      return Err(
        launch
          .failed(
            InteractionError::unavailable("managed DSH stderr was not piped"),
            options.shutdown_timeout,
          )
          .await,
      );
    };
    let (ready, mut receiver) = oneshot::channel();
    launch.pumps = vec![tokio::spawn(stdout_pump(stdout, ready)), tokio::spawn(drain(stderr))];
    let child = launch.child.as_mut().expect("owned DSH child");
    let readiness = tokio::time::timeout(options.startup_timeout, async {
      tokio::select! {
        ready = &mut receiver => ready.map_err(|_| InteractionError::unavailable("DSH exited or closed stdout before its readiness URL"))?,
        exited = child.wait() => {
          let status = exited.map(|status| status.to_string()).unwrap_or_else(|_| "unknown status".into());
          Err(InteractionError::unavailable(format!("DSH exited before readiness ({status})")))
        }
      }
    }).await.unwrap_or_else(|_| Err(InteractionError::unavailable("DSH did not publish its native readiness URL before the startup timeout")));
    let readiness = match readiness {
      Ok(readiness) => readiness,
      Err(error) => return Err(launch.failed(error, options.shutdown_timeout).await),
    };
    match child.try_wait() {
      Ok(None) => {}
      Ok(Some(_)) => {
        return Err(
          launch
            .failed(
              InteractionError::unavailable("DSH exited immediately after readiness"),
              options.shutdown_timeout,
            )
            .await,
        );
      }
      Err(_) => {
        return Err(
          launch
            .failed(
              InteractionError::unavailable("could not inspect the owned DSH child"),
              options.shutdown_timeout,
            )
            .await,
        );
      }
    }
    launch.endpoint = readiness.endpoint;
    launch.launch_token = readiness.launch_token;
    Ok(launch)
  }

  pub(crate) async fn failed(mut self, error: InteractionError, shutdown_timeout: Duration) -> ManagedLaunchError {
    let cleanup_confirmed = match self.child.as_mut() {
      Some(child) => terminate(child, shutdown_timeout).await.is_ok(),
      None => false,
    };
    for task in self.pumps.drain(..) {
      task.abort();
      let _ = task.await;
    }
    ManagedLaunchError {
      error,
      cleanup_confirmed,
    }
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

async fn stdout_pump(mut stdout: impl AsyncRead + Unpin, ready: oneshot::Sender<Result<Readiness, InteractionError>>) {
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

#[derive(PartialEq, Eq)]
struct Readiness {
  endpoint: String,
  launch_token: Option<String>,
}

impl std::fmt::Debug for Readiness {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("Readiness")
      .field("endpoint", &self.endpoint)
      .field("authenticated", &self.launch_token.is_some())
      .finish()
  }
}

fn readiness_url(line: &str) -> Option<Readiness> {
  let value = line.strip_prefix("dsh web: ")?;
  let value = value.split_whitespace().next()?;
  let mut url = Url::parse(value).ok()?;
  if url.scheme() != "http"
    || url.host_str() != Some("127.0.0.1")
    || url.port().is_none_or(|port| port == 0)
    || !matches!(url.path(), "" | "/")
    || url.fragment().is_some()
    || !url.username().is_empty()
    || url.password().is_some()
  {
    return None;
  }
  let launch_token = if url.query().is_some() {
    let pairs = url.query_pairs().collect::<Vec<_>>();
    if pairs.len() != 1
      || pairs[0].0 != "token"
      || pairs[0].1.len() != 43
      || !pairs[0]
        .1
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
      return None;
    }
    Some(pairs[0].1.to_string())
  } else {
    None
  };
  url.set_query(None);
  Some(Readiness {
    endpoint: url.to_string(),
    launch_token,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn readiness_accepts_only_native_loopback_os_assigned_urls() {
    assert_eq!(
      readiness_url("dsh web: http://127.0.0.1:54321"),
      Some(Readiness {
        endpoint: "http://127.0.0.1:54321/".into(),
        launch_token: None
      })
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
    assert_eq!(receiver.await.unwrap().unwrap().endpoint, "http://127.0.0.1:54321/");
  }

  #[test]
  fn authenticated_readiness_keeps_the_launch_credential_out_of_the_endpoint_and_debug() {
    let token = "A".repeat(43);
    let ready = readiness_url(&format!("dsh web: http://127.0.0.1:54321/?token={token}")).unwrap();
    assert_eq!(ready.endpoint, "http://127.0.0.1:54321/");
    assert_eq!(ready.launch_token.as_deref(), Some(token.as_str()));
    assert!(!format!("{ready:?}").contains(&token));
    for query in [
      format!("token={token}&token={token}"),
      format!("token={token}&other=1"),
      "token=short".into(),
    ] {
      assert!(readiness_url(&format!("dsh web: http://127.0.0.1:54321/?{query}")).is_none());
    }
  }

  #[cfg(unix)]
  struct SyntheticExecutable {
    directory: tempfile::TempDir,
    executable: PathBuf,
    pid_file: PathBuf,
    args_file: PathBuf,
  }

  #[cfg(unix)]
  impl SyntheticExecutable {
    fn new(readiness: Option<&str>) -> Self {
      use std::os::unix::fs::PermissionsExt;
      let directory = tempfile::tempdir().unwrap();
      let executable = directory.path().join("synthetic-dsh");
      let pid_file = directory.path().join("child.pid");
      let args_file = directory.path().join("child.args");
      let announce = readiness
        .map(|ready| format!("printf '%s\\n' '{ready}'\n"))
        .unwrap_or_default();
      std::fs::write(
        &executable,
        format!("#!/bin/sh\nprintf '%s' \"$$\" > \"$TOKN_TEST_PID_FILE\"\nprintf '%s\\n' \"$@\" > \"$TOKN_TEST_ARGS_FILE\"\n{announce}exec /bin/sleep 30\n"),
      )
      .unwrap();
      std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
      Self {
        directory,
        executable,
        pid_file,
        args_file,
      }
    }

    fn options(&self) -> ManagedOptions {
      let mut options = ManagedOptions::new(self.directory.path());
      options.program = self.executable.clone();
      options.env.insert(
        "TOKN_TEST_PID_FILE".into(),
        self.pid_file.to_string_lossy().into_owned(),
      );
      options.env.insert(
        "TOKN_TEST_ARGS_FILE".into(),
        self.args_file.to_string_lossy().into_owned(),
      );
      options.startup_timeout = Duration::from_millis(200);
      options.shutdown_timeout = Duration::from_millis(50);
      options.connect_timeout = Duration::from_millis(50);
      options.request_timeout = Duration::from_millis(50);
      options
    }

    fn assert_reaped(&self) {
      let pid: libc::pid_t = std::fs::read_to_string(&self.pid_file).unwrap().parse().unwrap();
      assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
      assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn readiness_timeout_confirms_cleanup_only_after_reaping_the_original_child() {
    let executable = SyntheticExecutable::new(None);
    let failure = Launch::start(&executable.options()).await.err().unwrap();
    assert!(failure.cleanup_confirmed);
    assert!(failure.error.message.contains("readiness URL"));
    executable.assert_reaped();
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn failed_rpc_handshake_reaps_the_original_child_before_reporting_cleanup() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ready = format!("dsh web: http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let executable = SyntheticExecutable::new(Some(&ready));
    let failure = crate::DshAdapter::launch_with_cleanup(executable.options())
      .await
      .err()
      .unwrap();
    assert!(failure.cleanup_confirmed);
    executable.assert_reaped();
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn browser_suppression_is_explicit_to_keep_legacy_web_arguments_compatible() {
    for prevent_browser in [false, true] {
      let executable = SyntheticExecutable::new(None);
      let mut options = executable.options();
      options.prevent_browser = prevent_browser;
      let failure = Launch::start(&options).await.err().unwrap();
      assert!(failure.cleanup_confirmed);
      let arguments = std::fs::read_to_string(&executable.args_file).unwrap();
      assert_eq!(
        arguments.lines().any(|argument| argument == "--no-open"),
        prevent_browser
      );
      executable.assert_reaped();
    }
  }
}
