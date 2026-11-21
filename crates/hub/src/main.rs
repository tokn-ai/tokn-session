use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
use std::{net::SocketAddr, path::PathBuf};
use tokio_util::sync::CancellationToken;
use tokn_session_hub::{
  client_onboarding::{self, PairedClientConfig},
  connector, host_setup, onboarding,
  secure::NoiseIdentity,
  server::{self, HubState},
  storage,
  store::Store,
};
use url::Url;

mod account_cli;
mod cli_onboarding;

#[derive(Parser)]
#[command(about = "Passwordless access to session hosts through one Hub")]
struct Args {
  #[command(subcommand)]
  command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum KeyKind {
  /// Encryption identity for a host or recipient client.
  Device,
}

#[derive(Default, clap::Args)]
struct HostArguments {
  #[arg(long)]
  hub: Option<Url>,
  #[arg(long)]
  name: Option<String>,
  #[arg(long)]
  viewer_url: Option<Url>,
  /// Stable Hub browser origin for host passkeys; defaults to --hub when it uses a hostname.
  #[arg(long)]
  passkey_origin: Option<String>,
  /// STUN URLs for direct WebRTC (comma-separated). Omit to keep saved settings;
  /// specify the flag without URLs to clear them. No public service is enabled by default.
  #[arg(long = "stun-server", value_delimiter = ',', num_args = 0..)]
  ice_servers: Option<Vec<String>>,
  /// Local configuration, keys, authenticator, and trusted-device state.
  #[arg(long)]
  state_dir: Option<PathBuf>,
  #[arg(long, env = "TOKN_VIEWER_TOKEN", hide_env_values = true)]
  viewer_token: Option<String>,
  /// Allow sending input to live agents. Use --allow-control=false to disable it again.
  #[arg(long, num_args = 0..=1, default_missing_value = "true")]
  allow_control: Option<bool>,
  /// Allow an unencrypted Hub URL only on loopback, for local development.
  #[arg(long)]
  insecure_loopback: bool,
}

impl HostArguments {
  fn into_options(self) -> Result<host_setup::HostOptions, String> {
    Ok(host_setup::HostOptions {
      hub: self.hub,
      name: self.name,
      viewer_url: self.viewer_url,
      passkey_origin: self.passkey_origin,
      ice_servers: self.ice_servers,
      state_dir: state_dir_or_default(self.state_dir)?,
      totp_secret_file: None,
      viewer_token: self.viewer_token,
      allow_control: self.allow_control,
      insecure_loopback: self.insecure_loopback,
    })
  }
}

#[derive(Subcommand)]
enum Command {
  /// Sign in to a Hub account and manage the saved CLI credential.
  Auth {
    #[command(subcommand)]
    command: account_cli::Command,
  },
  /// Move legacy Hub and desktop-client state into ~/.tokn/sessions.
  MigrateStorage {
    /// Confirm all Hub servers, connectors, and desktop apps are stopped.
    /// Old versions do not hold lifetime locks; stop background launchers too.
    #[arg(long)]
    offline: bool,
  },
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
  /// Register this computer with your Hub account, save its configuration, and exit.
  Register {
    /// Register this host under this machine name in your signed-in Hub account.
    #[arg(long, value_parser = machine_name)]
    machine: String,
    /// Import an existing Base32 authenticator secret from a private local file on first setup.
    #[arg(long)]
    totp_secret_file: Option<PathBuf>,
    /// Print the browser approval URL for use on another device.
    #[arg(long)]
    no_browser: bool,
    #[command(flatten)]
    host: HostArguments,
  },
  /// Start hosting, signing in and registering this computer when needed.
  Connect {
    /// Initial machine name. Existing machines keep their registered name.
    #[arg(long, value_parser = machine_name)]
    machine: Option<String>,
    /// Print the browser approval URL for use on another device.
    #[arg(long)]
    no_browser: bool,
    #[command(flatten)]
    host: HostArguments,
  },
  /// Change this computer's account address while keeping its identity and pairing.
  Rename {
    #[arg(long, value_parser = machine_name)]
    machine: String,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Print the browser approval URL for use on another device.
    #[arg(long)]
    no_browser: bool,
  },
  /// Open an E2EE host through a locally installed viewer on loopback.
  Client {
    #[arg(long)]
    hub: Option<Url>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Select a previously paired host; otherwise reopen the last selection.
    #[arg(long)]
    host: Option<String>,
    #[arg(long, default_value = "127.0.0.1:0")]
    bind: SocketAddr,
    #[arg(long, default_value = "apps/viewer/dist")]
    web_root: PathBuf,
    #[arg(long)]
    insecure_loopback: bool,
  },
  /// Show local authenticator setup, or import/export its secret for user-managed synchronization.
  Authenticator {
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
  /// Inspect or change remote runtime administration on this host. Existing
  /// paired clients still require host agent-control permission.
  RuntimeManagement {
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long, conflicts_with = "disable")]
    enable: bool,
    #[arg(long, conflicts_with = "enable")]
    disable: bool,
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
}

#[tokio::main]
async fn main() {
  if let Err(error) = run(Args::parse()).await {
    eprintln!("{error}");
    std::process::exit(1);
  }
}

fn default_path(file: &str) -> Result<PathBuf, String> {
  Ok(storage::default_state_dir()?.join(file))
}

fn state_dir_or_default(state_dir: Option<PathBuf>) -> Result<PathBuf, String> {
  match state_dir {
    Some(path) => Ok(path),
    None => storage::default_state_dir(),
  }
}

fn machine_name(value: &str) -> Result<String, String> {
  tokn_hub_client_core::address::parse_machine_address(&format!("account:{value}"))?;
  Ok(value.into())
}

async fn run(args: Args) -> Result<(), String> {
  let shutdown = CancellationToken::new();
  match args.command {
    Command::Auth { command } => account_cli::run(command).await,
    Command::MigrateStorage { offline } => {
      let paths = tokn_session_storage::SessionPaths::discover()?;
      let hub_dir = storage::migrate_legacy_storage(&paths, offline)?;
      let client_dir = storage::migrate_legacy_desktop_client(&paths, &storage::legacy_desktop_client_dir()?, offline)?;
      println!("{}", json!({ "hub_dir": hub_dir, "desktop_client_dir": client_dir }));
      Ok(())
    }
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
    Command::Register {
      machine,
      totp_secret_file,
      no_browser,
      host,
    } => {
      let mut options = host.into_options()?;
      let _lease = connector::HostLease::acquire(&options.state_dir.join("host-enrollment.key"))?;
      options.totp_secret_file = totp_secret_file;
      let config = cli_onboarding::prepare_host(options)?;
      let signal = shutdown_signal(shutdown.clone());
      let result = async {
        let account = account_cli::ensure_signed_in(&config.hub_url, &shutdown, no_browser).await?;
        if shutdown.is_cancelled() {
          return Err("Machine registration canceled".into());
        }
        let address = tokn_session_hub::account_host::claim(&config, &machine, &account.token).await?;
        eprintln!("Registered machine {address}");
        if shutdown.is_cancelled() {
          return Err("Machine registration canceled".into());
        }
        Ok(())
      }
      .await;
      signal.abort();
      result
    }
    Command::Connect {
      machine,
      no_browser,
      host,
    } => {
      let options = host.into_options()?;
      let lease = connector::HostLease::acquire(&options.state_dir.join("host-enrollment.key"))?;
      let state_dir = options.state_dir.clone();
      let config = cli_onboarding::prepare_host(options)?;
      let signal = shutdown_signal(shutdown.clone());
      let result = async {
        let host_id = &config.paired.host_id;
        match tokn_session_hub::account_host::saved_address(&state_dir, config.hub_url.as_str(), host_id)? {
          Some(address) => {
            if machine.as_ref().is_some_and(|name| address.split_once(':').map(|(_, saved)| saved) != Some(name.as_str())) {
              return Err("This computer already has a machine address. Use tokn-session-hub rename --machine <name> to change it".into());
            }
          }
          None => {
            let name = machine.unwrap_or_else(|| host_setup::suggest_machine_name(&config.name, host_id));
            let account = account_cli::ensure_signed_in(&config.hub_url, &shutdown, no_browser).await?;
            if shutdown.is_cancelled() { return Err("Machine registration canceled".into()); }
            let address = tokn_session_hub::account_host::claim(&config, &name, &account.token).await?;
            eprintln!("Registered machine {address}");
          }
        }
        if shutdown.is_cancelled() { return Err("Hosting canceled".into()); }
        connector::run_owned(config, shutdown.clone(), lease, None).await
      }.await;
      signal.abort();
      result
    }
    Command::Rename {
      machine,
      state_dir,
      no_browser,
    } => {
      let options = HostArguments {
        state_dir,
        ..Default::default()
      }
      .into_options()?;
      let _lease = connector::HostLease::acquire(&options.state_dir.join("host-enrollment.key"))?;
      let profile = onboarding::HostProfile::load(&options.state_dir.join("host.json"))?;
      host_setup::require_account_assignment(&options.state_dir, profile.as_ref(), None)?;
      let config = cli_onboarding::prepare_host(options)?;
      let signal = shutdown_signal(shutdown.clone());
      let result = async {
        let account = account_cli::ensure_signed_in(&config.hub_url, &shutdown, no_browser).await?;
        if shutdown.is_cancelled() {
          return Err("Machine rename canceled".into());
        }
        let address = tokn_session_hub::account_host::rename(&config, &machine, &account.token).await?;
        eprintln!("Renamed machine {address}");
        if shutdown.is_cancelled() {
          return Err("Machine rename canceled".into());
        }
        Ok(())
      }
      .await;
      signal.abort();
      result
    }
    Command::Client {
      hub,
      state_dir,
      host,
      bind,
      web_root,
      insecure_loopback,
    } => {
      let state_dir = state_dir_or_default(state_dir)?;
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
      result
    }
    Command::Authenticator {
      state_dir,
      import_file,
      export_file,
    } => {
      let state_dir = state_dir_or_default(state_dir)?;
      cli_onboarding::authenticator(&state_dir, import_file, export_file)
    }
    Command::Devices { state_dir } => {
      let state_dir = state_dir_or_default(state_dir)?;
      println!(
        "{}",
        serde_json::to_string_pretty(&onboarding::devices(&state_dir.join("host-access.json"))?)
          .map_err(|e| e.to_string())?
      );
      Ok(())
    }
    Command::RuntimeManagement {
      state_dir,
      enable,
      disable,
    } => {
      let state_dir = match state_dir {
        Some(path) => path,
        None => storage::inspect_state_dir()?,
      };
      let path = state_dir.join("host-access.json");
      if enable || disable {
        onboarding::set_runtime_management_enabled(&path, enable)?;
      }
      println!("{}", json!({"enabled":onboarding::runtime_management_enabled(&path)?}));
      Ok(())
    }
    Command::ForgetDevice { state_dir, public_key } => {
      let state_dir = state_dir_or_default(state_dir)?;
      onboarding::remove_device(&state_dir.join("host-access.json"), &public_key)?;
      println!("{}", json!({ "revoked_device": public_key }));
      Ok(())
    }
    Command::Keygen { kind, key_file } => {
      let (kind, public_key) = match kind {
        KeyKind::Device => ("device", NoiseIdentity::load_or_create(&key_file)?.public_key()),
      };
      println!("{}", json!({ "kind": kind, "public_key": public_key }));
      Ok(())
    }
  }
}

fn shutdown_signal(shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
  tokio::spawn(async move {
    let _ = tokio::signal::ctrl_c().await;
    shutdown.cancel();
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn explicit_state_paths_are_resolved_without_consulting_defaults() {
    let path = PathBuf::from("explicit/private/state");
    assert_eq!(state_dir_or_default(Some(path.clone())).unwrap(), path);
    assert!(Args::try_parse_from(["hub", "migrate-storage", "--offline"]).is_ok());
  }

  #[test]
  fn registration_and_hosting_are_distinct_commands() {
    let connect = ["hub", "connect", "--hub", "https://hub.example", "--name", "host"];
    assert!(Args::try_parse_from(connect).is_ok());
    assert!(Args::try_parse_from(["hub", "connect"]).is_ok());
    assert!(Args::try_parse_from(["hub", "client", "--hub", "https://hub.example"]).is_ok());
    let register = ["hub", "register", "--hub", "https://hub.example"];
    assert!(Args::try_parse_from(register).is_err());
    let registered = Args::try_parse_from(register.into_iter().chain([
      "--machine",
      "workstation",
      "--totp-secret-file",
      "seed.txt",
      "--allow-control",
    ]))
    .unwrap();
    let Command::Register {
      machine,
      totp_secret_file,
      host,
      ..
    } = registered.command
    else {
      panic!("Expected the registration command");
    };
    assert_eq!(machine, "workstation");
    assert_eq!(totp_secret_file, Some(PathBuf::from("seed.txt")));
    assert_eq!(host.allow_control, Some(true));
    assert!(Args::try_parse_from(register.into_iter().chain(["--machine", "invalid:name"])).is_err());
    assert!(Args::try_parse_from(connect.into_iter().chain(["--machine", "workstation"])).is_ok());
    assert!(Args::try_parse_from(connect.into_iter().chain(["--register-only"])).is_err());
    assert!(Args::try_parse_from(connect.into_iter().chain(["--totp-secret-file", "seed.txt"])).is_err());
    assert!(Args::try_parse_from(connect.into_iter().chain(["--trusted-hub"])).is_err());
    assert!(Args::try_parse_from(connect.into_iter().chain(["--namespace", "personal"])).is_err());
    assert!(Args::try_parse_from(connect.into_iter().chain(["--owner-public-key", "owner"])).is_err());
    assert!(Args::try_parse_from(["hub", "rename"]).is_err());
    assert!(Args::try_parse_from(["hub", "rename", "--machine", "studio"]).is_ok());
    assert!(Args::try_parse_from(["hub", "rename", "--machine", "invalid:name"]).is_err());
  }

  #[tokio::test]
  async fn connect_without_a_hub_does_not_create_identity_or_start_sign_in() {
    let directory = tempfile::tempdir().unwrap();
    let args = Args::try_parse_from(["hub", "connect", "--state-dir", directory.path().to_str().unwrap()]).unwrap();
    assert!(run(args).await.unwrap_err().contains("First setup requires --hub"));
    for name in [
      "host.json",
      "host-account.json",
      "host-enrollment.key",
      "host-noise.key",
      "host-access.json",
    ] {
      assert!(!directory.path().join(name).exists());
    }
  }

  #[test]
  fn only_current_pairing_access_commands_are_available() {
    assert!(Args::try_parse_from(["hub", "client", "--host", "saved-host"]).is_ok());
    assert!(Args::try_parse_from(["hub", "keygen", "--kind", "device", "--key-file", "device.key"]).is_ok());
    assert!(Args::try_parse_from(["hub", "authenticator"]).is_ok());
    for command in ["grant", "revoke"] {
      let error = Args::try_parse_from(["hub", command]).err().unwrap();
      assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }
    let error = Args::try_parse_from(["hub", "keygen", "--kind", "owner", "--key-file", "owner.key"])
      .err()
      .unwrap();
    assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
    for retired in [
      vec!["hub", "client", "--grant-file", "grant.json"],
      vec!["hub", "client", "--owner-public-key", "owner-key"],
      vec!["hub", "client", "--identity-file", "client.key"],
      vec!["hub", "authenticator", "--upgrade-sha256"],
    ] {
      let error = Args::try_parse_from(retired.clone()).err().unwrap();
      assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument, "{retired:?}");
    }
  }
}
