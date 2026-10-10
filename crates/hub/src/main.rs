use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
use std::{
  fs::{self, OpenOptions},
  io::{Read, Write},
  net::SocketAddr,
  path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;
use tokn_session_hub::{
  client_onboarding::{self, PairedClientConfig},
  connector::{self, ConnectorConfig, SecureHostConfig},
  onboarding,
  secure::{Grant, GrantScope, NoiseIdentity, OwnerIdentity},
  secure_client::{self, ClientConfig},
  server::{self, HubState},
  store::Store,
};
use url::Url;

mod cli_onboarding;

#[derive(Parser)]
#[command(about = "Passwordless access to session hosts through one Hub")]
struct Args {
  #[command(subcommand)]
  command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum KeyKind {
  /// Owner signing key, kept on a trusted device rather than the Hub.
  Owner,
  /// Encryption identity for a host or recipient client.
  Device,
}

#[derive(Subcommand)]
enum Command {
  /// Serve the Hub API and viewer. Use HTTPS termination for remote access.
  Serve {
    #[arg(long, default_value = "127.0.0.1:5559")]
    bind: SocketAddr,
    /// Stable browser origin used by passkeys (HTTPS, or http://localhost for development).
    #[arg(long, default_value = "http://localhost:5559")]
    public_url: String,
    #[arg(long)]
    state_path: Option<PathBuf>,
    #[arg(long, default_value = "apps/viewer/dist")]
    web_root: PathBuf,
    #[arg(long)]
    api_only: bool,
  },
  /// Enroll this host and keep an outbound tunnel to its loopback viewer-api.
  Connect {
    #[arg(long)]
    hub: Option<Url>,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    viewer_url: Option<Url>,
    /// Stable Hub browser origin for host passkeys; defaults to --hub when it uses a hostname.
    #[arg(long, conflicts_with_all = ["trusted_hub", "owner_public_key"])]
    passkey_origin: Option<String>,
    /// STUN URLs for direct WebRTC (comma-separated). Omit to keep saved settings;
    /// specify the flag without URLs to clear them. No public service is enabled by default.
    #[arg(long = "stun-server", value_delimiter = ',', num_args = 0.., conflicts_with_all = ["trusted_hub", "owner_public_key"])]
    ice_servers: Option<Vec<String>>,
    /// Local configuration, keys, authenticator, and trusted-device state.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Import an existing Base32 authenticator secret from a private local file on first setup.
    #[arg(long, conflicts_with_all = ["trusted_hub", "owner_public_key"])]
    totp_secret_file: Option<PathBuf>,
    #[arg(long, env = "TOKN_VIEWER_TOKEN", hide_env_values = true)]
    viewer_token: Option<String>,
    /// Generated enrollment identity. Reuse it when reconnecting to the same Hub.
    #[arg(long)]
    identity_file: Option<PathBuf>,
    /// Independently obtained owner signing public key. Requires E2EE for host content.
    #[arg(long, conflicts_with = "trusted_hub")]
    owner_public_key: Option<String>,
    /// Private host encryption identity, separate from its enrollment identity.
    #[arg(long, requires = "owner_public_key", conflicts_with = "trusted_hub")]
    noise_key_file: Option<PathBuf>,
    /// Local JSON array of revoked grant IDs; reloaded while serving requests.
    #[arg(long, requires = "owner_public_key", conflicts_with = "trusted_hub")]
    revocations_file: Option<PathBuf>,
    /// Explicit legacy mode: the Hub can read content and authorize clients.
    #[arg(long)]
    trusted_hub: bool,
    /// Allow sending input to live agents. Use --allow-control=false to disable it again.
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    allow_control: Option<bool>,
    /// Allow an unencrypted Hub URL only on loopback, for local development.
    #[arg(long)]
    insecure_loopback: bool,
  },
  /// Open an E2EE host through a locally installed viewer on loopback.
  Client {
    #[arg(long)]
    hub: Option<Url>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Select a previously paired host; otherwise reopen the last selection.
    #[arg(long, conflicts_with = "grant_file")]
    host: Option<String>,
    #[arg(long)]
    grant_file: Option<PathBuf>,
    /// Owner public key verified independently of the Hub and grant file.
    #[arg(long, requires = "grant_file")]
    owner_public_key: Option<String>,
    #[arg(long)]
    identity_file: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1:0")]
    bind: SocketAddr,
    #[arg(long, default_value = "apps/viewer/dist")]
    web_root: PathBuf,
    #[arg(long)]
    insecure_loopback: bool,
  },
  /// Show local authenticator setup, or import/export its secret for user-managed synchronization.
  Authenticator {
    /// Rotate a legacy SHA1 authenticator and retire unclassified device grants.
    #[arg(long, conflicts_with = "import_file")]
    upgrade_sha256: bool,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long, conflicts_with = "export_file")]
    import_file: Option<PathBuf>,
    /// Write a new mode-0600 Base32 file; never overwrite an existing file.
    #[arg(long)]
    export_file: Option<PathBuf>,
  },
  /// List this host's paired client public keys.
  Devices {
    #[arg(long)]
    state_dir: Option<PathBuf>,
  },
  /// Revoke a paired client on this host, including its active streams.
  ForgetDevice {
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    public_key: String,
  },
  /// Generate or display a local identity. Only the public key is printed.
  Keygen {
    #[arg(long, value_enum)]
    kind: KeyKind,
    #[arg(long)]
    key_file: PathBuf,
  },
  /// Sign a recipient's host access grant on a trusted owner device.
  Grant {
    #[arg(long)]
    owner_key_file: PathBuf,
    #[arg(long)]
    host_id: String,
    #[arg(long)]
    host_public_key: String,
    #[arg(long)]
    recipient_public_key: String,
    /// Explicit access to the entire host; required for agent control.
    #[arg(long, conflicts_with = "session_key")]
    all_sessions: bool,
    /// Exact session key to share. Repeat for multiple sessions; view access only.
    #[arg(long, required_unless_present = "all_sessions", conflicts_with = "all_sessions")]
    session_key: Vec<String>,
    #[arg(long, requires = "all_sessions", conflicts_with = "session_key")]
    allow_control: bool,
    /// Grant lifetime in seconds, starting now.
    #[arg(long, default_value_t = 86400)]
    expires_in: u64,
    /// New grant file. Existing files are never overwritten.
    #[arg(long)]
    out: PathBuf,
  },
  /// Add a grant ID to the host's local revocation file atomically.
  Revoke {
    #[arg(long)]
    revocations_file: PathBuf,
    #[arg(long)]
    grant_id: String,
  },
}

#[tokio::main]
async fn main() {
  if let Err(error) = run(Args::parse()).await {
    eprintln!("{error}");
    std::process::exit(1);
  }
}

fn default_path(file: &str) -> Result<PathBuf, String> {
  dirs::home_dir()
    .map(|home| home.join(".tokn/hub").join(file))
    .ok_or_else(|| "Cannot resolve home directory; specify an explicit state/key path".into())
}

async fn run(args: Args) -> Result<(), String> {
  let shutdown = CancellationToken::new();
  match args.command {
    Command::Serve {
      bind,
      public_url,
      state_path,
      web_root,
      api_only,
    } => {
      let origin = Url::parse(&public_url).map_err(|e| e.to_string())?;
      if !bind.ip().is_loopback() && origin.scheme() != "https" {
        return Err("Non-loopback Hub binding requires an HTTPS --public-url and HTTPS termination".into());
      }
      let state_path = match state_path {
        Some(path) => path,
        None => default_path("state.sqlite")?,
      };
      let state = HubState::new(Store::open(state_path)?, &public_url)?;
      let app = server::router(state.clone());
      let app = if api_only {
        app
      } else {
        server::with_web_ui(app, web_root)?
      };
      let listener = tokio::net::TcpListener::bind(bind).await.map_err(|e| e.to_string())?;
      eprintln!(
        "Hub listening on {} (public origin: {public_url})",
        listener.local_addr().map_err(|e| e.to_string())?
      );
      if let Some(token) = state.auth.bootstrap_token()? {
        let mut setup = origin;
        setup.set_path("/admin");
        setup.set_fragment(Some(&format!("bootstrap_token={token}")));
        eprintln!("Optional Hub administration: create the first passkey at {setup}");
      }
      let cancellation = shutdown.clone();
      axum::serve(listener, app)
        .with_graceful_shutdown(async move {
          let _ = tokio::signal::ctrl_c().await;
          cancellation.cancel();
          state.auth.shutdown();
          state.tunnels.shutdown();
        })
        .await
        .map_err(|e| e.to_string())
    }
    Command::Connect {
      hub,
      name,
      viewer_url,
      passkey_origin,
      ice_servers,
      state_dir,
      totp_secret_file,
      viewer_token,
      identity_file,
      owner_public_key,
      noise_key_file,
      revocations_file,
      trusted_hub,
      allow_control,
      insecure_loopback,
    } => {
      if owner_public_key.is_none() && !trusted_hub {
        if identity_file.is_some() {
          return Err("Paired mode manages its own keys; use --state-dir to choose their location".into());
        }
        let state_dir = state_dir.unwrap_or(default_path("")?);
        let lease = connector::HostLease::acquire(&state_dir.join("host-enrollment.key"))?;
        let config = cli_onboarding::prepare_host(cli_onboarding::HostOptions {
          hub,
          name,
          viewer_url,
          passkey_origin,
          ice_servers,
          state_dir,
          totp_secret_file,
          viewer_token,
          allow_control,
          insecure_loopback,
        })?;
        let signal = shutdown_signal(shutdown.clone());
        let result = connector::run_owned(config, shutdown, lease, None).await;
        signal.abort();
        return result;
      }
      let state_dir = state_dir.unwrap_or(default_path("")?);
      let key_file = match identity_file {
        Some(path) => path,
        None => state_dir.join("host.key"),
      };
      let secure = match owner_public_key {
        Some(owner_public_key) => Some(SecureHostConfig {
          noise_key_file: match noise_key_file {
            Some(path) => path,
            None => state_dir.join("host-noise.key"),
          },
          owner_public_key,
          revocations_file,
        }),
        None if trusted_hub => {
          eprintln!("Trusted Hub mode: the Hub can read session content and authorize access.");
          None
        }
        None => return Err("Specify --owner-public-key for E2EE or explicitly choose --trusted-hub".into()),
      };
      let config = ConnectorConfig {
        hub_url: hub.ok_or("Legacy Hub mode requires --hub")?,
        local_url: viewer_url.unwrap_or_else(|| Url::parse("http://127.0.0.1:5558").unwrap()),
        key_file,
        name: name.unwrap_or_else(|| "Session host".into()),
        local_token: viewer_token,
        allow_control: allow_control.unwrap_or(false),
        insecure_loopback,
        ice_servers: Vec::new(),
        secure,
        paired: None,
      };
      let signal = shutdown_signal(shutdown.clone());
      let result = connector::run(config, shutdown).await;
      signal.abort();
      result
    }
    Command::Client {
      hub,
      state_dir,
      host,
      grant_file,
      owner_public_key,
      identity_file,
      bind,
      web_root,
      insecure_loopback,
    } => {
      let state_dir = state_dir.unwrap_or(default_path("")?);
      if grant_file.is_none() {
        if identity_file.is_some() {
          return Err("Paired mode manages its own identity; use --state-dir to choose its location".into());
        }
        let (hub_url, insecure_loopback) = cli_onboarding::prepare_client(&state_dir, hub, insecure_loopback)?;
        let signal = shutdown_signal(shutdown.clone());
        let result = client_onboarding::run_paired(
          PairedClientConfig {
            hub_url,
            state_dir,
            bind,
            web_root,
            insecure_loopback,
            host_id: host,
          },
          shutdown,
        )
        .await;
        signal.abort();
        return result;
      }
      let identity_file = match identity_file {
        Some(path) => path,
        None => state_dir.join("client-noise.key"),
      };
      let signal = shutdown_signal(shutdown.clone());
      let result = secure_client::run(
        ClientConfig {
          hub_url: hub.ok_or("Legacy grant mode requires --hub")?,
          grant_file: grant_file.unwrap(),
          owner_public_key: owner_public_key.ok_or("Legacy grant mode requires --owner-public-key")?,
          identity_file,
          bind,
          web_root,
          insecure_loopback,
        },
        shutdown,
      )
      .await;
      signal.abort();
      result
    }
    Command::Authenticator {
      upgrade_sha256,
      state_dir,
      import_file,
      export_file,
    } => {
      let state_dir = state_dir.unwrap_or(default_path("")?);
      if upgrade_sha256 {
        use std::io::IsTerminal;
        if export_file.is_none() && !std::io::stderr().is_terminal() {
          return Err(
            "Upgrade requires a local terminal or --export-file so the new authenticator can be saved".into(),
          );
        }
        onboarding::upgrade_authenticator(&state_dir.join("host-access.json"))?;
      }
      cli_onboarding::authenticator(&state_dir, import_file, export_file)
    }
    Command::Devices { state_dir } => {
      let state_dir = state_dir.unwrap_or(default_path("")?);
      println!(
        "{}",
        serde_json::to_string_pretty(&onboarding::devices(&state_dir.join("host-access.json"))?)
          .map_err(|e| e.to_string())?
      );
      Ok(())
    }
    Command::ForgetDevice { state_dir, public_key } => {
      let state_dir = state_dir.unwrap_or(default_path("")?);
      onboarding::remove_device(&state_dir.join("host-access.json"), &public_key)?;
      println!("{}", json!({ "revoked_device": public_key }));
      Ok(())
    }
    Command::Keygen { kind, key_file } => {
      let (kind, public_key) = match kind {
        KeyKind::Owner => ("owner", OwnerIdentity::load_or_create(&key_file)?.public_key()),
        KeyKind::Device => ("device", NoiseIdentity::load_or_create(&key_file)?.public_key()),
      };
      println!("{}", json!({ "kind": kind, "public_key": public_key }));
      Ok(())
    }
    Command::Grant {
      owner_key_file,
      host_id,
      host_public_key,
      recipient_public_key,
      all_sessions,
      session_key,
      allow_control,
      expires_in,
      out,
    } => {
      if !owner_key_file.is_file() {
        return Err("Owner key does not exist; generate it with `keygen --kind owner` first".into());
      }
      if expires_in == 0 {
        return Err("Grant lifetime must be greater than zero".into());
      }
      let expires_at = secure_client::unix_time()?
        .checked_add(expires_in)
        .ok_or("Grant expiry overflows")?;
      let owner = OwnerIdentity::load_or_create(&owner_key_file)?;
      let grant = owner.sign_grant(Grant {
        version: 1,
        grant_id: uuid::Uuid::new_v4().to_string(),
        host_id,
        host_public_key,
        recipient_public_key,
        scope: if all_sessions {
          GrantScope::All {}
        } else {
          GrantScope::Sessions {
            session_keys: session_key,
          }
        },
        allow_control,
        expires_at,
      })?;
      let bytes = serde_json::to_vec(&grant).map_err(|e| e.to_string())?;
      write_new(&out, &bytes)?;
      println!(
        "{}",
        json!({ "grant_id": grant.grant.grant_id, "expires_at": expires_at, "file": out })
      );
      Ok(())
    }
    Command::Revoke {
      revocations_file,
      grant_id,
    } => revoke(&revocations_file, grant_id),
  }
}

fn shutdown_signal(shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
  tokio::spawn(async move {
    let _ = tokio::signal::ctrl_c().await;
    shutdown.cancel();
  })
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
  if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
  }
  let mut options = OpenOptions::new();
  options.write(true).create_new(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
  }
  let mut file = options
    .open(path)
    .map_err(|e| format!("Could not create {}: {e}", path.display()))?;
  file
    .write_all(bytes)
    .and_then(|_| file.sync_all())
    .map_err(|e| e.to_string())
}

fn revoke(path: &Path, grant_id: String) -> Result<(), String> {
  if grant_id.is_empty()
    || grant_id.len() > 128
    || !grant_id
      .bytes()
      .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'-'))
  {
    return Err("Invalid grant ID".into());
  }
  let mut lock_name = path.as_os_str().to_owned();
  lock_name.push(".lock");
  let lock_path = PathBuf::from(lock_name);
  write_new(&lock_path, b"").map_err(|e| format!("Revocation file is locked or unavailable: {e}"))?;
  struct RemoveOnDrop(PathBuf);
  impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
      let _ = fs::remove_file(&self.0);
    }
  }
  let _lock = RemoveOnDrop(lock_path);
  let mut revoked: Vec<String> = match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.is_file() && metadata.len() <= 1024 * 1024 => {
      let mut options = OpenOptions::new();
      options.read(true);
      #[cfg(unix)]
      {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
      }
      let file = options.open(path).map_err(|e| e.to_string())?;
      if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("Revocations must be a regular file".into());
      }
      let mut bytes = Vec::new();
      file
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
      if bytes.len() > 1024 * 1024 {
        return Err("Revocations file is too large".into());
      }
      serde_json::from_slice(&bytes).map_err(|e| format!("Invalid revocations file: {e}"))?
    }
    Ok(_) => return Err("Revocations must be a bounded regular file, not a symlink".into()),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
    Err(error) => return Err(error.to_string()),
  };
  if revoked.iter().any(|id| {
    id.is_empty()
      || id.len() > 128
      || !id
        .bytes()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'-'))
  }) {
    return Err("Invalid existing revoked grant ID".into());
  }
  if !revoked.contains(&grant_id) {
    revoked.push(grant_id.clone());
  }
  let bytes = serde_json::to_vec_pretty(&revoked).map_err(|e| e.to_string())?;
  if bytes.len() > 1024 * 1024 {
    return Err("Revocations file is full".into());
  }
  let mut temporary_name = path.as_os_str().to_owned();
  temporary_name.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
  let temporary = PathBuf::from(temporary_name);
  write_new(&temporary, &bytes)?;
  let _temporary = RemoveOnDrop(temporary.clone());
  fs::rename(&temporary, path).map_err(|e| format!("Could not update revocations: {e}"))?;
  println!("{}", json!({ "revoked_grant_id": grant_id }));
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn connect_defaults_to_pairing_and_legacy_control_requires_full_scope() {
    let connect = ["hub", "connect", "--hub", "https://hub.example", "--name", "host"];
    assert!(Args::try_parse_from(connect).is_ok());
    assert!(Args::try_parse_from(["hub", "connect"]).is_ok());
    assert!(Args::try_parse_from(["hub", "client", "--hub", "https://hub.example"]).is_ok());
    assert!(Args::try_parse_from(connect.into_iter().chain(["--trusted-hub"])).is_ok());
    assert!(Args::try_parse_from(connect.into_iter().chain(["--owner-public-key", "owner"])).is_ok());
    assert!(
      Args::try_parse_from(
        connect
          .into_iter()
          .chain(["--trusted-hub", "--owner-public-key", "owner"])
      )
      .is_err()
    );
    let grant = [
      "hub",
      "grant",
      "--owner-key-file",
      "owner.key",
      "--host-id",
      "host",
      "--host-public-key",
      "host-key",
      "--recipient-public-key",
      "recipient",
      "--out",
      "grant.json",
    ];
    assert!(Args::try_parse_from(grant).is_err());
    assert!(Args::try_parse_from(grant.into_iter().chain(["--all-sessions", "--allow-control"])).is_ok());
    assert!(Args::try_parse_from(grant.into_iter().chain(["--session-key", "one", "--allow-control"])).is_err());
    assert!(Args::try_parse_from(grant.into_iter().chain(["--session-key", "one", "--all-sessions"])).is_err());
  }

  #[test]
  fn revocation_preserves_existing_ids_and_rejects_unsafe_files() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("revocations.json");
    revoke(&path, "first".into()).unwrap();
    revoke(&path, "second".into()).unwrap();
    revoke(&path, "first".into()).unwrap();
    assert_eq!(
      serde_json::from_slice::<Vec<String>>(&fs::read(&path).unwrap()).unwrap(),
      ["first", "second"]
    );
    assert!(revoke(&path, "invalid/id".into()).is_err());
    fs::write(&path, "{}").unwrap();
    assert!(revoke(&path, "third".into()).is_err());
    #[cfg(unix)]
    {
      let link = directory.path().join("linked.json");
      std::os::unix::fs::symlink(&path, &link).unwrap();
      assert!(revoke(&link, "third".into()).is_err());
    }
  }
}
