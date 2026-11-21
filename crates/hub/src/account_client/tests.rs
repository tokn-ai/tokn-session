use super::*;
use axum::{
  Json, Router,
  extract::State,
  http::StatusCode,
  response::IntoResponse,
  routing::{get, post},
};
use std::sync::{
  Arc,
  atomic::{AtomicU16, AtomicUsize, Ordering},
};

fn credential(token: &str) -> Credential {
  Credential {
    kind: CredentialKind::DeviceLogin,
    token: token.into(),
  }
}

#[test]
fn a_stale_logout_cannot_remove_a_newer_credential_or_another_hub() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join(CREDENTIAL_FILE);
  let first = "https://first.example/";
  let second = "https://second.example/";
  save_credential(&path, first.into(), credential("old"), None).unwrap();
  save_credential(&path, second.into(), credential("other"), None).unwrap();
  let replaced = save_credential(&path, first.into(), credential("new"), None)
    .unwrap()
    .previous
    .unwrap();
  assert_eq!(replaced.token, "old");
  assert!(!forget_credential(&path, first, &credential("old")).unwrap());
  assert!(forget_credential(&path, first, &credential("new")).unwrap());
  let accounts = read_credentials(&path).unwrap();
  assert!(!accounts.credentials.contains_key(first));
  assert_eq!(accounts.selected.as_deref(), Some(second));
  assert_eq!(accounts.credentials.get(second).unwrap().token, "other");
}

#[test]
fn canceled_persistence_restores_the_prior_credential_without_replacing_a_newer_login() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join(CREDENTIAL_FILE);
  let origin = "https://hub.example/";
  let another = "https://another.example/";
  save_credential(&path, origin.into(), credential("original"), None).unwrap();
  save_credential(&path, another.into(), credential("other"), None).unwrap();
  let update = save_credential(&path, origin.into(), credential("canceled"), None).unwrap();
  restore_credential(&path, origin, "canceled", update).unwrap();
  let restored = read_credentials(&path).unwrap();
  assert_eq!(restored.credentials.get(origin).unwrap().token, "original");
  assert_eq!(restored.selected.as_deref(), Some(another));
  let stale = save_credential(&path, origin.into(), credential("canceled"), None).unwrap();
  save_credential(&path, origin.into(), credential("newer"), None).unwrap();
  restore_credential(&path, origin, "canceled", stale).unwrap();
  assert_eq!(
    read_credentials(&path).unwrap().credentials.get(origin).unwrap().token,
    "newer"
  );
}

#[derive(Clone)]
struct FixtureState {
  identity_status: Arc<AtomicU16>,
  challenge_starts: Arc<AtomicUsize>,
  polls: Arc<AtomicUsize>,
  logouts: Arc<AtomicUsize>,
  challenge_length: usize,
}

struct Fixture {
  hub: Url,
  state: FixtureState,
  task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
  fn drop(&mut self) {
    self.task.abort();
  }
}
impl Fixture {
  async fn start(status: u16, challenge_length: usize) -> Self {
    let state = FixtureState {
      identity_status: Arc::new(AtomicU16::new(status)),
      challenge_starts: Arc::new(AtomicUsize::new(0)),
      polls: Arc::new(AtomicUsize::new(0)),
      logouts: Arc::new(AtomicUsize::new(0)),
      challenge_length,
    };
    let app = Router::new()
      .route("/hub/v1/account/me", get(|State(state): State<FixtureState>| async move {
        let status = StatusCode::from_u16(state.identity_status.load(Ordering::Acquire)).unwrap();
        if status == StatusCode::OK { Json(identity()).into_response() } else { status.into_response() }
      }))
      .route("/hub/v1/account/device/start", post(|State(state): State<FixtureState>| async move {
        state.challenge_starts.fetch_add(1, Ordering::AcqRel);
        Json(json!({ "device_code": "private-device-token", "user_code": "A".repeat(state.challenge_length), "expires_in": 60, "interval": 1 }))
      }))
      .route("/hub/v1/account/device/token", post(|State(state): State<FixtureState>| async move {
        state.polls.fetch_add(1, Ordering::AcqRel);
        state.identity_status.store(200, Ordering::Release);
        let mut issued = identity();
        issued["token"] = json!("new-token");
        Json(issued)
      }))
      .route("/hub/v1/account/logout", post(|State(state): State<FixtureState>| async move {
        state.logouts.fetch_add(1, Ordering::AcqRel);
        StatusCode::NO_CONTENT
      }))
      .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move {
      axum::serve(listener, app).await.unwrap();
    });
    Self { hub, state, task }
  }
}
fn identity() -> serde_json::Value {
  json!({ "user_id": "550e8400-e29b-41d4-a716-446655440000", "username": "alice" })
}

#[tokio::test]
async fn expired_credentials_are_replaced_and_reused_by_another_native_client() {
  let fixture = Fixture::start(401, 8).await;
  let directory = tempfile::tempdir().unwrap();
  let client = AccountClient::new(directory.path().to_owned()).unwrap();
  save_credential(&client.path, fixture.hub.to_string(), credential("old-token"), None).unwrap();
  let announced = Arc::new(AtomicUsize::new(0));
  let count = announced.clone();
  let account = client
    .ensure_signed_in(&fixture.hub, &CancellationToken::new(), move |challenge| {
      let verification = Url::parse(&challenge.verification_url).unwrap();
      assert_eq!(verification.path(), "/account");
      assert_eq!(
        verification.query_pairs().next().unwrap(),
        ("user_code".into(), "AAAAAAAA".into())
      );
      assert_eq!(challenge.user_code, "AAAAAAAA");
      assert!(
        !serde_json::to_string(&challenge)
          .unwrap()
          .contains("private-device-token")
      );
      count.fetch_add(1, Ordering::AcqRel);
    })
    .await
    .unwrap();
  assert_eq!(account.username, "alice");
  assert_eq!(account.token.as_str(), "new-token");
  assert_eq!(announced.load(Ordering::Acquire), 1);
  assert_eq!(fixture.state.challenge_starts.load(Ordering::Acquire), 1);
  assert_eq!(fixture.state.logouts.load(Ordering::Acquire), 1);
  let another = AccountClient::new(directory.path().to_owned()).unwrap();
  let cached = another
    .ensure_signed_in(&fixture.hub, &CancellationToken::new(), |_| {
      panic!("cached credential should be reused")
    })
    .await
    .unwrap();
  assert_eq!(cached.token.as_str(), "new-token");
  assert_eq!(fixture.state.challenge_starts.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn an_outage_preserves_the_credential_without_starting_another_login() {
  let fixture = Fixture::start(503, 8).await;
  let directory = tempfile::tempdir().unwrap();
  let client = AccountClient::new(directory.path().to_owned()).unwrap();
  save_credential(&client.path, fixture.hub.to_string(), credential("existing"), None).unwrap();
  let before = std::fs::read(&client.path).unwrap();
  let result = client
    .ensure_signed_in(&fixture.hub, &CancellationToken::new(), |_| {
      panic!("outage must not start login")
    })
    .await;
  assert!(result.err().unwrap().contains("unavailable"));
  assert_eq!(std::fs::read(&client.path).unwrap(), before);
  assert_eq!(fixture.state.challenge_starts.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn canceling_the_challenge_prevents_polling_and_preserves_previous_login() {
  let fixture = Fixture::start(401, 8).await;
  let directory = tempfile::tempdir().unwrap();
  let client = AccountClient::new(directory.path().to_owned()).unwrap();
  save_credential(&client.path, fixture.hub.to_string(), credential("existing"), None).unwrap();
  let before = std::fs::read(&client.path).unwrap();
  let cancel = CancellationToken::new();
  let result = client
    .ensure_signed_in(&fixture.hub, &cancel, |_| cancel.cancel())
    .await;
  assert_eq!(result.err().unwrap(), CANCELED);
  assert_eq!(std::fs::read(&client.path).unwrap(), before);
  assert_eq!(fixture.state.polls.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn invalid_challenges_never_reach_the_presenter_or_disk() {
  let fixture = Fixture::start(200, 33).await;
  let directory = tempfile::tempdir().unwrap();
  let client = AccountClient::new(directory.path().to_owned()).unwrap();
  let result = client
    .sign_in(&fixture.hub, &CancellationToken::new(), |_| {
      panic!("invalid challenge must not be shown")
    })
    .await;
  assert!(result.err().unwrap().contains("invalid device"));
  assert!(!client.path.exists());
  assert_eq!(fixture.state.polls.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn failed_api_key_storage_does_not_revoke_the_supplied_key() {
  let fixture = Fixture::start(200, 8).await;
  let directory = tempfile::tempdir().unwrap();
  let client = AccountClient::new(directory.path().to_owned()).unwrap();
  std::fs::create_dir(&client.path).unwrap();
  let result = client
    .login_with_token(
      &fixture.hub,
      Zeroizing::new("supplied-key".into()),
      &CancellationToken::new(),
    )
    .await;
  assert!(result.is_err());
  assert_eq!(fixture.state.logouts.load(Ordering::Acquire), 0);
}
