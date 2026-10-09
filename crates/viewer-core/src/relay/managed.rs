use std::{collections::HashSet, path::Path, process::Stdio, sync::Arc, time::Duration};

use tokio::{
  io::{AsyncBufReadExt, AsyncReadExt, BufReader},
  process::{Child, ChildStdin, Command},
};
use tokio_util::sync::CancellationToken;

use super::ViewerRelay;
use tokn_session_relay::stdio::CHILD_FLAG;

const START_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_ATTEMPTS: usize = 3;

struct ManagedChild {
  child: Child,
  output: BufReader<tokio::process::ChildStdout>,
  // Child::wait closes child.stdin automatically. Keep the lifetime handle
  // separately or merely monitoring exit would terminate a healthy Relay.
  lifetime: Option<ChildStdin>,
}

impl ManagedChild {
  fn spawn(executable: &Path) -> Result<Self, String> {
    let mut command = Command::new(executable);
    command
      .arg(CHILD_FLAG)
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::inherit())
      .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    Self::spawn_command(&mut command)
  }

  fn spawn_command(command: &mut Command) -> Result<Self, String> {
    let mut child = command
      .spawn()
      .map_err(|e| format!("Could not start bundled Relay: {e}"))?;
    let lifetime = child.stdin.take();
    let output = BufReader::new(child.stdout.take().ok_or("Relay output pipe is missing")?);
    Ok(Self {
      child,
      output,
      lifetime,
    })
  }

  async fn line(&mut self) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    (&mut self.output)
      .take((tokn_session_relay::stdio::MAX_LINE_BYTES + 1) as u64)
      .read_until(b'\n', &mut bytes)
      .await
      .map_err(|e| e.to_string())?;
    if bytes.last() != Some(&b'\n') || bytes.len() > tokn_session_relay::stdio::MAX_LINE_BYTES {
      return Err("Relay pipe closed or exceeded the frame limit".into());
    }
    Ok(bytes)
  }

  async fn ready(&mut self) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_slice(&self.line().await?).map_err(|e| e.to_string())?;
    if value["type"] != "ready" || value["version"] != tokn_session_relay::stdio::VERSION {
      return Err("Invalid Relay pipe handshake".into());
    }
    Ok(())
  }

  async fn consume(&mut self, snapshots: &crate::service_server::Service, manager: &ViewerRelay) -> Result<(), String> {
    loop {
      let changes: tokn_session_relay::stdio::SessionChanges =
        serde_json::from_slice(&self.line().await?).map_err(|e| format!("Invalid Relay changes: {e}"))?;
      snapshots.invalidate_sessions(&changes.sessions).await;
      let mut changed_paths = HashSet::new();
      for hint in changes.sessions {
        if let Some(provider) = super::viewer_provider(hint.provider) {
          manager.changed_session_source(provider, &hint.path, Some(&hint.session_id));
          changed_paths.insert((provider, hint.path));
        }
      }
      for path in changed_paths {
        let _ = manager.index_wakes.send(path);
      }
    }
  }

  async fn stop(&mut self) {
    // EOF is the graceful lifetime signal; kill only this child if it stalls.
    drop(self.lifetime.take());
    if tokio::time::timeout(STOP_TIMEOUT, self.child.wait()).await.is_err() {
      let _ = self.child.kill().await;
    }
  }
}

impl ViewerRelay {
  pub(super) fn snapshot_service(&self, native: bool) -> Result<Arc<crate::service_server::Service>, String> {
    let mut config = tokn_session_relay::stdio::default_config(native)?;
    config.poll_interval = Duration::from_millis(500);
    match &self.index {
      Some(index) => crate::service_server::Service::from_index(config, index.clone()),
      None => crate::service_server::Service::new(config),
    }
  }

  pub(super) async fn run_managed(self: Arc<Self>, epoch: u64, cancel: CancellationToken, native: bool) {
    let executable = match std::env::current_exe() {
      Ok(path) => path,
      Err(error) => {
        self.managed_phase(epoch, "failed", Some(error.to_string()));
        return;
      }
    };
    self.supervise(&executable, epoch, cancel, native).await;
  }

  async fn supervise(self: &Arc<Self>, executable: &Path, epoch: u64, cancel: CancellationToken, native: bool) {
    // Mode changes may race an old startup/shutdown. Never overlap owned children.
    let _owner = tokio::select! {
      biased;
      _ = cancel.cancelled() => return,
      guard = self.managed_lock.lock() => guard,
    };
    let configured_snapshots = {
      let state = self.state.lock().unwrap();
      if state.epoch != epoch || cancel.is_cancelled() {
        return;
      }
      match &state.connection {
        Some(crate::service_client::Connection::Embedded(service)) => Some(service.clone()),
        _ => None,
      }
    };
    let snapshots = match configured_snapshots
      .map(Ok)
      .unwrap_or_else(|| self.snapshot_service(native))
    {
      Ok(service) => service,
      Err(error) => {
        self.managed_phase(epoch, "failed", Some(error));
        return;
      }
    };
    if self.index.is_some() {
      // Snapshots belong to viewer-core and remain available even when its
      // advisory Relay feed is starting, retrying, or unavailable.
      let mut state = self.state.lock().unwrap();
      if state.epoch != epoch || cancel.is_cancelled() {
        return;
      }
      state.connection = Some(crate::service_client::Connection::Embedded(snapshots.clone()));
      state.active_endpoint = Some("embedded".into());
      state.native = native;
      state.providers = tokn_session_relay::PROVIDERS
        .into_iter()
        .filter_map(super::viewer_provider)
        .collect();
      self.ready.notify_all();
    }
    for attempt in 0..MAX_ATTEMPTS {
      if cancel.is_cancelled() {
        return;
      }
      self.managed_phase(epoch, if attempt == 0 { "starting" } else { "retrying" }, None);
      let error = match ManagedChild::spawn(executable) {
        Ok(mut child) => {
          let ready = tokio::select! {
            biased;
            _ = cancel.cancelled() => { child.stop().await; return; }
            result = tokio::time::timeout(START_TIMEOUT, child.ready()) => result.unwrap_or_else(|_| Err("Relay startup timed out".into())),
          };
          let error = match ready {
            Ok(()) => {
              if self.index.is_some() {
                self.managed_phase(epoch, "live", None);
              } else {
                self.connect_source(
                  crate::service_client::Connection::Embedded(snapshots.clone()),
                  "stdio".into(),
                  epoch,
                );
              }
              tokio::select! {
                biased;
                _ = cancel.cancelled() => { child.stop().await; return; }
                result = child.consume(&snapshots, &self) => {
                  result.err().unwrap_or_else(|| "Relay pipe stopped".into())
                },
              }
            }
            Err(error) => error,
          };
          child.stop().await;
          error
        }
        Err(error) => error,
      };
      self.managed_phase(
        epoch,
        if attempt + 1 == MAX_ATTEMPTS {
          "failed"
        } else {
          "retrying"
        },
        Some(error),
      );
      if attempt + 1 < MAX_ATTEMPTS {
        tokio::select! {
          _ = cancel.cancelled() => return,
          _ = tokio::time::sleep(Duration::from_secs(1 << attempt)) => {}
        }
      }
    }
  }

  fn managed_phase(&self, epoch: u64, phase: &str, error: Option<String>) {
    {
      let mut state = self.state.lock().unwrap();
      if state.epoch != epoch || state.cancel.is_cancelled() {
        return;
      }
      if self.index.is_none() {
        state.connection_cancel.cancel();
        state.active_endpoint = None;
        state.connection = None;
      }
      state.phase = phase.into();
      state.error = error;
    }
    self.ready.notify_all();
    self.notify(None, false);
  }

  pub async fn shutdown(&self) {
    {
      let mut state = self.state.lock().unwrap();
      state.epoch += 1;
      state.cancel.cancel();
      state.connection_cancel.cancel();
      state.active_endpoint = None;
      state.connection = None;
      state.sessions.clear();
    }
    self.ready.notify_all();
    // The supervisor releases this only after closing stdin and reaping its child.
    let _owner = self.managed_lock.lock().await;
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    model::ViewerProvider,
    relay::{RelayMode, RelaySettings},
  };

  #[test]
  fn lifetime_fixture() {
    if std::env::var_os("TOKN_RELAY_LIFETIME_FIXTURE").is_none() {
      return;
    }
    use std::io::Read;
    let mut byte = [0];
    while matches!(std::io::stdin().read(&mut byte), Ok(1)) {}
  }

  #[tokio::test]
  async fn monitoring_exit_keeps_the_lifetime_pipe_open_until_explicit_stop() {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
      .args(["--exact", "relay::managed::tests::lifetime_fixture"])
      .env("TOKN_RELAY_LIFETIME_FIXTURE", "1")
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::inherit())
      .kill_on_drop(true);
    let mut child = ManagedChild::spawn_command(&mut command).unwrap();
    assert!(child.child.stdin.is_none());
    assert!(child.lifetime.is_some());
    assert!(
      tokio::time::timeout(Duration::from_millis(200), child.child.wait())
        .await
        .is_err(),
      "waiting for exit must not cause EOF"
    );
    child.stop().await;
    assert!(child.child.try_wait().unwrap().unwrap().success());
  }

  #[tokio::test]
  async fn bounded_retries_fail_visibly_without_falling_back_to_local() {
    let manager = ViewerRelay::new();
    let cancel = {
      let mut state = manager.state.lock().unwrap();
      state.providers = vec![ViewerProvider::Codex];
      state.cancel.clone()
    };
    let directory = tempfile::tempdir().unwrap();
    tokio::time::timeout(
      Duration::from_secs(6),
      manager.supervise(&directory.path().join("missing-viewer"), 0, cancel, false),
    )
    .await
    .unwrap();
    assert_eq!(manager.status().phase, "failed");
    assert!(manager.status().error.unwrap().contains("Could not start"));
    assert!(manager.covers(ViewerProvider::Codex));
    assert!(manager.status().active_endpoint.is_none());
  }

  #[tokio::test]
  async fn indexed_snapshots_remain_readable_when_the_feed_cannot_start() {
    use crate::model::SessionLocator;
    use tokn_session_index::{SessionIndex, SessionKey, SessionMetadata, SourceReplacement, SourceState};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("session.jsonl");
    std::fs::write(
      &path,
      concat!(
        "{\"type\":\"session\",\"id\":\"saved\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"message\",\"id\":\"one\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n"
      ),
    )
    .unwrap();
    let index = Arc::new(SessionIndex::open_in_memory().unwrap());
    let source = crate::service::index_source_key_for_path(ViewerProvider::Pi, &path).unwrap();
    index
      .replace_sources(&[
        SourceReplacement::new(
          SourceState::new(source.clone(), "fixture", 0),
          vec![SessionMetadata::new(
            SessionKey::new("pi", &source.source_key, "saved"),
            path.to_str().unwrap(),
          )],
        ),
        SourceReplacement::new(
          SourceState::new(
            crate::service::index_catalog_source_key(ViewerProvider::Pi),
            "fixture",
            0,
          ),
          vec![],
        ),
      ])
      .unwrap();
    // Exercise public configuration while the supervisor cannot run. Snapshot
    // startup and a Local -> Automatic reconfiguration must both work already.
    let configured = ViewerRelay::with_index(Some(index.clone()));
    let owner = configured.managed_lock.lock().await;
    for native in [true, false] {
      configured
        .configure(RelaySettings {
          mode: RelayMode::Automatic,
          include_native: native,
          ..Default::default()
        })
        .unwrap();
      let loader = configured.clone();
      let target = SessionLocator {
        version: 1,
        provider: ViewerProvider::Pi,
        session_id: "saved".into(),
        source_path: path.clone(),
      };
      let locator = target.clone();
      let loaded = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::task::spawn_blocking(move || loader.load(&target)),
      )
      .await
      .expect("snapshots must not wait for the supervisor")
      .unwrap()
      .unwrap();
      assert!(!loaded.events.is_empty());
      assert_eq!(configured.native(&locator, 0, &loaded).is_some(), native);
      configured
        .configure(RelaySettings {
          mode: RelayMode::Local,
          ..Default::default()
        })
        .unwrap();
    }
    drop(owner);
    configured.shutdown().await;
    let manager = ViewerRelay::with_index(Some(index.clone()));
    let cancel = manager.state.lock().unwrap().cancel.clone();
    manager
      .supervise(&root.path().join("missing-viewer"), 0, cancel, true)
      .await;
    assert_eq!(manager.status().phase, "failed");
    let locator = SessionLocator {
      version: 1,
      provider: ViewerProvider::Pi,
      session_id: "saved".into(),
      source_path: path,
    };
    let loader = manager.clone();
    let target = locator.clone();
    let loaded = tokio::task::spawn_blocking(move || loader.load(&target))
      .await
      .unwrap()
      .unwrap();
    assert!(!loaded.events.is_empty());
    assert!(manager.native(&locator, 0, &loaded).is_some());
    // Metadata backfill reaches an already followed snapshot without reopening
    // or replaying its event history, even with no Relay records arriving.
    let mut updates = manager.changes.subscribe();
    let mut renamed = SessionMetadata::new(
      SessionKey::new("pi", &source.source_key, "saved"),
      locator.source_path.to_str().unwrap(),
    );
    renamed.title = Some("Renamed in the index".into());
    index
      .replace_sources(&[SourceReplacement::new(
        SourceState::new(source, "updated", 1),
        vec![renamed],
      )])
      .unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
      loop {
        updates.recv().await.unwrap();
        if manager.state.lock().unwrap().sessions[&locator]
          .loaded
          .as_ref()
          .unwrap()
          .reference
          .title
          .as_deref()
          == Some("Renamed in the index")
        {
          break;
        }
      }
    })
    .await
    .unwrap();
    let refreshed = manager.advance(&locator).unwrap();
    assert_eq!(refreshed.events.len(), loaded.events.len());
    let mut unknown = locator;
    unknown.session_id = "not-indexed".into();
    assert!(manager.load(&unknown).is_err());
    manager.shutdown().await;
  }

  #[tokio::test]
  async fn mode_change_cancels_pending_start_and_stale_status_cannot_override_local() {
    let manager = ViewerRelay::new();
    let cancel = manager.state.lock().unwrap().cancel.clone();
    let owner = manager.managed_lock.lock().await;
    let task_manager = manager.clone();
    let task = tokio::spawn(async move {
      task_manager
        .supervise(Path::new("missing-viewer"), 0, cancel, false)
        .await;
    });
    manager
      .configure(RelaySettings {
        mode: RelayMode::Local,
        ..Default::default()
      })
      .unwrap();
    tokio::time::timeout(Duration::from_secs(1), task)
      .await
      .unwrap()
      .unwrap();
    manager.managed_phase(0, "failed", Some("obsolete failure".into()));
    manager.connect_endpoint("tcp://127.0.0.1:1".into(), 0);
    assert_eq!(manager.status().phase, "local");
    assert!(manager.status().error.is_none());
    assert!(manager.status().active_endpoint.is_none());
    drop(owner);
    manager.shutdown().await;
  }

  #[tokio::test]
  async fn new_child_port_resumes_cached_sessions_without_mixing_native_snapshots() {
    use crate::model::SessionLocator;
    use crate::{service_client::load_catalog, service_server::serve_listener};
    use tokn_session_core::Provider;
    use tokn_session_relay::{ProviderRoot, RelayConfig};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("session.jsonl");
    let initial = "{\"type\":\"session\",\"id\":\"restart\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}\n{\"type\":\"message\",\"id\":\"one\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n";
    std::fs::write(&path, initial).unwrap();
    let mut config = RelayConfig::new(vec![ProviderRoot::new(Provider::Pi, root.path().into())]);
    config.include_native = true;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("tcp://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(serve_listener(listener, config.clone()));
    let manager = ViewerRelay::new();
    manager.state.lock().unwrap().providers = vec![ViewerProvider::Pi];
    let mut changes = manager.changes.subscribe();
    manager.connect_endpoint(endpoint, 0);
    tokio::time::timeout(Duration::from_secs(4), async {
      while !manager.has_catalog() {
        changes.recv().await.unwrap();
      }
    })
    .await
    .unwrap();
    let locator = SessionLocator {
      version: 1,
      provider: ViewerProvider::Pi,
      session_id: "restart".into(),
      source_path: path.clone(),
    };
    let loader = manager.clone();
    let target = locator.clone();
    let pinned = tokio::task::spawn_blocking(move || loader.load(&target))
      .await
      .unwrap()
      .unwrap();
    server.abort();
    let _ = server.await;
    manager.managed_phase(0, "retrying", Some("child exited".into()));
    assert!(manager.status().active_endpoint.is_none());
    assert!(manager.covers(ViewerProvider::Pi));
    assert!(Arc::ptr_eq(&pinned, &manager.load(&locator).unwrap()));
    assert!(manager.native(&locator, 0, &pinned).is_some());

    std::fs::write(
      &path,
      format!(
        "{initial}{{\"type\":\"message\",\"id\":\"two\",\"message\":{{\"role\":\"user\",\"content\":\"again\"}}}}\n"
      ),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("tcp://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(serve_listener(listener, config));
    manager.connect_endpoint(endpoint.clone(), 0);
    tokio::time::timeout(Duration::from_secs(4), async {
      loop {
        let count = manager.state.lock().unwrap().sessions[&locator]
          .loaded
          .as_ref()
          .unwrap()
          .events
          .len();
        if count > pinned.events.len() {
          break;
        }
        changes.recv().await.unwrap();
      }
    })
    .await
    .unwrap();
    assert!(Arc::ptr_eq(&pinned, &manager.load(&locator).unwrap()));
    let latest = manager.advance(&locator).unwrap();
    assert_eq!(latest.events.len(), pinned.events.len() + 1);
    assert!(manager.native(&locator, 0, &pinned).is_none());
    assert!(manager.native(&locator, 0, &latest).is_some());
    manager
      .configure(RelaySettings {
        mode: RelayMode::Local,
        ..Default::default()
      })
      .unwrap();
    assert!(!manager.covers(ViewerProvider::Pi));
    assert!(manager.state.lock().unwrap().sessions.is_empty());
    manager.shutdown().await;
    assert!(
      load_catalog(&endpoint).await.is_ok(),
      "viewer shutdown never stops an external service"
    );
    server.abort();
  }
}
