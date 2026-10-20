//! Native-global inventory and bounded, cancellation-safe restart receipts.
use std::{
  collections::HashMap,
  future::Future,
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  },
  time::{SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::watch;
use tokn_viewer_core::runtimes::{
  RuntimeExecutionKind, RuntimeInfo, RuntimeInventory, RuntimeKind, RuntimeOwnership, RuntimeRestartRequest,
  RuntimeRestartResponse,
};

const MAX_RECEIPTS: usize = 256;
const MAX_PENDING: usize = 8;
type RestartResult = Result<RuntimeRestartResponse, String>;

struct Receipt {
  runtime_id: String,
  expected_instance_id: String,
  result: watch::Sender<Option<RestartResult>>,
}

#[derive(Clone, Default)]
pub struct RuntimeRestartBroker {
  receipts: Arc<Mutex<HashMap<String, Arc<Receipt>>>>,
  closed: Arc<AtomicBool>,
}

impl RuntimeRestartBroker {
  pub fn close(&self) {
    self.closed.store(true, Ordering::Release);
  }

  async fn restart<F, Fut>(&self, request: RuntimeRestartRequest, action: F) -> RestartResult
  where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), String>> + Send + 'static,
  {
    validate_request(&request)?;
    let mut receive = {
      let mut receipts = self.receipts.lock().unwrap();
      if self.closed.load(Ordering::Acquire) {
        return Err("Runtime management is shutting down".into());
      }
      if let Some(receipt) = receipts.get(&request.request_id) {
        if receipt.runtime_id != request.runtime_id || receipt.expected_instance_id != request.expected_instance_id {
          return Err("Restart request ID was already used for a different runtime instance".into());
        }
        receipt.result.subscribe()
      } else {
        if receipts.len() >= MAX_RECEIPTS {
          return Err(
            "Restart receipt limit reached for this viewer; relaunch the viewer before another restart".into(),
          );
        }
        if receipts
          .values()
          .filter(|receipt| receipt.result.borrow().is_none())
          .count()
          >= MAX_PENDING
        {
          return Err("Too many runtime restarts are already pending".into());
        }
        let (result, receive) = watch::channel(None);
        receipts.insert(
          request.request_id.clone(),
          Arc::new(Receipt {
            runtime_id: request.runtime_id.clone(),
            expected_instance_id: request.expected_instance_id.clone(),
            result: result.clone(),
          }),
        );
        // The operation belongs to the app, not the requesting WebView future.
        // A canceled invoke can retry the same ID without restarting twice.
        let closed = self.closed.clone();
        tokio::spawn(async move {
          let outcome = tokio::spawn(async move {
            if closed.load(Ordering::Acquire) {
              return Err("Runtime management is shutting down".into());
            }
            action().await
          })
          .await
          .map_err(|error| format!("Runtime restart task failed: {error}"))
          .and_then(|result| result)
          .map(|_| RuntimeRestartResponse {
            request_id: request.request_id,
            runtime_id: request.runtime_id,
            message: "Restart requested".into(),
          });
          result.send_replace(Some(outcome));
        });
        receive
      }
    };
    loop {
      if let Some(result) = receive.borrow().clone() {
        return result;
      }
      receive
        .changed()
        .await
        .map_err(|_| "Runtime restart receipt disappeared".to_string())?;
    }
  }
}

fn validate_request(request: &RuntimeRestartRequest) -> Result<(), String> {
  let id = uuid::Uuid::parse_str(&request.request_id).map_err(|_| "Restart request ID must be a UUID")?;
  if id.to_string() != request.request_id {
    return Err("Restart request ID must be a canonical UUID".into());
  }
  if request.runtime_id.is_empty()
    || request.runtime_id.len() > 128
    || request.expected_instance_id.is_empty()
    || request.expected_instance_id.len() > 256
    || request.runtime_id.chars().any(char::is_control)
    || request.expected_instance_id.chars().any(char::is_control)
  {
    return Err("Invalid runtime restart identity".into());
  }
  Ok(())
}

pub fn changed(app: &AppHandle) {
  let _ = app.emit("viewer-runtimes-changed", serde_json::json!({}));
}

pub fn now_ms() -> i64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
    .unwrap_or(0)
}

/// Only the public origin is inventory metadata. Never forward userinfo,
/// query parameters, fragments, or credential-bearing route components.
pub fn public_endpoint(value: &str) -> Option<String> {
  let url = url::Url::parse(value).ok()?;
  matches!(url.scheme(), "http" | "https" | "ws" | "wss").then(|| url.origin().ascii_serialization())
}

pub async fn inventory(app: &AppHandle) -> RuntimeInventory {
  let mut runtimes = match app.state::<crate::local_viewer::LocalViewer>().existing() {
    Some(service) => service.list_runtimes().await,
    None => vec![uninitialized_reader()],
  };
  runtimes.extend(app.state::<crate::local_host::LocalHost>().runtime_inventory().await);
  for connection in app
    .state::<crate::native_remote::NativeRemote>()
    .list_connections()
    .await
  {
    runtimes.push(RuntimeInfo {
      runtime_id: format!("hub:{}", connection.connection_id),
      instance_id: connection.connection_id,
      name: "Hub machine connection".into(),
      kind: RuntimeKind::HubConnection,
      execution_kind: RuntimeExecutionKind::External,
      ownership: RuntimeOwnership::External,
      status: "open".into(),
      pid: None,
      started_at_ms: None,
      session_key: None,
      session_id: None,
      project_id: None,
      working_directory: None,
      endpoint: public_endpoint(&connection.endpoint),
      detail: Some(format!(
        "Registered machine {}; {} carrier. Manage the connection from Machines.",
        connection.host_id,
        match connection.transport.kind {
          tokn_hub_remote::TransportKind::Direct => "direct",
          tokn_hub_remote::TransportKind::Relay => "encrypted relay",
        }
      )),
      last_error: connection.transport.reason,
      can_restart: false,
      restart_disabled_reason: Some("Remote processes are not owned by this viewer".into()),
    });
  }
  RuntimeInventory { version: 1, runtimes }
}

fn uninitialized_reader() -> RuntimeInfo {
  RuntimeInfo {
    runtime_id: "reader".into(),
    instance_id: "reader:uninitialized".into(),
    name: "Session reader / Relay".into(),
    kind: RuntimeKind::Reader,
    execution_kind: RuntimeExecutionKind::Embedded,
    ownership: RuntimeOwnership::Viewer,
    status: "stopped".into(),
    pid: None,
    started_at_ms: None,
    session_key: None,
    session_id: None,
    project_id: None,
    working_directory: None,
    endpoint: None,
    detail: Some("Local session services have not been initialized".into()),
    last_error: None,
    can_restart: false,
    restart_disabled_reason: Some("Open the local machine to start its session reader".into()),
  }
}

pub async fn restart(app: AppHandle, request: RuntimeRestartRequest) -> RestartResult {
  let broker = app.state::<RuntimeRestartBroker>().inner().clone();
  let action_request = request.clone();
  let action_app = app.clone();
  broker
    .restart(request, move || async move {
      changed(&action_app);
      let result = if action_request.runtime_id == "hosting" {
        action_app
          .state::<crate::local_host::LocalHost>()
          .restart(action_app.clone(), &action_request.expected_instance_id)
          .await
      } else if action_request.runtime_id == "reader" || action_request.runtime_id.starts_with("pi:") {
        match action_app.state::<crate::local_viewer::LocalViewer>().existing() {
          Some(service) => service.restart_runtime(action_request).await,
          None => Err("Local session services have not been initialized".into()),
        }
      } else {
        Err("This runtime cannot be restarted by the viewer".into())
      };
      changed(&action_app);
      result
    })
    .await
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use tokio::sync::Notify;

  fn request() -> RuntimeRestartRequest {
    RuntimeRestartRequest {
      request_id: uuid::Uuid::new_v4().to_string(),
      runtime_id: "reader".into(),
      expected_instance_id: "reader:1".into(),
    }
  }

  #[test]
  fn inventory_endpoints_never_contain_credentials_or_routes() {
    assert_eq!(
      public_endpoint("https://user:secret@hub.example/private?token=secret#secret"),
      Some("https://hub.example".into())
    );
    assert!(public_endpoint("file:///private/token").is_none());
    let reader = uninitialized_reader();
    assert!(!reader.can_restart);
    assert!(reader.pid.is_none());
  }

  #[tokio::test]
  async fn canceled_restart_waiter_retries_one_app_owned_action() {
    let broker = RuntimeRestartBroker::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let request = request();
    let waiting = {
      let (broker, request, calls, entered, release) = (
        broker.clone(),
        request.clone(),
        calls.clone(),
        entered.clone(),
        release.clone(),
      );
      tokio::spawn(async move {
        broker
          .restart(request, move || async move {
            calls.fetch_add(1, Ordering::SeqCst);
            entered.notify_one();
            release.notified().await;
            Ok(())
          })
          .await
      })
    };
    entered.notified().await;
    waiting.abort();
    release.notify_one();
    let response = broker
      .restart(request.clone(), || async {
        panic!("duplicate restart");
      })
      .await
      .unwrap();
    assert_eq!(response.request_id, request.request_id);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let mut conflict = request.clone();
    conflict.expected_instance_id = "reader:2".into();
    assert!(
      broker
        .restart(conflict, || async { Ok(()) })
        .await
        .unwrap_err()
        .contains("different runtime instance")
    );
  }

  #[tokio::test]
  async fn failed_and_panicked_restarts_are_stable_receipts() {
    let broker = RuntimeRestartBroker::default();
    let request = request();
    assert_eq!(
      broker
        .restart(request.clone(), || async { Err("stale instance".into()) })
        .await
        .unwrap_err(),
      "stale instance"
    );
    assert_eq!(
      broker.restart(request, || async { Ok(()) }).await.unwrap_err(),
      "stale instance"
    );
    assert!(
      broker
        .restart(self::request(), || async {
          panic!("fake failure");
        })
        .await
        .unwrap_err()
        .contains("restart task failed")
    );
  }

  #[tokio::test]
  async fn invalid_requests_do_not_consume_receipts() {
    let broker = RuntimeRestartBroker::default();
    let mut request = request();
    request.request_id = "invalid".into();
    assert!(broker.restart(request, || async { Ok(()) }).await.is_err());
    assert!(broker.receipts.lock().unwrap().is_empty());
  }

  #[tokio::test]
  async fn closing_admission_does_not_start_another_runtime() {
    let broker = RuntimeRestartBroker::default();
    broker.close();
    assert!(
      broker
        .restart(request(), || async {
          panic!("must not start while closing");
        })
        .await
        .unwrap_err()
        .contains("shutting down")
    );
    assert!(broker.receipts.lock().unwrap().is_empty());
  }

  #[tokio::test]
  async fn pending_and_completed_receipts_have_independent_bounds() {
    let broker = RuntimeRestartBroker::default();
    for _ in 0..MAX_PENDING {
      let request = request();
      broker.receipts.lock().unwrap().insert(
        request.request_id,
        Arc::new(Receipt {
          runtime_id: request.runtime_id,
          expected_instance_id: request.expected_instance_id,
          result: watch::channel(None).0,
        }),
      );
    }
    assert!(
      broker
        .restart(request(), || async {
          panic!("pending limit must reject");
        })
        .await
        .unwrap_err()
        .contains("already pending")
    );
    for receipt in broker.receipts.lock().unwrap().values() {
      receipt.result.send_replace(Some(Err("completed".into())));
    }
    assert!(broker.restart(request(), || async { Ok(()) }).await.is_ok());
    for _ in MAX_PENDING + 1..MAX_RECEIPTS {
      let request = request();
      broker.receipts.lock().unwrap().insert(
        request.request_id,
        Arc::new(Receipt {
          runtime_id: request.runtime_id,
          expected_instance_id: request.expected_instance_id,
          result: watch::channel(Some(Err("completed".into()))).0,
        }),
      );
    }
    assert!(
      broker
        .restart(request(), || async {
          panic!("receipt limit must reject");
        })
        .await
        .unwrap_err()
        .contains("receipt limit")
    );
  }
}
