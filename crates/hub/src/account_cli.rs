//! Terminal presentation for the shared native Hub account client.
use clap::Subcommand;
use std::{
  io::{IsTerminal, Read},
  process::{Command as ProcessCommand, Stdio},
};
use tokio_util::sync::CancellationToken;
use tokn_session_hub::{
  account_client::{AccountClient, LoginChallenge, SignedInAccount},
  storage,
};
use url::Url;
use zeroize::Zeroizing;

#[derive(Subcommand)]
pub(crate) enum Command {
  /// Sign in through the Hub website or supply a scoped API key.
  Login {
    /// Hub HTTPS origin. Loopback HTTP is available for development.
    #[arg(long)]
    hub: Url,
    /// Read an API key from standard input.
    #[arg(long)]
    with_token: bool,
    /// Print the approval URL for use on another device.
    #[arg(long, conflicts_with = "with_token")]
    no_browser: bool,
  },
  /// Verify the saved account credential with the Hub.
  Status {
    #[arg(long)]
    hub: Option<Url>,
  },
  /// Remove the saved credential and revoke a browser-issued native login.
  Logout {
    #[arg(long)]
    hub: Option<Url>,
  },
}

pub(crate) async fn run(command: Command) -> Result<(), String> {
  let client = AccountClient::new(storage::default_state_dir()?)?;
  match command {
    Command::Login {
      hub,
      with_token,
      no_browser,
    } => {
      let token = if with_token {
        Some(
          tokio::task::spawn_blocking(read_token)
            .await
            .map_err(|_| "Could not read API key")??,
        )
      } else {
        None
      };
      let cancel = CancellationToken::new();
      let signal = super::shutdown_signal(cancel.clone());
      let result = async {
        let account = if let Some(token) = token {
          client.login_with_token(&hub, token, &cancel).await?
        } else {
          client
            .sign_in(&hub, &cancel, |challenge| present_challenge(challenge, no_browser))
            .await?
        };
        if let Some(warning) = &account.warning {
          eprintln!("{warning}");
        }
        println!("Signed in to {hub} as {}", account.username);
        Ok(())
      }
      .await;
      signal.abort();
      result
    }
    Command::Status { hub } => {
      let (hub, account) = client.status(hub.as_ref()).await?;
      println!("Signed in to {hub} as {}", account.username);
      Ok(())
    }
    Command::Logout { hub } => {
      let hub = client.logout(hub.as_ref()).await?;
      println!("Signed out of {hub}");
      Ok(())
    }
  }
}

pub(crate) async fn ensure_signed_in(
  hub: &Url,
  cancel: &CancellationToken,
  no_browser: bool,
) -> Result<SignedInAccount, String> {
  let client = AccountClient::new(storage::default_state_dir()?)?;
  let account = client
    .ensure_signed_in(hub, cancel, |challenge| present_challenge(challenge, no_browser))
    .await?;
  if let Some(warning) = &account.warning {
    eprintln!("{warning}");
  }
  Ok(account)
}

fn present_challenge(challenge: LoginChallenge, no_browser: bool) {
  eprintln!(
    "Open {} and confirm code {}.",
    challenge.verification_url, challenge.user_code
  );
  if !no_browser {
    if let Ok(url) = Url::parse(&challenge.verification_url) {
      open_browser(&url);
    }
  }
}

fn read_token() -> Result<Zeroizing<String>, String> {
  const MAX_TOKEN_BYTES: usize = 1024;
  let stdin = std::io::stdin();
  if stdin.is_terminal() {
    return Err("Pipe the API key to standard input with --with-token.".into());
  }
  let mut input = Zeroizing::new(String::new());
  stdin
    .lock()
    .take(MAX_TOKEN_BYTES as u64 + 3)
    .read_to_string(&mut input)
    .map_err(|_| "Could not read API key from standard input")?;
  if input.len() > MAX_TOKEN_BYTES + 2 {
    return Err("Hub API key exceeds its size limit".into());
  }
  let token = input.trim_end_matches(['\r', '\n']);
  tokn_session_hub::account_client::validate_token(token)?;
  Ok(Zeroizing::new(token.into()))
}

fn open_browser(url: &Url) {
  #[cfg(target_os = "macos")]
  let mut opener = ProcessCommand::new("/usr/bin/open");
  #[cfg(target_os = "windows")]
  let mut opener = {
    let mut command = ProcessCommand::new("rundll32.exe");
    command.arg("url.dll,FileProtocolHandler");
    command
  };
  #[cfg(not(any(target_os = "macos", target_os = "windows")))]
  let mut opener = ProcessCommand::new("xdg-open");
  let opened = opener
    .arg(url.as_str())
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn();
  if let Ok(mut process) = opened {
    std::thread::spawn(move || {
      let _ = process.wait();
    });
  }
}
