//! Owned startup reads the child's native readiness line instead of probing a port.
use crate::{InteractionError, LaunchOptions};
use reqwest::Url;
use std::{process::Stdio, time::Duration};
use tokio::{
  io::{AsyncRead, AsyncReadExt},
  process::{Child, Command},
  sync::oneshot,
  task::JoinHandle,
};

pub(crate) struct Launch {
  pub endpoint: String,
  pub child: Option<Child>,
  pub pumps: Vec<JoinHandle<()>>,
}

impl Launch {
  pub async fn start(options: &LaunchOptions, password: &str) -> Result<Self, InteractionError> {
    Self::start_with_timeout(options, password, Duration::from_secs(30)).await
  }

  async fn start_with_timeout(
    options: &LaunchOptions,
    password: &str,
    timeout: Duration,
  ) -> Result<Self, InteractionError> {
    if options.runtime_id.trim().is_empty()
      || !std::path::Path::new(&options.directory).is_absolute()
      || !std::path::Path::new(&options.directory).is_dir()
    {
      return Err(InteractionError::invalid(
        "OpenCode runtime ID and an existing absolute launch directory are required",
      ));
    }
    let mut child = Command::new(&options.executable)
      .args(["serve", "--hostname", "127.0.0.1", "--port", &options.port.to_string()])
      .current_dir(&options.directory)
      .envs(&options.environment)
      .env("OPENCODE_SERVER_USERNAME", "tokn")
      .env("OPENCODE_SERVER_PASSWORD", password)
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .kill_on_drop(true)
      .spawn()
      .map_err(|_| InteractionError::unavailable("Could not launch the configured OpenCode executable"))?;
    let stdout = child
      .stdout
      .take()
      .ok_or_else(|| InteractionError::unavailable("Owned OpenCode stdout is unavailable"))?;
    let stderr = child
      .stderr
      .take()
      .ok_or_else(|| InteractionError::unavailable("Owned OpenCode stderr is unavailable"))?;
    let (ready, mut readiness) = oneshot::channel();
    let pumps = vec![
      tokio::spawn(stdout_pump(stdout, ready, options.port)),
      tokio::spawn(drain(stderr)),
    ];
    let mut launch = Self {
      endpoint: String::new(),
      child: Some(child),
      pumps,
    };
    let child = launch.child.as_mut().expect("owned OpenCode child");
    let result = tokio::time::timeout(timeout, async {
      tokio::select! {
        ready = &mut readiness => ready.map_err(|_| InteractionError::unavailable("OpenCode closed stdout before its native readiness URL")),
        _ = child.wait() => Err(InteractionError::unavailable("Owned OpenCode server exited before it published its native readiness URL")),
      }
    }).await;
    let endpoint = match result {
      Ok(Ok(endpoint)) => endpoint,
      result => {
        let _ = stop_child(child).await;
        return Err(match result {
          Ok(Err(error)) => error,
          _ => InteractionError::unavailable("Owned OpenCode readiness timed out"),
        });
      }
    };
    if child
      .try_wait()
      .map_err(|_| InteractionError::unavailable("Could not inspect owned OpenCode process"))?
      .is_some()
    {
      return Err(InteractionError::unavailable(
        "Owned OpenCode server exited after readiness",
      ));
    }
    launch.endpoint = endpoint;
    Ok(launch)
  }
}

pub(crate) async fn stop_child(child: &mut Child) -> Result<(), InteractionError> {
  if child
    .try_wait()
    .map_err(|_| InteractionError::unavailable("Could not inspect owned OpenCode process"))?
    .is_some()
  {
    return Ok(());
  }
  child
    .start_kill()
    .map_err(|_| InteractionError::unavailable("Could not stop owned OpenCode process"))?;
  tokio::time::timeout(Duration::from_secs(5), child.wait())
    .await
    .map_err(|_| InteractionError::unavailable("Timed out stopping owned OpenCode process"))?
    .map_err(|_| InteractionError::unavailable("Could not reap owned OpenCode process"))?;
  Ok(())
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

async fn stdout_pump(mut stdout: impl AsyncRead + Unpin, ready: oneshot::Sender<String>, expected_port: u16) {
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
          if let Ok(line) = std::str::from_utf8(&line) {
            if let Some(endpoint) = readiness_url(line.trim_end_matches('\r'), expected_port) {
              let _ = ready.take().expect("readiness sender").send(endpoint);
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
  let mut buffer = [0; 4096];
  while matches!(reader.read(&mut buffer).await, Ok(count) if count > 0) {}
}

fn readiness_url(line: &str, expected_port: u16) -> Option<String> {
  let endpoint = line.strip_prefix("opencode server listening on ")?;
  let url = Url::parse(endpoint).ok()?;
  let actual_port = url.port_or_known_default()?;
  if url.scheme() != "http"
    || url.host_str() != Some("127.0.0.1")
    || actual_port == 0
    || (expected_port != 0 && actual_port != expected_port)
    || url.path() != "/"
    || !url.username().is_empty()
    || url.password().is_some()
    || url.query().is_some()
    || url.fragment().is_some()
  {
    return None;
  }
  Some(url.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn readiness_is_exact_native_loopback_url_and_requested_port() {
    assert_eq!(
      readiness_url("opencode server listening on http://127.0.0.1:4321", 4321),
      Some("http://127.0.0.1:4321/".into())
    );
    assert!(readiness_url("opencode server listening on http://127.0.0.1:4321", 0).is_some());
    for line in [
      "http://127.0.0.1:4321",
      "prefix opencode server listening on http://127.0.0.1:4321",
      "opencode server listening on http://localhost:4321",
      "opencode server listening on http://127.0.0.1:4322",
      "opencode server listening on http://127.0.0.1:4321/path",
      "opencode server listening on http://user:pass@127.0.0.1:4321",
    ] {
      assert!(readiness_url(line, 4321).is_none(), "{line}");
    }
  }

  #[tokio::test]
  async fn readiness_output_is_bounded_and_drains_split_lines() {
    let bytes = format!(
      "{}\nopencode server listening on http://127.0.0.1:4321\r\nmore output\n",
      "x".repeat(20_000)
    );
    let (sender, receiver) = oneshot::channel();
    stdout_pump(bytes.as_bytes(), sender, 4321).await;
    assert_eq!(receiver.await.unwrap(), "http://127.0.0.1:4321/");
  }

  #[tokio::test]
  async fn oversized_or_unterminated_output_cannot_claim_readiness() {
    for bytes in [
      format!(
        "opencode server listening on http://127.0.0.1:4321{}\n",
        "x".repeat(9000)
      ),
      "opencode server listening on http://127.0.0.1:4321".into(),
    ] {
      let (sender, receiver) = oneshot::channel();
      stdout_pump(bytes.as_bytes(), sender, 4321).await;
      assert!(receiver.await.is_err());
    }
  }

  #[cfg(unix)]
  struct SyntheticExecutable(std::path::PathBuf);

  #[cfg(unix)]
  impl SyntheticExecutable {
    fn new(script: &str) -> Self {
      use std::os::unix::fs::PermissionsExt;
      let path = std::env::temp_dir().join(format!("tokn-opencode-launch-{}", uuid::Uuid::new_v4()));
      std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
      std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
      Self(path)
    }

    fn options(&self) -> LaunchOptions {
      LaunchOptions {
        runtime_id: "synthetic-opencode".into(),
        executable: self.0.clone(),
        directory: std::env::temp_dir().to_string_lossy().into_owned(),
        port: 0,
        environment: Default::default(),
      }
    }
  }

  #[cfg(unix)]
  impl Drop for SyntheticExecutable {
    fn drop(&mut self) {
      let _ = std::fs::remove_file(&self.0);
    }
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn owned_launch_uses_child_readiness_url_for_port_zero_and_reaps_child() {
    let executable =
      SyntheticExecutable::new("printf 'opencode server listening on http://127.0.0.1:4321\\n'\nexec /bin/sleep 30");
    let mut launch = Launch::start_with_timeout(&executable.options(), "test-secret", Duration::from_secs(2))
      .await
      .unwrap();
    assert_eq!(launch.endpoint, "http://127.0.0.1:4321/");
    let child = launch.child.as_mut().unwrap();
    assert!(child.try_wait().unwrap().is_none());
    stop_child(child).await.unwrap();
    assert!(child.try_wait().unwrap().is_some());
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn owned_launch_requires_readiness_from_its_child() {
    let executable = SyntheticExecutable::new("exec /bin/sleep 30");
    let result = Launch::start_with_timeout(&executable.options(), "test-secret", Duration::from_millis(100)).await;
    assert!(result.err().unwrap().message.contains("readiness timed out"));
    let executable = SyntheticExecutable::new("exit 1");
    let result = Launch::start_with_timeout(&executable.options(), "test-secret", Duration::from_secs(2)).await;
    assert!(result.is_err());
  }
}
