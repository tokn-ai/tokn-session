//! Hub account discovery for the applet. Approval proofs and bearer tokens
//! stay inside the guest. Account sign-in never modifies machine trust.
use crate::{host, storage::canonical_hub};
use futures_util::lock::Mutex;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{cell::RefCell, collections::BTreeSet, rc::Rc};
use tokn_hub_client_core::{
  account::{DEVICE_TOKEN, DeviceAuthorization, DevicePurpose, VIEWER_START},
  address::{ResolvedMachine, parse_machine_address},
  protocol::{decode, valid_host_uuid},
};
use url::Url;
use zeroize::{Zeroize, Zeroizing};

const CAPABILITIES: &str = "/hub/v1/account/capabilities";
const ACCOUNT: &str = "/hub/v1/account/me";
const MACHINES: &str = "/hub/v1/account/machines";
const LOGOUT: &str = "/hub/v1/account/logout";
const MAX_ACCOUNT_BODY: usize = 4096;
const MAX_ROSTER_BODY: usize = 48 * 1024;
const MAX_MACHINES: usize = 64;
const CANCELED: &str = "Hub sign-in canceled";
const EXPIRED: &str = "Hub sign-in expired. Try again.";

thread_local! {
  static GATE: Rc<Mutex<()>> = Rc::new(Mutex::new(()));
  static ATTEMPT: RefCell<Option<Rc<Attempt>>> = const { RefCell::new(None) };
}

struct Attempt {
  hub: Url,
  id: String,
  poll: RefCell<Option<PollState>>,
}

struct PollState {
  authorization: DeviceAuthorization,
  expires_at: u64,
  interval: u64,
}

impl Attempt {
  fn active(self: &Rc<Self>) -> bool {
    ATTEMPT.with(|current| current.borrow().as_ref().is_some_and(|item| Rc::ptr_eq(self, item)))
  }

  fn finish(self: &Rc<Self>) {
    if self.active() {
      ATTEMPT.with(|current| current.borrow_mut().take());
    }
  }
}

#[derive(Deserialize, PartialEq, Eq, Serialize)]
struct Identity {
  user_id: String,
  username: String,
}

impl Identity {
  fn validate(&self) -> Result<(), String> {
    if !valid_host_uuid(&self.user_id) || parse_machine_address(&format!("{}:machine", self.username)).is_err() {
      return Err("Hub returned an invalid account identity".into());
    }
    Ok(())
  }
}

#[derive(Deserialize, Serialize)]
struct Login {
  #[serde(flatten)]
  account: Identity,
  token: String,
}

impl Login {
  fn validate(&self) -> Result<(), String> {
    self.account.validate()?;
    self
      .token
      .strip_prefix("tkna_")
      .filter(|value| value.len() == 43 && decode(value, 32).is_ok_and(|bytes| bytes.len() == 32))
      .ok_or("Invalid Hub account credential")?;
    Ok(())
  }
}

impl Drop for Login {
  fn drop(&mut self) {
    self.token.zeroize();
  }
}

#[derive(Deserialize)]
struct Capabilities {
  enabled: bool,
  #[serde(default)]
  viewer_device_login: bool,
}

#[derive(Deserialize)]
struct Roster {
  machines: Vec<ResolvedMachine>,
}

impl Roster {
  fn validate(&self, account: &Identity) -> Result<(), String> {
    let mut ids = BTreeSet::new();
    let mut addresses = BTreeSet::new();
    if self.machines.len() > MAX_MACHINES {
      return Err("Hub account machine list exceeds its size limit".into());
    }
    for machine in &self.machines {
      machine.validate_for(&machine.machine_address)?;
      let (username, _) = parse_machine_address(&machine.machine_address)?;
      if username != account.username || !ids.insert(&machine.host_id) || !addresses.insert(&machine.machine_address) {
        return Err("Hub returned an invalid account machine list".into());
      }
    }
    Ok(())
  }
}

#[derive(Deserialize)]
struct HttpHeader {
  name: String,
  value: String,
}

#[derive(Deserialize)]
struct HttpResponse {
  status: u16,
  headers: Vec<HttpHeader>,
  body: Vec<u8>,
}

impl HttpResponse {
  fn json<T: DeserializeOwned>(&self, maximum: usize) -> Result<T, String> {
    if self.status != 200 {
      return Err(account_error(self.status));
    }
    let json_type = self.headers.iter().any(|header| {
      header.name.eq_ignore_ascii_case("content-type")
        && header
          .value
          .split(';')
          .next()
          .is_some_and(|mime| mime.trim() == "application/json")
    });
    if !json_type || self.body.len() > maximum {
      return Err("Hub returned an unsupported or too large account response".into());
    }
    serde_json::from_slice(&self.body).map_err(|_| "Hub returned an invalid account response".into())
  }
}

impl Drop for HttpResponse {
  fn drop(&mut self) {
    self.body.zeroize();
  }
}

fn account_error(status: u16) -> String {
  match status {
    401 => "The Hub account sign-in expired. Sign in again.".into(),
    404 | 503 => "Account sign-in is unavailable on this Hub.".into(),
    429 => "Too many Hub account requests. Wait and try again.".into(),
    _ => format!("The Hub account request failed (HTTP {status})."),
  }
}

async fn request(hub: &Url, path: &str, body: Option<Value>, token: Option<&str>) -> Result<HttpResponse, String> {
  let mut url = hub.clone();
  url.set_path(path);
  let mut headers = vec![json!({"name":"Accept","value":"application/json"})];
  if let Some(token) = token {
    headers.push(json!({"name":"Authorization","value":format!("Bearer {token}")}));
  }
  let method = if body.is_some() {
    headers.push(json!({"name":"Content-Type","value":"application/json"}));
    "POST"
  } else {
    "GET"
  };
  let body = body.map(|body| serde_json::to_vec(&body).expect("serializable Hub account payload"));
  // Apell's HTTP service does not follow redirects. The origin is canonical
  // and tokens are attached only to fixed account endpoints on that origin.
  host::call(
    "http.request",
    json!({"url":url.as_str(),"method":method,"headers":headers,"body":body,"timeout_ms":10000}),
  )
  .await
  .map_err(|_| "Could not reach the Hub account service.".into())
}

fn key(hub: &Url) -> String {
  format!("hub-account-{:x}", Sha256::digest(hub.as_str().as_bytes()))
}

async fn load(hub: &Url) -> Result<Option<Login>, String> {
  let raw = apell_guest::services::secret(&key(hub))
    .await
    .map_err(|_| "Could not load saved Hub sign-in")?;
  let Some(raw) = raw else {
    return Ok(None);
  };
  let raw = Zeroizing::new(raw);
  if raw.len() > MAX_ACCOUNT_BODY {
    return Err("Saved Hub sign-in exceeds its size limit".into());
  }
  let login: Login = serde_json::from_str(&raw).map_err(|_| "Saved Hub sign-in is invalid")?;
  login.validate()?;
  Ok(Some(login))
}

async fn remove(hub: &Url) -> Result<(), String> {
  apell_guest::services::delete_secret(&key(hub))
    .await
    .map_err(|_| "Could not remove saved Hub sign-in".into())
}

async fn revoke(hub: &Url, token: &str) -> Result<(), String> {
  let response = request(hub, LOGOUT, Some(json!({})), Some(token)).await?;
  if !matches!(response.status, 204 | 401) {
    return Err(account_error(response.status));
  }
  Ok(())
}

fn attempt_id(params: &Value) -> Result<&str, String> {
  let id = host::string(params, "attempt_id")?;
  if !valid_host_uuid(id) {
    return Err("Invalid Hub sign-in attempt".into());
  }
  Ok(id)
}

pub async fn status(hub: &str) -> Result<Value, String> {
  let hub = canonical_hub(hub)?;
  let reply = request(&hub, CAPABILITIES, None, None).await?;
  if reply.status == 404 {
    return Ok(json!({"supported":false}));
  }
  let capabilities: Capabilities = reply.json(MAX_ACCOUNT_BODY)?;
  let supported = capabilities.enabled && capabilities.viewer_device_login;
  if !supported {
    return Ok(json!({"supported":false}));
  }
  let gate = GATE.with(Clone::clone);
  let _guard = gate.lock().await;
  let Some(login) = load(&hub).await? else {
    return Ok(json!({"supported":true}));
  };
  let reply = request(&hub, ACCOUNT, None, Some(&login.token)).await?;
  if reply.status == 401 {
    remove(&hub).await?;
    return Ok(json!({"supported":true}));
  }
  let account: Identity = reply.json(MAX_ACCOUNT_BODY)?;
  account.validate()?;
  if account != login.account {
    return Err("The Hub returned a different account for this credential".into());
  }
  Ok(json!({"supported":true,"account":account}))
}

pub async fn start(params: Value) -> Result<Value, String> {
  let hub = canonical_hub(host::string(&params, "hub_url")?)?;
  let id = attempt_id(&params)?.to_owned();
  let attempt = Rc::new(Attempt {
    hub,
    id,
    poll: RefCell::new(None),
  });
  ATTEMPT.with(|current| current.replace(Some(attempt.clone())));
  let result = start_attempt(&attempt).await;
  if result.is_err() {
    attempt.finish();
  }
  result
}

async fn start_attempt(attempt: &Rc<Attempt>) -> Result<Value, String> {
  {
    let gate = GATE.with(Clone::clone);
    let _guard = gate.lock().await;
    if !attempt.active() {
      return Err(CANCELED.into());
    }
    if load(&attempt.hub).await?.is_some() {
      return Err("Sign out of your Hub account before signing in again".into());
    }
  }
  if !attempt.active() {
    return Err(CANCELED.into());
  }
  let authorization: DeviceAuthorization = request(&attempt.hub, VIEWER_START, Some(json!({})), None)
    .await?
    .json(MAX_ACCOUNT_BODY)?;
  let challenge = authorization.challenge(&attempt.hub, host::unix_time()?, DevicePurpose::Viewer)?;
  if !attempt.active() {
    return Err(CANCELED.into());
  }
  let interval = authorization.interval;
  *attempt.poll.borrow_mut() = Some(PollState {
    authorization,
    expires_at: challenge.expires_at,
    interval,
  });
  Ok(json!({
    "verification_url":challenge.verification_url,
    "user_code":challenge.user_code,
    "expires_at":challenge.expires_at,
    "interval_ms":interval * 1000
  }))
}

pub async fn poll(params: Value) -> Result<Value, String> {
  let hub = canonical_hub(host::string(&params, "hub_url")?)?;
  let id = attempt_id(&params)?;
  let attempt = ATTEMPT
    .with(|current| {
      current
        .borrow()
        .as_ref()
        .filter(|item| item.hub == hub && item.id == id)
        .cloned()
    })
    .ok_or(CANCELED)?;
  let state = attempt
    .poll
    .borrow_mut()
    .take()
    .ok_or("Hub sign-in is already checking approval")?;
  let result = poll_attempt(&attempt, state).await;
  if result.is_err() {
    attempt.finish();
  }
  result
}

async fn poll_attempt(attempt: &Rc<Attempt>, mut state: PollState) -> Result<Value, String> {
  if host::unix_time()? >= state.expires_at {
    return Err(EXPIRED.into());
  }
  let reply = request(
    &attempt.hub,
    DEVICE_TOKEN,
    Some(json!({"device_code":state.authorization.device_code})),
    None,
  )
  .await?;
  if matches!(reply.status, 202 | 429) {
    if !attempt.active() {
      return Err(CANCELED.into());
    }
    if reply.status == 429 {
      state.interval = (state.interval + 5).min(60);
    }
    let interval_ms = state.interval * 1000;
    *attempt.poll.borrow_mut() = Some(state);
    return Ok(json!({"interval_ms":interval_ms}));
  }
  if reply.status == 400 {
    return Err(EXPIRED.into());
  }
  let login: Login = reply.json(MAX_ACCOUNT_BODY)?;
  login.validate()?;
  if let Err(error) = save_approval(attempt, &login).await {
    let _ = revoke(&attempt.hub, &login.token).await;
    return Err(error);
  }
  Ok(json!({"account":login.account,"interval_ms":0}))
}

async fn save_approval(attempt: &Rc<Attempt>, login: &Login) -> Result<(), String> {
  let gate = GATE.with(Clone::clone);
  let _guard = gate.lock().await;
  if !attempt.active() {
    return Err(CANCELED.into());
  }
  if load(&attempt.hub).await?.is_some() {
    return Err("An account is already signed in to this Hub".into());
  }
  if !attempt.active() {
    return Err(CANCELED.into());
  }
  let encoded = Zeroizing::new(serde_json::to_string(login).map_err(|_| "Could not encode Hub sign-in")?);
  apell_guest::services::save_secret(&key(&attempt.hub), &encoded)
    .await
    .map_err(|_| "Could not save Hub sign-in securely")?;
  if !attempt.active() {
    remove(&attempt.hub).await?;
    return Err(CANCELED.into());
  }
  attempt.finish();
  Ok(())
}

pub fn cancel(params: Value) -> Result<Value, String> {
  let hub = canonical_hub(host::string(&params, "hub_url")?)?;
  let id = attempt_id(&params)?;
  cancel_for(&hub, Some(id));
  Ok(Value::Null)
}

fn cancel_for(hub: &Url, id: Option<&str>) {
  ATTEMPT.with(|current| {
    let mut current = current.borrow_mut();
    if current
      .as_ref()
      .is_some_and(|item| &item.hub == hub && id.is_none_or(|id| item.id == id))
    {
      current.take();
    }
  });
}

pub async fn machines(hub: &str) -> Result<Value, String> {
  let hub = canonical_hub(hub)?;
  let gate = GATE.with(Clone::clone);
  let _guard = gate.lock().await;
  let Some(login) = load(&hub).await? else {
    return Ok(json!({"machines":[]}));
  };
  let reply = request(&hub, MACHINES, None, Some(&login.token)).await?;
  if reply.status == 401 {
    remove(&hub).await?;
    return Ok(json!({"machines":[]}));
  }
  let roster: Roster = reply.json(MAX_ROSTER_BODY)?;
  roster.validate(&login.account)?;
  Ok(json!({"account":login.account,"machines":roster.machines}))
}

pub async fn logout(hub: &str) -> Result<Value, String> {
  let hub = canonical_hub(hub)?;
  cancel_for(&hub, None);
  let gate = GATE.with(Clone::clone);
  let _guard = gate.lock().await;
  if let Some(login) = load(&hub).await? {
    revoke(&hub, &login.token).await?;
    remove(&hub).await?;
  }
  Ok(Value::Null)
}
