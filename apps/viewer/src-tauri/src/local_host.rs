//! App-owned hosting reuses local trust, never installs a background service.
use crate::local_viewer::LocalViewer;
use base64::{Engine, engine::general_purpose::STANDARD};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::{
  future::Future,
  path::PathBuf,
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  },
  time::Duration,
};
use tauri::{AppHandle, Emitter, Manager};
use tokio::{sync::Mutex as AsyncMutex, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tokn_session_hub::{
  connector::{self, ConnectorStatus, HostLease},
  host_setup::{self, HostOptions},
  onboarding::HostProfile,
  pairing::TOTP_PERIOD,
  secure::NoiseIdentity,
};
use tokn_viewer_core::runtimes::{RuntimeExecutionKind, RuntimeInfo, RuntimeKind, RuntimeOwnership};
use zeroize::Zeroize;

const HOST_HANDOFF_TIMEOUT: Duration = Duration::from_secs(5);
const HOST_HANDOFF_POLL: Duration = Duration::from_millis(100);

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostPhase {
  #[default]
  Stopped,
  External,
  Background,
  Unavailable,
  Connecting,
  Online,
  Reconnecting,
  Error,
}
#[derive(Clone, Serialize)]
pub struct HostStatus {
  pub phase: HostPhase,
  pub hub_url: String,
  pub name: String,
  pub allow_control: bool,
  pub machine_reference: Option<String>,
  pub error: Option<String>,
  pub external_stop_supported: bool,
  pub external_remove_supported: bool,
  pub background_services: crate::external_host::BackgroundHosting,
}
impl Default for HostStatus {
  fn default() -> Self {
    Self {
      phase: HostPhase::Stopped,
      hub_url: String::new(),
      name: "This computer".into(),
      allow_control: false,
      machine_reference: None,
      error: None,
      external_stop_supported: false,
      external_remove_supported: false,
      background_services: Default::default(),
    }
  }
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartHostRequest {
  pub hub_url: String,
  pub name: String,
  pub allow_control: bool,
}
#[derive(Serialize)]
pub struct HostPairing {
  pub machine_reference: String,
  pub qr_data_url: String,
  pub setup_uri: String,
  pub current_code: String,
  pub host_time: u64,
  pub expires_at: u64,
}
impl Drop for HostPairing {
  fn drop(&mut self) {
    self.qr_data_url.zeroize();
    self.setup_uri.zeroize();
    self.current_code.zeroize();
  }
}
struct RunningHost {
  shutdown: CancellationToken,
  task: JoinHandle<()>,
  instance_id: String,
  started_at_ms: i64,
  configuration: Option<StartHostRequest>,
}
impl RunningHost {
  async fn stop(mut self) {
    self.shutdown.cancel();
    if tokio::time::timeout(std::time::Duration::from_secs(3), &mut self.task)
      .await
      .is_err()
    {
      self.task.abort();
      let _ = (&mut self.task).await;
    }
  }
}
impl Drop for RunningHost {
  fn drop(&mut self) {
    self.shutdown.cancel();
    self.task.abort();
  }
}

pub struct LocalHost {
  state_dir: PathBuf,
  paths: Option<tokn_session_storage::SessionPaths>,
  operation: AsyncMutex<Option<RunningHost>>,
  status: Arc<Mutex<HostStatus>>,
  closed: AtomicBool,
  closing: CancellationToken,
}
impl LocalHost {
  pub fn new(state_dir: PathBuf) -> Self {
    Self {
      state_dir,
      paths: None,
      operation: AsyncMutex::new(None),
      status: Arc::new(Mutex::new(HostStatus::default())),
      closed: AtomicBool::new(false),
      closing: CancellationToken::new(),
    }
  }
  pub fn unified(paths: tokn_session_storage::SessionPaths) -> Self {
    Self {
      paths: Some(paths.clone()),
      ..Self::new(paths.hub_dir())
    }
  }
  async fn storage_path(&self, writable: bool) -> Result<PathBuf, String> {
    let Some(paths) = self.paths.clone() else {
      return Ok(self.state_dir.clone());
    };
    tokio::task::spawn_blocking(move || {
      if writable {
        tokn_session_hub::storage::default_state_dir_for_paths(&paths)
      } else {
        tokn_session_hub::storage::inspect_state_dir_for_paths(&paths)
      }
    })
    .await
    .map_err(|_| "Host storage resolution task failed")?
  }
  pub async fn status(&self) -> Result<HostStatus, String> {
    let running = self.operation.lock().await;
    if running.is_some() {
      return Ok(self.status.lock().unwrap().clone());
    }
    detected_status(self.storage_path(false).await?).await
  }

  pub async fn start(&self, app: AppHandle, request: StartHostRequest) -> Result<HostStatus, String> {
    let mut running = self.operation.lock().await;
    self.start_locked(app, request, &mut running).await
  }

  async fn start_locked(
    &self,
    app: AppHandle,
    request: StartHostRequest,
    running: &mut Option<RunningHost>,
  ) -> Result<HostStatus, String> {
    if self.closed.load(Ordering::Acquire) {
      return Err("App hosting is shutting down".into());
    }
    if running.is_some() {
      return Err("Stop app hosting before changing its settings".into());
    }
    let configuration = request.clone();
    let state_dir = self.storage_path(true).await?;
    let path = state_dir.clone();
    let (lease, previous) = tokio::task::spawn_blocking(move || {
      let lease = HostLease::acquire(&path.join("host-enrollment.key"))?;
      let previous = HostProfile::load(&path.join("host.json"))?;
      Ok::<_, String>((lease, previous))
    })
    .await
    .map_err(|_| "Host ownership task failed")??;
    let background = crate::external_host::inspect(&state_dir, previous.as_ref()).await?;
    if background.has_loaded() || background.api.error.is_some() || background.connector.error.is_some() {
      return Err("Stop background hosting services, or resolve their status errors, before hosting in the app".into());
    }
    if let Some(profile) = &previous {
      if host_setup::existing_host_online(profile).await? {
        return Err("Another connector is already hosting this machine through its saved Hub. Stop that connector before hosting in the app.".into());
      }
    }
    let (service, events) = app.state::<LocalViewer>().api(&app).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
      .await
      .map_err(|e| e.to_string())?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    let mut random = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut random);
    let token = STANDARD.encode(random);
    let path = state_dir;
    let config_token = token.clone();
    let prepared = tokio::task::spawn_blocking(move || {
      host_setup::prepare_host(HostOptions {
        hub: Some(url::Url::parse(&request.hub_url).map_err(|_| "Enter a valid HTTPS Hub address")?),
        name: Some(request.name),
        viewer_url: None,
        passkey_origin: None,
        state_dir: path,
        totp_secret_file: None,
        viewer_token: Some(config_token),
        allow_control: Some(request.allow_control),
        insecure_loopback: false,
        ice_servers: None,
      })
    })
    .await
    .map_err(|_| "Host setup task failed")??;
    if self.closed.load(Ordering::Acquire) {
      return Err("App hosting is shutting down".into());
    }
    let mut config = prepared.config;
    // The persisted CLI target stays intact. The app serves its own existing
    // reader on an ephemeral, token-protected loopback port only while hosting.
    config.local_url = url::Url::parse(&format!("http://{address}")).map_err(|e| e.to_string())?;
    let status = HostStatus {
      phase: HostPhase::Connecting,
      hub_url: config.hub_url.origin().ascii_serialization(),
      name: config.name.clone(),
      allow_control: config.allow_control,
      machine_reference: Some(prepared.reference),
      error: None,
      external_stop_supported: false,
      external_remove_supported: false,
      background_services: Default::default(),
    };
    publish(&app, &self.status, status);
    let shutdown = CancellationToken::new();
    let router = tokn_viewer_api::router(service, events, Some(token), Vec::new(), shutdown.clone());
    let callback_app = app.clone();
    let callback_state = self.status.clone();
    let callback_shutdown = shutdown.clone();
    let sink = Arc::new(move |connection| {
      if callback_shutdown.is_cancelled() {
        return;
      }
      let mut status = callback_state.lock().unwrap().clone();
      (status.phase, status.error) = match connection {
        ConnectorStatus::Connecting => (HostPhase::Connecting, None),
        ConnectorStatus::Online => (HostPhase::Online, None),
        ConnectorStatus::Reconnecting { error } => (HostPhase::Reconnecting, Some(error)),
      };
      publish(&callback_app, &callback_state, status);
    });
    let task_shutdown = shutdown.clone();
    let state = self.status.clone();
    let task_app = app.clone();
    let task = tokio::spawn(async move {
      let stop = task_shutdown.clone();
      let api = axum::serve(listener, router).with_graceful_shutdown(async move { stop.cancelled().await });
      let result = tokio::select! {
        result = api => result.map_err(|e| e.to_string()),
        result = connector::run_owned(config, task_shutdown.clone(), lease, Some(sink)) => result,
      };
      let stopped = task_shutdown.is_cancelled();
      task_shutdown.cancel();
      if !stopped {
        let mut status = state.lock().unwrap().clone();
        status.phase = HostPhase::Error;
        status.error = Some(
          result
            .err()
            .unwrap_or_else(|| "Host service stopped unexpectedly".into()),
        );
        publish(&task_app, &state, status);
      }
    });
    *running = Some(RunningHost {
      shutdown,
      task,
      instance_id: uuid::Uuid::new_v4().to_string(),
      started_at_ms: crate::runtime_management::now_ms(),
      configuration: Some(configuration),
    });
    crate::runtime_management::changed(&app);
    Ok(self.status.lock().unwrap().clone())
  }

  /// Restart only this app's exact current instance. The saved enrollment,
  /// authorization, and profile are reused by the normal checked start path.
  pub async fn restart(&self, app: AppHandle, expected_instance_id: &str) -> Result<(), String> {
    let mut running = self.operation.lock().await;
    if self.closed.load(Ordering::Acquire) {
      return Err("App hosting is shutting down".into());
    }
    let host = running.as_ref().ok_or("Hosting is not owned by this viewer")?;
    let request = restart_configuration(host, expected_instance_id)?;
    // Capture the prior identity while its owned task and lease still exist.
    // Local task completion alone does not acknowledge remote deregistration.
    let path = self.storage_path(false).await?.join("host.json");
    let profile = tokio::task::spawn_blocking(move || HostProfile::load(&path))
      .await
      .map_err(|_| "Host configuration task failed")??
      .ok_or("The owned hosting configuration is unavailable")?;
    running.take().unwrap().stop().await;
    let result = async {
      wait_for_host_offline(
        || host_setup::existing_host_online(&profile),
        &self.closing,
        HOST_HANDOFF_TIMEOUT,
        HOST_HANDOFF_POLL,
      )
      .await?;
      self.start_locked(app.clone(), request, &mut running).await
    }
    .await;
    if let Err(error) = &result {
      let mut status = self.status.lock().unwrap().clone();
      status.phase = HostPhase::Error;
      status.error = Some(error.clone());
      publish(&app, &self.status, status);
    }
    result.map(|_| ())
  }

  pub async fn runtime_inventory(&self) -> Vec<RuntimeInfo> {
    let running = self.operation.lock().await;
    if let Some(host) = running.as_ref() {
      return vec![hosting_runtime(self.status.lock().unwrap().clone(), Some(host))];
    }
    let observed = match self.storage_path(false).await {
      Ok(path) => detected_status(path).await,
      Err(error) => Err(error),
    };
    let status = observed.unwrap_or_else(|error| HostStatus {
      phase: HostPhase::Unavailable,
      error: Some(error),
      ..Default::default()
    });
    let mut runtimes = vec![hosting_runtime(status.clone(), None)];
    for (id, name, kind, service) in [
      (
        "background-api",
        "Background viewer API",
        RuntimeKind::BackgroundApi,
        &status.background_services.api,
      ),
      (
        "background-connector",
        "Background Hub connector",
        RuntimeKind::BackgroundConnector,
        &status.background_services.connector,
      ),
    ] {
      if service.registered || service.loaded || service.error.is_some() {
        runtimes.push(RuntimeInfo {
          runtime_id: id.into(),
          instance_id: format!("{id}:external"),
          name: name.into(),
          kind,
          execution_kind: RuntimeExecutionKind::External,
          ownership: RuntimeOwnership::External,
          status: if service.error.is_some() {
            "unavailable"
          } else if service.running {
            "running"
          } else if service.loaded {
            "loaded"
          } else {
            "stopped"
          }
          .into(),
          pid: None,
          started_at_ms: None,
          session_key: None,
          session_id: None,
          project_id: None,
          working_directory: None,
          endpoint: crate::runtime_management::public_endpoint(&status.hub_url),
          detail: Some(
            if service.verified {
              "Recognized OS service; managed outside this viewer"
            } else {
              "Unverified OS service registration"
            }
            .into(),
          ),
          last_error: service.error.clone(),
          can_restart: false,
          restart_disabled_reason: Some("Background services are managed outside this viewer".into()),
        });
      }
    }
    runtimes
  }
  pub async fn stop(&self, app: &AppHandle) -> Result<HostStatus, String> {
    let mut running = self.operation.lock().await;
    if let Some(host) = running.take() {
      host.stop().await;
    }
    let mut status = self.status.lock().unwrap().clone();
    status.phase = HostPhase::Stopped;
    status.error = None;
    publish(app, &self.status, status.clone());
    Ok(status)
  }
  pub async fn stop_external(&self, app: &AppHandle) -> Result<HostStatus, String> {
    let running = self.operation.lock().await;
    if running.is_some() {
      return Err("Use Stop hosting for the connector owned by this app".into());
    }
    self.manage_background(app, false).await
  }
  pub async fn remove_external(&self, app: &AppHandle) -> Result<HostStatus, String> {
    let running = self.operation.lock().await;
    if running.is_some() {
      return Err("Stop app hosting before removing background service registrations".into());
    }
    self.manage_background(app, true).await
  }
  async fn manage_background(&self, app: &AppHandle, remove: bool) -> Result<HostStatus, String> {
    let state_dir = self.storage_path(false).await?;
    let observed = detected_status(state_dir.clone()).await?;
    if !(if remove {
      observed.external_remove_supported
    } else {
      observed.external_stop_supported
    }) {
      return Err(
        "Background hosting cannot be managed safely; refresh its status or stop unrecognized connectors manually"
          .into(),
      );
    }
    let path = state_dir.join("host.json");
    let profile = tokio::task::spawn_blocking(move || HostProfile::load(&path))
      .await
      .map_err(|_| "Host configuration task failed")??;
    let result = crate::external_host::manage(&state_dir, profile.as_ref(), remove).await;
    // Always publish observed partial progress, including a failed second stop
    // or removal. Never retain the old all-running view after one service stops.
    let mut status = detected_status(state_dir).await?;
    if let Err(error) = &result {
      status.error = Some(error.clone());
    }
    publish(app, &self.status, status.clone());
    result?;
    if status.background_services.has_loaded() {
      return Err("Background services are still loaded; refresh their status".into());
    }
    if remove && status.background_services.has_registered() {
      return Err("Background hosting registrations remain; refresh their status".into());
    }
    Ok(status)
  }
  pub async fn shutdown(&self, app: &AppHandle) {
    self.closed.store(true, Ordering::Release);
    self.closing.cancel();
    let _ = self.stop(app).await;
  }
  pub async fn pairing(&self) -> Result<HostPairing, String> {
    let path = self.storage_path(false).await?;
    tokio::task::spawn_blocking(move || pairing(&path))
      .await
      .map_err(|_| "Pairing display task failed")?
  }
}
fn publish(app: &AppHandle, state: &Mutex<HostStatus>, status: HostStatus) {
  *state.lock().unwrap() = status.clone();
  let _ = app.emit("local-host-status", status);
  crate::runtime_management::changed(app);
}

fn hosting_runtime(status: HostStatus, owned: Option<&RunningHost>) -> RuntimeInfo {
  let external = owned.is_none() && matches!(status.phase, HostPhase::External | HostPhase::Background);
  RuntimeInfo {
    runtime_id: "hosting".into(),
    instance_id: owned
      .map(|host| host.instance_id.clone())
      .unwrap_or_else(|| "hosting:stopped".into()),
    name: "Hub hosting".into(),
    kind: RuntimeKind::Hosting,
    execution_kind: if external {
      RuntimeExecutionKind::External
    } else {
      RuntimeExecutionKind::Embedded
    },
    ownership: if external {
      RuntimeOwnership::External
    } else {
      RuntimeOwnership::Viewer
    },
    status: serde_json::to_value(&status.phase)
      .ok()
      .and_then(|value| value.as_str().map(str::to_owned))
      .unwrap_or_else(|| "unavailable".into()),
    pid: owned.map(|_| std::process::id()),
    started_at_ms: owned.map(|host| host.started_at_ms),
    session_key: None,
    session_id: None,
    project_id: None,
    working_directory: None,
    endpoint: crate::runtime_management::public_endpoint(&status.hub_url),
    detail: Some(
      if owned.is_some() {
        "Viewer-owned API and connector; stops when the viewer quits"
      } else if external {
        "Hosting is owned outside this viewer"
      } else {
        "Start hosting from Machines to create an app-owned runtime"
      }
      .into(),
    ),
    last_error: status.error,
    can_restart: owned.is_some_and(|host| host.configuration.is_some()),
    restart_disabled_reason: owned
      .is_none()
      .then(|| "No hosting runtime is owned by this viewer".into()),
  }
}

fn restart_configuration(host: &RunningHost, expected_instance_id: &str) -> Result<StartHostRequest, String> {
  if host.instance_id != expected_instance_id {
    return Err("Hosting instance changed; refresh runtimes before restarting".into());
  }
  host
    .configuration
    .clone()
    .ok_or_else(|| "Hosting cannot be restarted safely".into())
}

/// A stopped local connector can remain briefly registered at the Hub until
/// the remote socket task observes EOF. Only owned restart uses this handoff;
/// the normal start path still performs every competing-owner check.
async fn wait_for_host_offline<P, F>(
  mut probe: P,
  closing: &CancellationToken,
  timeout: Duration,
  poll_interval: Duration,
) -> Result<(), String>
where
  P: FnMut() -> F,
  F: Future<Output = Result<bool, String>>,
{
  let handoff = async {
    loop {
      if !probe().await? {
        return Ok::<(), String>(());
      }
      tokio::time::sleep(poll_interval).await;
    }
  };
  tokio::select! {
    biased;
    _ = closing.cancelled() => Err("App hosting is shutting down".into()),
    result = tokio::time::timeout(timeout, handoff) => result
      .map_err(|_| "The previous connector is still visible at the Hub; refresh hosting before retrying".to_string())?
      .map_err(|error| format!("Could not confirm the previous connector stopped: {error}")),
  }
}
async fn detected_status(path: PathBuf) -> Result<HostStatus, String> {
  let read_path = path.clone();
  let (mut status, profile, held) = tokio::task::spawn_blocking(move || {
    let path = read_path;
    let profile = HostProfile::load(&path.join("host.json"))?;
    let status = profile_status(&path, profile.as_ref())?;
    let held = HostLease::is_held(&path.join("host-enrollment.key"))?;
    Ok::<_, String>((status, profile, held))
  })
  .await
  .map_err(|_| "Host status task failed")??;
  status.background_services = crate::external_host::inspect(&path, profile.as_ref()).await?;
  status.external_stop_supported = status.background_services.stop_supported();
  status.external_remove_supported = status.background_services.remove_supported();
  if held || status.background_services.connector.loaded {
    status.phase = HostPhase::External;
  } else if status.background_services.api.loaded {
    status.phase = HostPhase::Background;
  } else if let Some(profile) = &profile {
    match host_setup::existing_host_online(profile).await {
      Ok(true) => status.phase = HostPhase::External,
      Ok(false) => {}
      Err(error) => {
        status.phase = HostPhase::Unavailable;
        status.error = Some(error);
      }
    }
  }
  // A manual/unknown connector may depend on the registered API. Do not
  // advertise a whole-unit stop that would leave that connector alive.
  if matches!(status.phase, HostPhase::External) && !status.background_services.connector.loaded {
    status.external_stop_supported = false;
    status.external_remove_supported = false;
  }
  if !status.background_services.has_loaded()
    && let Some(error) = status
      .background_services
      .api
      .error
      .as_ref()
      .or(status.background_services.connector.error.as_ref())
  {
    status.phase = HostPhase::Unavailable;
    status.error = Some(error.clone());
  }
  Ok(status)
}

#[cfg(test)]
fn saved_status(path: &std::path::Path) -> Result<HostStatus, String> {
  profile_status(path, HostProfile::load(&path.join("host.json"))?.as_ref())
}
fn profile_status(path: &std::path::Path, profile: Option<&HostProfile>) -> Result<HostStatus, String> {
  let Some(profile) = profile else {
    return Ok(HostStatus::default());
  };
  let identity = NoiseIdentity::load(&path.join("host-noise.key"))?;
  Ok(HostStatus {
    phase: HostPhase::Stopped,
    hub_url: profile.hub_url.clone(),
    name: profile.name.clone(),
    allow_control: profile.allow_control,
    machine_reference: Some(format!("{}@{}", profile.host_id, identity.public_key())),
    error: None,
    external_stop_supported: false,
    external_remove_supported: false,
    background_services: Default::default(),
  })
}
fn pairing(path: &std::path::Path) -> Result<HostPairing, String> {
  let profile = HostProfile::load(&path.join("host.json"))?.ok_or("Start hosting to set up this machine first")?;
  let identity = NoiseIdentity::load(&path.join("host-noise.key"))?;
  let secret = tokn_session_hub::onboarding::read_totp_secret(&path.join("host-access.json"))?;
  let setup_uri = secret.provisioning_uri(&profile.name)?;
  let svg = qrcode::QrCode::new(setup_uri.as_bytes())
    .map_err(|_| "Could not render authenticator QR")?
    .render::<qrcode::render::svg::Color>()
    .min_dimensions(220, 220)
    .build();
  let now = tokn_session_hub::secure_client::unix_time()?;
  Ok(HostPairing {
    machine_reference: format!("{}@{}", profile.host_id, identity.public_key()),
    qr_data_url: format!("data:image/svg+xml;base64,{}", STANDARD.encode(svg)),
    setup_uri,
    current_code: secret.code_at(now),
    host_time: now,
    expires_at: now + TOTP_PERIOD - now % TOTP_PERIOD,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn setup(path: &std::path::Path) -> host_setup::PreparedHost {
    host_setup::prepare_host(HostOptions {
      hub: Some(url::Url::parse("https://hub.example").unwrap()),
      name: Some("Test machine".into()),
      viewer_url: None,
      passkey_origin: None,
      state_dir: path.to_owned(),
      totp_secret_file: None,
      viewer_token: None,
      allow_control: Some(false),
      insecure_loopback: false,
      ice_servers: None,
    })
    .unwrap()
  }
  #[test]
  fn pairing_display_reuses_identity_and_sha256_secret_without_changing_trust() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let prepared = setup(path);
    let state = path.join("host-access.json");
    let device = NoiseIdentity::generate().unwrap().public_key();
    tokn_session_hub::onboarding::authorize_device(&state, &device, 10, 300).unwrap();
    let before = std::fs::read(&state).unwrap();
    let display = pairing(path).unwrap();
    assert_eq!(display.machine_reference, prepared.reference);
    let uri = url::Url::parse(&display.setup_uri).unwrap();
    let parameters: std::collections::HashMap<_, _> = uri.query_pairs().collect();
    assert_eq!(parameters["algorithm"], "SHA256");
    assert_eq!(parameters["digits"], "6");
    assert_eq!(parameters["period"], "30");
    assert_eq!(
      display.current_code,
      tokn_session_hub::onboarding::read_totp_secret(&state)
        .unwrap()
        .code_at(display.host_time)
    );
    assert!((1..=30).contains(&(display.expires_at - display.host_time)));
    let svg = STANDARD
      .decode(display.qr_data_url.strip_prefix("data:image/svg+xml;base64,").unwrap())
      .unwrap();
    assert!(String::from_utf8(svg).unwrap().contains("<svg"));
    assert_eq!(std::fs::read(&state).unwrap(), before);
    assert_eq!(
      saved_status(path).unwrap().machine_reference.as_deref(),
      Some(prepared.reference.as_str())
    );
  }
  #[test]
  fn inspecting_unconfigured_host_does_not_create_keys_or_pairing_state() {
    let directory = tempfile::tempdir().unwrap();
    assert!(saved_status(directory.path()).unwrap().machine_reference.is_none());
    assert!(pairing(directory.path()).is_err());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
  }
  #[tokio::test]
  async fn unified_host_inspects_legacy_ownership_and_blocks_writes_until_migration() {
    let home = tempfile::tempdir().unwrap();
    let paths = tokn_session_storage::SessionPaths::from_home(home.path());
    let legacy = paths.legacy_hub_dir();
    setup(&legacy);
    let key = legacy.join("host-enrollment.key");
    let lease = HostLease::acquire(&key).unwrap();
    let local = LocalHost::unified(paths.clone());
    assert!(matches!(local.status().await.unwrap().phase, HostPhase::External));
    assert_eq!(local.storage_path(false).await.unwrap(), legacy);
    assert!(local.storage_path(true).await.unwrap_err().contains("migrate-storage"));
    assert!(!paths.hub_dir().exists());
    drop(lease);
    tokn_session_hub::storage::migrate_legacy_storage(&paths, true).unwrap();
    assert_eq!(local.storage_path(false).await.unwrap(), paths.hub_dir());
    assert_eq!(local.storage_path(true).await.unwrap(), paths.hub_dir());
  }

  #[tokio::test]
  async fn external_lock_is_detected_without_contacting_hub_or_changing_state() {
    let directory = tempfile::tempdir().unwrap();
    setup(directory.path());
    let path = directory.path().join("host.json");
    let before = std::fs::read(&path).unwrap();
    let _lease = HostLease::acquire(&directory.path().join("host-enrollment.key")).unwrap();
    let status = detected_status(directory.path().to_owned()).await.unwrap();
    assert!(matches!(status.phase, HostPhase::External));
    assert!(status.error.is_none());
    assert_eq!(std::fs::read(&path).unwrap(), before);
  }
  #[tokio::test]
  async fn legacy_hub_status_distinguishes_external_offline_and_unavailable() {
    use axum::{Router, extract::WebSocketUpgrade, http::StatusCode, response::IntoResponse, routing::get};
    use std::sync::atomic::AtomicU8;
    let directory = tempfile::tempdir().unwrap();
    setup(directory.path());
    let path = directory.path().join("host.json");
    let mut profile = HostProfile::load(&path).unwrap().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    profile.hub_url = format!("http://{}", listener.local_addr().unwrap());
    profile.insecure_loopback = true;
    tokn_session_hub::onboarding::save_configuration(&path, &profile).unwrap();
    let mode = Arc::new(AtomicU8::new(1));
    let state = mode.clone();
    let router = Router::new().route(
      &format!("/hub/v1/secure/{}", profile.host_id),
      get(move |socket: WebSocketUpgrade| {
        let mode = state.load(Ordering::Acquire);
        async move {
          match mode {
            1 => socket.on_upgrade(|_socket| async {}).into_response(),
            0 => (
              StatusCode::BAD_GATEWAY,
              axum::Json(serde_json::json!({"error": "Host is offline"})),
            )
              .into_response(),
            _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
          }
        }
      }),
    );
    let task = tokio::spawn(async move {
      axum::serve(listener, router).await.unwrap();
    });
    assert!(matches!(
      detected_status(directory.path().to_owned()).await.unwrap().phase,
      HostPhase::External
    ));
    mode.store(0, Ordering::Release);
    assert!(matches!(
      detected_status(directory.path().to_owned()).await.unwrap().phase,
      HostPhase::Stopped
    ));
    mode.store(2, Ordering::Release);
    let unavailable = detected_status(directory.path().to_owned()).await.unwrap();
    assert!(matches!(unavailable.phase, HostPhase::Unavailable));
    assert!(unavailable.error.is_some());
    task.abort();
  }
  #[tokio::test]
  async fn unconfigured_status_inspection_does_not_create_state() {
    let directory = tempfile::tempdir().unwrap();
    let status = detected_status(directory.path().to_owned()).await.unwrap();
    assert!(matches!(status.phase, HostPhase::Stopped));
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
  }
  #[tokio::test]
  async fn unconfigured_runtime_inventory_is_read_only_and_cannot_restart() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalHost::new(directory.path().to_owned());
    let runtimes = local.runtime_inventory().await;
    assert_eq!(runtimes.len(), 1);
    assert_eq!(runtimes[0].runtime_id, "hosting");
    assert_eq!(runtimes[0].status, "stopped");
    assert!(!runtimes[0].can_restart);
    assert!(runtimes[0].endpoint.is_none());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    assert!(local.operation.lock().await.is_none());
  }
  #[tokio::test]
  async fn restart_capability_requires_the_owned_instance_and_reuses_its_configuration() {
    let host = RunningHost {
      shutdown: CancellationToken::new(),
      task: tokio::spawn(std::future::pending()),
      instance_id: "owned-instance".into(),
      started_at_ms: 42,
      configuration: Some(StartHostRequest {
        hub_url: "https://hub.example".into(),
        name: "Existing host".into(),
        allow_control: false,
      }),
    };
    assert!(
      restart_configuration(&host, "stale-instance")
        .unwrap_err()
        .contains("instance changed")
    );
    let configuration = restart_configuration(&host, "owned-instance").unwrap();
    assert_eq!(configuration.hub_url, "https://hub.example");
    assert_eq!(configuration.name, "Existing host");
    assert!(!configuration.allow_control);
    let row = hosting_runtime(HostStatus::default(), Some(&host));
    assert_eq!(row.instance_id, "owned-instance");
    assert_eq!(row.started_at_ms, Some(42));
    assert!(row.can_restart);
    let external = hosting_runtime(
      HostStatus {
        phase: HostPhase::External,
        ..Default::default()
      },
      None,
    );
    assert_eq!(external.ownership, RuntimeOwnership::External);
    assert!(!external.can_restart);
    assert!(external.pid.is_none());
  }
  #[tokio::test]
  async fn owned_restart_handoff_waits_until_the_previous_connector_is_offline() {
    use std::sync::atomic::AtomicUsize;
    let calls = AtomicUsize::new(0);
    wait_for_host_offline(
      || std::future::ready(Ok(calls.fetch_add(1, Ordering::SeqCst) == 0)),
      &CancellationToken::new(),
      Duration::from_millis(200),
      Duration::from_millis(1),
    )
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
  }
  #[tokio::test]
  async fn owned_restart_handoff_rejects_a_connector_that_remains_online() {
    let error = wait_for_host_offline(
      || std::future::ready(Ok(true)),
      &CancellationToken::new(),
      Duration::from_millis(20),
      Duration::from_millis(1),
    )
    .await
    .unwrap_err();
    assert!(error.contains("still visible at the Hub"));
  }
  #[tokio::test]
  async fn owned_restart_handoff_propagates_probe_errors_without_treating_them_as_offline() {
    let error = wait_for_host_offline(
      || std::future::ready(Err("Hub unavailable".into())),
      &CancellationToken::new(),
      Duration::from_millis(200),
      Duration::from_millis(1),
    )
    .await
    .unwrap_err();
    assert!(error.contains("Hub unavailable"));
  }
  #[tokio::test]
  async fn owned_restart_handoff_cancels_an_in_flight_probe_when_the_app_closes() {
    let closing = CancellationToken::new();
    let entered = Arc::new(tokio::sync::Notify::new());
    let waiting = {
      let (closing, entered) = (closing.clone(), entered.clone());
      tokio::spawn(async move {
        wait_for_host_offline(
          || {
            entered.notify_one();
            std::future::pending::<Result<bool, String>>()
          },
          &closing,
          Duration::from_secs(60),
          Duration::from_millis(1),
        )
        .await
      })
    };
    entered.notified().await;
    closing.cancel();
    let error = tokio::time::timeout(Duration::from_secs(1), waiting)
      .await
      .unwrap()
      .unwrap()
      .unwrap_err();
    assert!(error.contains("shutting down"));
    assert!(
      wait_for_host_offline(
        || async { panic!("a closed app must not probe") },
        &closing,
        Duration::from_millis(200),
        Duration::from_millis(1),
      )
      .await
      .unwrap_err()
      .contains("shutting down")
    );
  }
  #[tokio::test]
  async fn stopping_owned_host_releases_its_port_and_process_lease() {
    let directory = tempfile::tempdir().unwrap();
    let key = directory.path().join("host-enrollment.key");
    assert!(!HostLease::is_held(&key).unwrap());
    let lease = HostLease::acquire(&key).unwrap();
    assert!(HostLease::is_held(&key).unwrap());
    assert!(HostLease::acquire(&key).is_err());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let cancel = shutdown.clone();
    let task = tokio::spawn(async move {
      let _lease = lease;
      let _listener = listener;
      cancel.cancelled().await;
    });
    RunningHost {
      shutdown,
      task,
      instance_id: "test".into(),
      started_at_ms: 0,
      configuration: None,
    }
    .stop()
    .await;
    assert!(!HostLease::is_held(&key).unwrap());
    assert!(HostLease::acquire(&key).is_ok());
    assert!(tokio::net::TcpListener::bind(address).await.is_ok());
  }
}
