//! Read-only desktop Hub account bridge. OAuth happens on the Hub in the browser.
use std::{
  path::PathBuf,
  sync::{Mutex, MutexGuard},
};

use serde::Serialize;
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tokn_hub_client_core::address::ResolvedMachine;
use tokn_session_hub::account_client::{AccountClient, LoginChallenge, SignedInAccount};
use url::Url;
use uuid::Uuid;

const POLL_INTERVAL_MS: u32 = 1_000;
const CANCELED: &str = "Hub sign-in canceled";

pub struct DesktopHubAccount {
  client: AccountClient,
  active: Mutex<Option<Attempt>>,
}

struct Attempt {
  hub: Url,
  id: Uuid,
  cancel: CancellationToken,
  result: watch::Receiver<Option<Result<Identity, String>>>,
  challenge: Option<LoginChallenge>,
}

impl Drop for Attempt {
  fn drop(&mut self) {
    self.cancel.cancel();
  }
}

impl DesktopHubAccount {
  pub fn new(state_dir: PathBuf) -> Result<Self, String> {
    Ok(Self {
      client: AccountClient::new_viewer(state_dir)?,
      active: Mutex::new(None),
    })
  }

  fn active(&self) -> Result<MutexGuard<'_, Option<Attempt>>, String> {
    self
      .active
      .lock()
      .map_err(|_| "Could not read the Hub sign-in state".into())
  }

  pub fn cancel(&self) {
    if let Ok(mut active) = self.active() {
      active.take();
    }
  }
}

#[derive(Clone, Serialize)]
pub struct Identity {
  user_id: Uuid,
  username: String,
}

impl From<SignedInAccount> for Identity {
  fn from(value: SignedInAccount) -> Self {
    Self {
      user_id: value.user_id,
      username: value.username,
    }
  }
}

#[derive(Serialize)]
pub struct Status {
  supported: bool,
  account: Option<Identity>,
}

#[derive(Serialize)]
pub struct Roster {
  account: Option<Identity>,
  machines: Vec<ResolvedMachine>,
}

#[derive(Serialize)]
pub struct Started {
  #[serde(flatten)]
  challenge: LoginChallenge,
  interval_ms: u32,
}

#[derive(Serialize)]
pub struct Poll {
  account: Option<Identity>,
  interval_ms: u32,
}

fn matches(attempt: &Attempt, hub: &Url, id: Uuid) -> bool {
  attempt.hub == *hub && attempt.id == id
}

#[tauri::command(rename_all = "snake_case")]
pub async fn hub_account_status(state: State<'_, DesktopHubAccount>, hub_url: String) -> Result<Status, String> {
  let hub = tokn_hub_remote::canonical_hub(&hub_url)?;
  let supported = state.client.viewer_login_supported(&hub).await?;
  let account = if supported {
    state.client.saved_account(&hub).await?.map(Identity::from)
  } else {
    None
  };
  Ok(Status { supported, account })
}

#[tauri::command(rename_all = "snake_case")]
pub async fn hub_account_machines(state: State<'_, DesktopHubAccount>, hub_url: String) -> Result<Roster, String> {
  let hub = tokn_hub_remote::canonical_hub(&hub_url)?;
  if let Some((account, machines)) = state.client.viewer_roster(&hub).await? {
    return Ok(Roster {
      account: Some(account.into()),
      machines,
    });
  }
  Ok(Roster {
    account: None,
    machines: Vec::new(),
  })
}

#[tauri::command(rename_all = "snake_case")]
pub async fn hub_account_login_start(
  state: State<'_, DesktopHubAccount>,
  hub_url: String,
  attempt_id: Uuid,
) -> Result<Started, String> {
  let hub = tokn_hub_remote::canonical_hub(&hub_url)?;
  let cancel = CancellationToken::new();
  let (finished, result) = watch::channel(None);
  *state.active()? = Some(Attempt {
    hub: hub.clone(),
    id: attempt_id,
    cancel: cancel.clone(),
    result,
    challenge: None,
  });
  let client = state.client.clone();
  let worker_hub = hub.clone();
  let (approval, mut public_challenge) = mpsc::channel(1);
  tauri::async_runtime::spawn(async move {
    let approve = approval.clone();
    let result = async {
      if client.saved_account(&worker_hub).await?.is_some() {
        return Err("Sign out of your Hub account before signing in again".into());
      }
      client
        .sign_in(&worker_hub, &cancel, |challenge| {
          let _ = approve.try_send(Ok(challenge));
        })
        .await
        .map(Identity::from)
    }
    .await;
    if let Err(error) = &result {
      let _ = approval.try_send(Err(error.clone()));
    }
    finished.send_replace(Some(result));
  });
  let challenge = public_challenge.recv().await.unwrap_or_else(|| Err(CANCELED.into()));
  let mut active = state.active()?;
  let current = active
    .as_mut()
    .filter(|attempt| matches(attempt, &hub, attempt_id))
    .ok_or(CANCELED)?;
  match challenge {
    Ok(challenge) => {
      current.challenge = Some(challenge.clone());
      Ok(Started {
        challenge,
        interval_ms: POLL_INTERVAL_MS,
      })
    }
    Err(error) => {
      active.take();
      Err(error)
    }
  }
}

#[tauri::command(rename_all = "snake_case")]
pub fn hub_account_login_poll(
  state: State<'_, DesktopHubAccount>,
  hub_url: String,
  attempt_id: Uuid,
) -> Result<Poll, String> {
  let hub = tokn_hub_remote::canonical_hub(&hub_url)?;
  let mut active = state.active()?;
  let current = active
    .as_ref()
    .filter(|attempt| matches(attempt, &hub, attempt_id))
    .ok_or(CANCELED)?;
  let result = current.result.borrow().clone();
  if result.is_some() {
    active.take();
  }
  Ok(Poll {
    account: result.transpose()?,
    interval_ms: POLL_INTERVAL_MS,
  })
}

#[tauri::command(rename_all = "snake_case")]
pub fn hub_account_login_cancel(
  state: State<'_, DesktopHubAccount>,
  hub_url: String,
  attempt_id: Uuid,
) -> Result<(), String> {
  let hub = tokn_hub_remote::canonical_hub(&hub_url)?;
  let mut active = state.active()?;
  if active
    .as_ref()
    .is_some_and(|attempt| matches(attempt, &hub, attempt_id))
  {
    active.take();
  }
  Ok(())
}

#[tauri::command(rename_all = "snake_case")]
pub fn hub_account_login_open(
  app: AppHandle,
  state: State<'_, DesktopHubAccount>,
  hub_url: String,
  attempt_id: Uuid,
) -> Result<(), String> {
  let hub = tokn_hub_remote::canonical_hub(&hub_url)?;
  let address = state
    .active()?
    .as_ref()
    .filter(|attempt| matches(attempt, &hub, attempt_id))
    .and_then(|attempt| attempt.challenge.as_ref())
    .map(|challenge| challenge.verification_url.clone())
    .ok_or(CANCELED)?;
  app
    .opener()
    .open_url(address, None::<&str>)
    .map_err(|_| "Could not open the approval address in your browser".into())
}

#[tauri::command(rename_all = "snake_case")]
pub async fn hub_account_logout(state: State<'_, DesktopHubAccount>, hub_url: String) -> Result<(), String> {
  let hub = tokn_hub_remote::canonical_hub(&hub_url)?;
  state.client.logout(Some(&hub)).await.map(|_| ())
}
