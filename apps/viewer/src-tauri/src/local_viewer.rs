//! Local session readers are optional until a local command is requested.
use std::{future::Future, path::PathBuf, sync::Mutex};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{OnceCell, RwLock, broadcast};
use tokn_viewer_core::{
  ViewerService, relay,
  runtime::{ViewerEvent, ViewerRuntime, session_index_path},
};

#[derive(Default)]
pub struct LocalViewer {
  initialized: OnceCell<StartedViewer>,
  // Readers may initialize concurrently; shutdown waits for the initializer
  // and prevents a late command from starting workers after exit begins.
  stopped: RwLock<bool>,
}

impl LocalViewer {
  pub async fn service(&self, app: &AppHandle) -> Result<ViewerService, String> {
    self.get_or_start(|| start(app.clone())).await
  }

  async fn get_or_start<F, Fut>(&self, initialize: F) -> Result<ViewerService, String>
  where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<StartedViewer, String>>,
  {
    let stopped = self.stopped.read().await;
    if *stopped {
      return Err("Local session services are shutting down".into());
    }
    let started = self
      .initialized
      .get_or_try_init(initialize)
      .await
      .map_err(|error| format!("Local sessions could not start: {error}"))?;
    Ok(started.service.clone())
  }

  pub async fn api(&self, app: &AppHandle) -> Result<(ViewerService, broadcast::Sender<ViewerEvent>), String> {
    let service = self.service(app).await?;
    let started = self.initialized.get().ok_or("Local viewer did not initialize")?;
    Ok((service, started.events.clone()))
  }

  pub async fn shutdown(&self) {
    let mut stopped = self.stopped.write().await;
    if *stopped {
      return;
    }
    *stopped = true;
    if let Some(started) = self.initialized.get() {
      started.shutdown().await;
    }
  }
}

struct StartedViewer {
  events: broadcast::Sender<ViewerEvent>,
  service: ViewerService,
  background: Mutex<Option<Background>>,
}

struct Background {
  runtime: Option<ViewerRuntime>,
  forwarding: tokio::task::JoinHandle<()>,
}

impl Drop for Background {
  fn drop(&mut self) {
    self.forwarding.abort();
    // ViewerRuntime's drop aborts its owned workers.
  }
}

impl StartedViewer {
  async fn shutdown(&self) {
    let background = self.background.lock().unwrap_or_else(|error| error.into_inner()).take();
    if let Some(mut background) = background {
      background.forwarding.abort();
      background.runtime.take();
      let _ = (&mut background.forwarding).await;
    }
    self.service.relay.shutdown().await;
  }
}

fn prepare(
  index_path: PathBuf,
  settings_path: Result<PathBuf, String>,
) -> Result<(ViewerService, Result<relay::RelaySettings, String>), String> {
  if let Some(parent) = index_path.parent() {
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
  }
  let service = ViewerService::native(index_path)?;
  let settings = settings_path.and_then(|path| relay::read_settings(&path));
  Ok((service, settings))
}

async fn start(app: AppHandle) -> Result<StartedViewer, String> {
  let settings_path = app
    .path()
    .app_config_dir()
    .map(|path| path.join("relay.json"))
    .map_err(|error| error.to_string());
  let (service, settings) = tokio::task::spawn_blocking(move || prepare(session_index_path()?, settings_path))
    .await
    .map_err(|error| format!("Local session initialization task failed: {error}"))??;
  // Configure before starting index workers, so saved External settings cannot
  // briefly trigger native discovery while the local view is opening.
  match settings {
    Ok(settings) => {
      if let Err(error) = service.relay.configure(settings) {
        service.relay.configuration_failed(error);
      }
    }
    Err(error) => service.relay.configuration_failed(error),
  }
  let runtime = ViewerRuntime::start(service.clone());
  let forwarding = tokio::spawn(forward_events(runtime.events.subscribe(), app));
  Ok(StartedViewer {
    events: runtime.events.clone(),
    service,
    background: Mutex::new(Some(Background {
      runtime: Some(runtime),
      forwarding,
    })),
  })
}

async fn forward_events(mut events: broadcast::Receiver<ViewerEvent>, app: AppHandle) {
  loop {
    match events.recv().await {
      Ok(event) => {
        let _ = app.emit(&event.event, event.payload);
      }
      Err(broadcast::error::RecvError::Lagged(_)) => {
        let _ = app.emit(
          "relay-changed",
          relay::RelayChange {
            session_key: None,
            reset: true,
          },
        );
        let _ = app.emit(
          "session-index-changed",
          serde_json::json!({"attention_session_keys":[],"updated_session_keys":[]}),
        );
      }
      Err(broadcast::error::RecvError::Closed) => return,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
  };
  use tokio::sync::{Barrier, Notify};

  async fn test_start(index_path: PathBuf) -> Result<StartedViewer, String> {
    let settings_path = index_path.with_extension("settings.json");
    let (service, _) = tokio::task::spawn_blocking(move || prepare(index_path, Ok(settings_path)))
      .await
      .map_err(|error| error.to_string())??;
    // Exercise real SQLite creation without installing watchers on the test
    // runner's own provider history. The initializer owns the runtime in use.
    Ok(StartedViewer {
      events: broadcast::channel(16).0,
      service,
      background: Mutex::new(None),
    })
  }

  #[tokio::test]
  async fn unused_local_state_shutdown_does_not_initialize_or_open_an_index() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("local/index.sqlite");
    let local = LocalViewer::default();
    assert!(local.initialized.get().is_none());
    local.shutdown().await;
    let calls = AtomicUsize::new(0);
    let result = local
      .get_or_start(|| {
        calls.fetch_add(1, Ordering::SeqCst);
        test_start(path.clone())
      })
      .await;
    assert!(matches!(result, Err(error) if error.contains("shutting down")));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!path.exists());
  }

  #[tokio::test]
  async fn concurrent_local_commands_initialize_one_service() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.sqlite");
    let local = Arc::new(LocalViewer::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(Barrier::new(16));
    let mut tasks = Vec::new();
    for _ in 0..16 {
      let (local, calls, ready, path) = (local.clone(), calls.clone(), ready.clone(), path.clone());
      tasks.push(tokio::spawn(async move {
        ready.wait().await;
        local
          .get_or_start(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            test_start(path)
          })
          .await
          .unwrap()
      }));
    }
    let expected = tasks.remove(0).await.unwrap();
    for task in tasks {
      assert!(Arc::ptr_eq(&expected.relay, &task.await.unwrap().relay));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(path.exists());
    local.shutdown().await;
  }

  #[tokio::test]
  async fn failed_local_file_initialization_can_retry_after_repair() {
    let directory = tempfile::tempdir().unwrap();
    let blocked = directory.path().join("blocked");
    std::fs::write(&blocked, "a file cannot contain the local index").unwrap();
    let path = blocked.join("index.sqlite");
    let local = LocalViewer::default();
    let failed = local.get_or_start(|| test_start(path.clone())).await;
    assert!(matches!(failed, Err(error) if error.contains("Local sessions could not start")));
    assert!(local.initialized.get().is_none());
    std::fs::remove_file(&blocked).unwrap();
    local.get_or_start(|| test_start(path.clone())).await.unwrap();
    assert!(path.exists());
    assert!(local.initialized.get().is_some());
    local.shutdown().await;
  }

  #[tokio::test]
  async fn canceled_initialization_releases_the_cell_and_allows_retry() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.sqlite");
    let local = Arc::new(LocalViewer::default());
    let entered = Arc::new(Notify::new());
    let initializing = {
      let (local, entered) = (local.clone(), entered.clone());
      tokio::spawn(async move {
        local
          .get_or_start(|| async {
            entered.notify_one();
            std::future::pending::<Result<StartedViewer, String>>().await
          })
          .await
      })
    };
    entered.notified().await;
    initializing.abort();
    assert!(matches!(initializing.await, Err(error) if error.is_cancelled()));
    local.get_or_start(|| test_start(path.clone())).await.unwrap();
    assert!(path.exists());
    local.shutdown().await;
  }

  #[tokio::test]
  async fn shutdown_waits_for_in_flight_initialization_and_stops_its_workers() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.sqlite");
    let local = Arc::new(LocalViewer::default());
    let entered = Arc::new(Notify::new());
    let finish = Arc::new(Notify::new());
    let worker_stopped = Arc::new(AtomicUsize::new(0));
    struct Stopped(Arc<AtomicUsize>);
    impl Drop for Stopped {
      fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
      }
    }
    let initializing = {
      let (local, entered, finish, worker_stopped) =
        (local.clone(), entered.clone(), finish.clone(), worker_stopped.clone());
      tokio::spawn(async move {
        local
          .get_or_start(|| async {
            entered.notify_one();
            finish.notified().await;
            let started = test_start(path).await?;
            started.service.relay.configure(relay::RelaySettings {
              mode: relay::RelayMode::Local,
              ..Default::default()
            })?;
            let worker_started = Arc::new(Notify::new());
            let ready = worker_started.clone();
            let forwarding = tokio::spawn(async move {
              let _stopped = Stopped(worker_stopped);
              ready.notify_one();
              std::future::pending::<()>().await;
            });
            *started.background.lock().unwrap() = Some(Background {
              runtime: None,
              forwarding,
            });
            worker_started.notified().await;
            Ok(started)
          })
          .await
          .unwrap()
      })
    };
    entered.notified().await;
    let stopping = {
      let local = local.clone();
      tokio::spawn(async move { local.shutdown().await })
    };
    tokio::task::yield_now().await;
    assert!(!stopping.is_finished());
    finish.notify_one();
    let service = initializing.await.unwrap();
    stopping.await.unwrap();
    assert_eq!(worker_stopped.load(Ordering::SeqCst), 1);
    assert_eq!(service.relay.status().active_endpoint, None);
    local.shutdown().await;
    assert_eq!(worker_stopped.load(Ordering::SeqCst), 1);
  }
}
