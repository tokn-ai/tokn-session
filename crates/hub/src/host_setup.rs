//! Shared local host setup. Never sends authenticator material to the Hub.
use crate::{
  connector::{self, ConnectorConfig, PairedHostConfig},
  onboarding::{self, HostProfile},
  pairing::TotpSecret,
  secure::NoiseIdentity,
};
use std::{
  fs::{self, OpenOptions},
  io::Read,
  path::{Path, PathBuf},
};
use url::Url;
use zeroize::Zeroizing;

pub struct PreparedHost {
  pub config: ConnectorConfig,
  pub is_new_secret: bool,
  pub reference: String,
  pub passkey_origin: Option<String>,
}

pub struct HostOptions {
  pub hub: Option<Url>,
  pub name: Option<String>,
  pub viewer_url: Option<Url>,
  pub passkey_origin: Option<String>,
  pub state_dir: PathBuf,
  pub totp_secret_file: Option<PathBuf>,
  pub viewer_token: Option<String>,
  pub allow_control: Option<bool>,
  pub insecure_loopback: bool,
  pub ice_servers: Option<Vec<String>>,
}

pub fn prepare_host(options: HostOptions) -> Result<PreparedHost, String> {
  let profile_path = options.state_dir.join("host.json");
  let previous = HostProfile::load(&profile_path)?;
  let hub = options
    .hub
    .or_else(|| previous.as_ref().and_then(|profile| Url::parse(&profile.hub_url).ok()))
    .ok_or("First setup requires --hub https://your-hub.example")?;
  let local_url = options
    .viewer_url
    .or_else(|| {
      previous
        .as_ref()
        .and_then(|profile| Url::parse(&profile.viewer_url).ok())
    })
    .unwrap_or_else(|| Url::parse("http://127.0.0.1:5558").unwrap());
  let passkey_origin = options
    .passkey_origin
    .or_else(|| previous.as_ref().and_then(|profile| profile.passkey_origin.clone()));
  let passkey_origin = match passkey_origin {
    Some(origin) => Some(
      crate::host_passkeys::validate_origin(&origin)?
        .origin()
        .ascii_serialization(),
    ),
    None => {
      let origin = onboarding::canonical_hub(&hub)?;
      crate::host_passkeys::validate_origin(&origin)
        .ok()
        .map(|origin| origin.origin().ascii_serialization())
    }
  };
  let profile = HostProfile {
    version: 1,
    host_id: previous
      .as_ref()
      .map(|profile| profile.host_id.clone())
      .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
    hub_url: onboarding::canonical_hub(&hub)?,
    name: options
      .name
      .or_else(|| previous.as_ref().map(|profile| profile.name.clone()))
      .unwrap_or_else(|| "Session host".into()),
    viewer_url: local_url.to_string(),
    allow_control: options
      .allow_control
      .unwrap_or_else(|| previous.as_ref().is_some_and(|profile| profile.allow_control)),
    insecure_loopback: options.insecure_loopback || previous.as_ref().is_some_and(|profile| profile.insecure_loopback),
    passkey_origin,
    ice_servers: options.ice_servers.unwrap_or_else(|| {
      previous
        .as_ref()
        .map(|profile| profile.ice_servers.clone())
        .unwrap_or_default()
    }),
  };
  let key_file = options.state_dir.join("host-enrollment.key");
  let noise_key_file = options.state_dir.join("host-noise.key");
  let state_file = options.state_dir.join("host-access.json");
  if previous.is_none() && [&key_file, &noise_key_file].iter().any(|path| path.exists()) {
    return Err("Host configuration is missing for existing keys; restore host.json before reconnecting".into());
  }
  let config = ConnectorConfig {
    hub_url: hub,
    local_url,
    key_file: key_file.clone(),
    name: profile.name.clone(),
    local_token: options.viewer_token,
    allow_control: profile.allow_control,
    insecure_loopback: profile.insecure_loopback,
    ice_servers: profile.ice_servers.clone(),
    secure: None,
    paired: Some(PairedHostConfig {
      host_id: profile.host_id.clone(),
      noise_key_file: noise_key_file.clone(),
      state_file: state_file.clone(),
    }),
  };
  config.validate()?;
  if previous.is_some()
    && [&key_file, &noise_key_file, &state_file]
      .iter()
      .any(|path| !path.is_file())
  {
    return Err("Saved host keys or pairing state are missing; restore them before reconnecting".into());
  }
  if options.totp_secret_file.is_some() && state_file.try_exists().map_err(|e| e.to_string())? {
    return Err("Authenticator is already configured; importing must not replace existing device trust".into());
  }
  let is_new_secret = !state_file.try_exists().map_err(|e| e.to_string())?;
  // Validate user input and existing private state before creating identities.
  // An invalid import must be retryable without leaving a half-created host.
  let new_secret = if is_new_secret {
    Some(match options.totp_secret_file {
      Some(path) => read_seed(&path)?,
      None => TotpSecret::generate(),
    })
  } else {
    onboarding::read_totp_secret(&state_file)?;
    onboarding::validate_host_passkey_origin(&state_file, profile.passkey_origin.as_deref())?;
    None
  };
  connector::initialize_identity(&key_file)?;
  let noise_identity = NoiseIdentity::load_or_create(&noise_key_file)?;
  if let Some(secret) = new_secret {
    onboarding::initialize_host_access(&state_file, &secret)?;
  }
  profile.save(&profile_path)?;
  Ok(PreparedHost {
    config,
    is_new_secret,
    reference: format!("{}@{}", profile.host_id, noise_identity.public_key()),
    passkey_origin: profile.passkey_origin,
  })
}

pub fn read_seed(path: &Path) -> Result<TotpSecret, String> {
  let metadata = fs::symlink_metadata(path).map_err(|e| format!("Could not inspect authenticator import: {e}"))?;
  if !metadata.is_file() {
    return Err("Authenticator import must be a private regular file".into());
  }
  let mut options = OpenOptions::new();
  options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
  let file = options
    .open(path)
    .map_err(|e| format!("Could not open authenticator import: {e}"))?;
  let metadata = file.metadata().map_err(|e| e.to_string())?;
  if !metadata.is_file() || metadata.len() > 256 {
    return Err("Authenticator import must be a Base32 seed of at most 256 bytes".into());
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no memory side effects.
    if metadata.mode() & 0o777 != 0o600 || metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() } {
      return Err("Authenticator import must be owned by the current user with mode 0600 and no hard links".into());
    }
  }
  let mut bytes = Zeroizing::new(String::new());
  file
    .take(257)
    .read_to_string(&mut bytes)
    .map_err(|_| "Authenticator import must contain Base32 text")?;
  TotpSecret::from_base32(&bytes)
}

/// Older connectors do not hold a local ownership lock. Check the saved Hub's
/// public secure route before starting, so the app cannot silently displace one.
pub async fn existing_host_online(profile: &HostProfile) -> Result<bool, String> {
  let mut url = Url::parse(&profile.hub_url).map_err(|_| "Saved Hub URL is invalid")?;
  let scheme = match url.scheme() {
    "https" => "wss",
    "http" if profile.insecure_loopback && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")) => "ws",
    _ => return Err("Saved Hub requires HTTPS".into()),
  };
  url.set_scheme(scheme).map_err(|_| "Saved Hub URL is invalid")?;
  url.set_path(&format!("/hub/v1/secure/{}", profile.host_id));
  let result = tokio::time::timeout(
    std::time::Duration::from_secs(5),
    tokio_tungstenite::connect_async(url.as_str()),
  )
  .await
  .map_err(|_| "Could not check whether another connector is online: Hub check timed out")?;
  match result {
    Ok((socket, _)) => {
      drop(socket);
      Ok(true)
    }
    Err(tokio_tungstenite::tungstenite::Error::Http(response))
      if response.status().as_u16() == 404
        || (response.status().as_u16() == 502
          && response.body().as_ref().is_some_and(|body| {
            serde_json::from_slice::<serde_json::Value>(body)
              .ok()
              .is_some_and(|value| value["error"].as_str() == Some("Host is offline"))
          })) =>
    {
      Ok(false)
    }
    Err(tokio_tungstenite::tungstenite::Error::Http(response)) => Err(format!(
      "Could not check whether another connector is online: Hub returned HTTP {}. Check the saved Hub connection and retry.",
      response.status().as_u16()
    )),
    Err(_) => {
      Err("Could not check whether another connector is online. Check the saved Hub connection and retry.".into())
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use axum::{Router, extract::WebSocketUpgrade, http::StatusCode, response::IntoResponse, routing::get};

  #[tokio::test]
  async fn hub_offline_response_is_distinct_from_gateway_failure() {
    use std::sync::{
      Arc,
      atomic::{AtomicBool, Ordering},
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let offline = Arc::new(AtomicBool::new(true));
    let state = offline.clone();
    let router = Router::new().fallback(get(move || {
      let offline = state.load(Ordering::Acquire);
      async move {
        if offline {
          (
            StatusCode::BAD_GATEWAY,
            axum::Json(serde_json::json!({"error": "Host is offline"})),
          )
            .into_response()
        } else {
          (StatusCode::BAD_GATEWAY, "nginx upstream unavailable").into_response()
        }
      }
    }));
    let task = tokio::spawn(async move {
      axum::serve(listener, router).await.unwrap();
    });
    let profile = HostProfile {
      version: 1,
      host_id: "550e8400-e29b-41d4-a716-446655440000".into(),
      hub_url: format!("http://{address}"),
      name: "Test".into(),
      viewer_url: "http://127.0.0.1:5558".into(),
      allow_control: false,
      insecure_loopback: true,
      passkey_origin: None,
      ice_servers: Vec::new(),
    };
    assert!(!existing_host_online(&profile).await.unwrap());
    offline.store(false, Ordering::Release);
    assert!(existing_host_online(&profile).await.unwrap_err().contains("HTTP 502"));
    task.abort();
  }
  #[tokio::test]
  async fn legacy_online_connector_is_detected_without_replacing_it() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let host_id = "550e8400-e29b-41d4-a716-446655440000";
    let path = format!("/hub/v1/secure/{host_id}");
    let router = Router::new()
      .route(
        &path,
        get(|socket: WebSocketUpgrade| async move { socket.on_upgrade(|_socket| async {}).into_response() }),
      )
      .fallback(|| async { StatusCode::NOT_FOUND });
    let task = tokio::spawn(async move {
      axum::serve(listener, router).await.unwrap();
    });
    let mut profile = HostProfile {
      version: 1,
      host_id: host_id.into(),
      hub_url: format!("http://{address}"),
      name: "Test".into(),
      viewer_url: "http://127.0.0.1:5558".into(),
      allow_control: false,
      insecure_loopback: true,
      passkey_origin: None,
      ice_servers: Vec::new(),
    };
    assert!(existing_host_online(&profile).await.unwrap());
    profile.host_id = "550e8400-e29b-41d4-a716-446655440001".into();
    assert!(!existing_host_online(&profile).await.unwrap());
    profile.insecure_loopback = false;
    assert!(existing_host_online(&profile).await.is_err());
    task.abort();
  }
}
