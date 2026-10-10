//! App-owned hosting reuses local trust, never installs a background service.
use crate::local_viewer::LocalViewer;
use base64::{Engine, engine::general_purpose::STANDARD};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::{
  path::PathBuf,
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  },
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
use zeroize::Zeroize;

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostPhase {
  #[default]
  Stopped,
  External,
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
    }
  }
}
#[derive(Deserialize)]
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
  operation: AsyncMutex<Option<RunningHost>>,
  status: Arc<Mutex<HostStatus>>,
  closed: AtomicBool,
}
impl LocalHost {
  pub fn new(state_dir: PathBuf) -> Self {
    Self {
      state_dir,
      operation: AsyncMutex::new(None),
      status: Arc::new(Mutex::new(HostStatus::default())),
      closed: AtomicBool::new(false),
    }
  }
  pub async fn status(&self) -> Result<HostStatus, String> {
    let running = self.operation.lock().await;
    if running.is_some() {
      return Ok(self.status.lock().unwrap().clone());
    }
    detected_status(self.state_dir.clone()).await
  }

  pub async fn start(&self, app: AppHandle, request: StartHostRequest) -> Result<HostStatus, String> {
    let mut running = self.operation.lock().await;
    if self.closed.load(Ordering::Acquire) {
      return Err("App hosting is shutting down".into());
    }
    if running.is_some() {
      return Err("Stop app hosting before changing its settings".into());
    }
    let path = self.state_dir.clone();
    let (lease, previous) = tokio::task::spawn_blocking(move || {
      let lease = HostLease::acquire(&path.join("host-enrollment.key"))?;
      let previous = HostProfile::load(&path.join("host.json"))?;
      Ok::<_, String>((lease, previous))
    })
    .await
    .map_err(|_| "Host ownership task failed")??;
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
    let path = self.state_dir.clone();
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
        publish(&app, &state, status);
      }
    });
    *running = Some(RunningHost { shutdown, task });
    Ok(self.status.lock().unwrap().clone())
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
    let status = detected_status(self.state_dir.clone()).await?;
    if !matches!(status.phase, HostPhase::External) {
      return Err("External hosting is no longer detected; refresh its status".into());
    }
    let path = self.state_dir.join("host.json");
    let profile = tokio::task::spawn_blocking(move || HostProfile::load(&path))
      .await
      .map_err(|_| "Host configuration task failed")??
      .ok_or("No saved host configuration")?;
    crate::external_host::stop(&self.state_dir, &profile).await?;
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), async {
      loop {
        let status = detected_status(self.state_dir.clone()).await?;
        if matches!(status.phase, HostPhase::Stopped) {
          break Ok::<_, String>(status);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
      }
    })
    .await
    .map_err(|_| {
      "Connector service was unloaded, but hosting has not been confirmed offline. Refresh its status.".to_string()
    })??;
    publish(app, &self.status, status.clone());
    Ok(status)
  }
  pub async fn shutdown(&self, app: &AppHandle) {
    self.closed.store(true, Ordering::Release);
    let _ = self.stop(app).await;
  }
  pub async fn pairing(&self) -> Result<HostPairing, String> {
    let path = self.state_dir.clone();
    tokio::task::spawn_blocking(move || pairing(&path))
      .await
      .map_err(|_| "Pairing display task failed")?
  }
}
fn publish(app: &AppHandle, state: &Mutex<HostStatus>, status: HostStatus) {
  *state.lock().unwrap() = status.clone();
  let _ = app.emit("local-host-status", status);
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
  if held {
    status.phase = HostPhase::External;
  } else if let Some(profile) = &profile {
    match host_setup::existing_host_online(&profile).await {
      Ok(true) => status.phase = HostPhase::External,
      Ok(false) => {}
      Err(error) => {
        status.phase = HostPhase::Unavailable;
        status.error = Some(error);
      }
    }
  }
  if matches!(status.phase, HostPhase::External) {
    if let Some(profile) = &profile {
      status.external_stop_supported = crate::external_host::available(&path, profile).await;
    }
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
            0 => StatusCode::NOT_FOUND.into_response(),
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
    RunningHost { shutdown, task }.stop().await;
    assert!(!HostLease::is_held(&key).unwrap());
    assert!(HostLease::acquire(&key).is_ok());
    assert!(tokio::net::TcpListener::bind(address).await.is_ok());
  }
}
