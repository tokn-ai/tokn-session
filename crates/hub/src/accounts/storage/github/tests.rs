use super::*;
use crate::accounts::hash;
use crate::accounts::storage::{credential, email_login, logout};

const EMAIL: &str = "person@example.test";
const OTHER_EMAIL: &str = "other@example.test";

fn fixture() -> (tempfile::TempDir, Store) {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path().join("hub.sqlite")).unwrap();
  super::super::initialize(&store).unwrap();
  (directory, store)
}

fn github(id: u64, login: &str, email: Option<&str>) -> Identity {
  Identity {
    id,
    login: login.into(),
    verified_email: email.map(str::to_owned),
  }
}

fn token(login: &Login, transport: TokenTransport) -> AccountToken {
  AccountToken {
    hash: hash(&login.token),
    from_cookie: transport == TokenTransport::Cookie,
    expected_user_id: Some(login.identity.user_id.clone()),
  }
}

fn signed_in(store: &Store, identity: &Identity) -> Login {
  match sign_in(store, identity).unwrap() {
    SignIn::SignedIn(login) => login,
    _ => panic!("expected linked GitHub account to sign in"),
  }
}

#[test]
fn matching_email_requires_an_explicit_link_and_preserves_the_hub_identity() {
  let (_directory, store) = fixture();
  let existing = email_login(&store, EMAIL, Some("existing"), TokenTransport::Cookie).unwrap();
  let existing_token = token(&existing, TokenTransport::Cookie);
  let identity = github(1234, "different-github-login", Some(EMAIL));

  assert!(matches!(sign_in(&store, &identity).unwrap(), SignIn::LinkRequired));
  assert_eq!(
    sign_up(&store, &identity, "unused").err().unwrap().status,
    StatusCode::CONFLICT
  );
  assert!(connection(&store, &existing_token).unwrap().is_none());

  let other = email_login(&store, OTHER_EMAIL, Some("other"), TokenTransport::Cookie).unwrap();
  let other_token = token(&other, TokenTransport::Cookie);
  assert_eq!(
    link(&store, &other_token, &identity).unwrap_err().status,
    StatusCode::CONFLICT
  );
  link(&store, &existing_token, &identity).unwrap();
  link(&store, &existing_token, &identity).unwrap();
  assert_eq!(
    link(&store, &other_token, &identity).unwrap_err().status,
    StatusCode::CONFLICT
  );
  assert!(connection(&store, &other_token).unwrap().is_none());

  // A GitHub rename or loss of verified email does not change the account key.
  let renamed = github(identity.id, "Renamed-GitHub", None);
  let browser = signed_in(&store, &renamed);
  assert_eq!(browser.identity.user_id, existing.identity.user_id);
  assert_eq!(browser.identity.username, "existing");
  let browser_token = token(&browser, TokenTransport::Cookie);
  assert_eq!(
    credential(&store, &browser_token).unwrap().identity.username,
    "existing"
  );
  assert_eq!(
    connection(&store, &existing_token).unwrap().unwrap().github_login,
    "Renamed-GitHub"
  );
  // Neither a reused login name nor a second verified email can impersonate the link.
  let different_id = github(2222, "Renamed-GitHub", Some(EMAIL));
  assert!(matches!(sign_in(&store, &different_id).unwrap(), SignIn::LinkRequired));
  assert_eq!(
    link(&store, &existing_token, &different_id).unwrap_err().status,
    StatusCode::CONFLICT
  );
}

#[test]
fn signup_requires_verified_email_and_a_free_hub_name_without_consuming_failed_attempts() {
  let (_directory, store) = fixture();
  let existing = email_login(&store, EMAIL, Some("reserved"), TokenTransport::Cookie).unwrap();
  let no_email = github(42, "reserved", None);
  assert_eq!(
    sign_in(&store, &no_email).err().unwrap().status,
    StatusCode::UNPROCESSABLE_ENTITY
  );
  assert_eq!(
    sign_up(&store, &no_email, "good-name").err().unwrap().status,
    StatusCode::UNPROCESSABLE_ENTITY
  );

  let identity = github(42, "reserved", Some(OTHER_EMAIL));
  assert!(matches!(sign_in(&store, &identity).unwrap(), SignIn::SignupRequired));
  assert_eq!(
    sign_up(&store, &identity, "reserved").err().unwrap().status,
    StatusCode::CONFLICT
  );
  let new_account = sign_up(&store, &identity, "new-hub-name").unwrap();
  assert_ne!(new_account.identity.user_id, existing.identity.user_id);
  assert_eq!(new_account.identity.username, "new-hub-name");
  let via_email = email_login(&store, OTHER_EMAIL, None, TokenTransport::Cookie).unwrap();
  assert_eq!(via_email.identity.user_id, new_account.identity.user_id);
  assert_eq!(
    signed_in(&store, &identity).identity.user_id,
    new_account.identity.user_id
  );
  assert_eq!(
    sign_up(&store, &identity, "duplicate").err().unwrap().status,
    StatusCode::CONFLICT
  );
}

#[test]
fn linking_checks_a_live_cookie_and_its_expected_hub_account() {
  let (_directory, store) = fixture();
  let identity = github(42, "github-login", None);
  let cookie = email_login(&store, EMAIL, Some("existing"), TokenTransport::Cookie).unwrap();
  let bearer = email_login(&store, EMAIL, None, TokenTransport::Bearer).unwrap();
  let bearer_token = token(&bearer, TokenTransport::Bearer);
  assert_eq!(
    link(&store, &bearer_token, &identity).unwrap_err().status,
    StatusCode::FORBIDDEN
  );

  let mut cookie_token = token(&cookie, TokenTransport::Cookie);
  cookie_token.expected_user_id = None;
  assert_eq!(
    link(&store, &cookie_token, &identity).unwrap_err().status,
    StatusCode::FORBIDDEN
  );
  cookie_token.expected_user_id = Some("other-account".to_owned());
  let error = link(&store, &cookie_token, &identity).unwrap_err();
  assert_eq!(error.status, StatusCode::CONFLICT);
  assert!(error.account_session_changed);

  let cookie_token = token(&cookie, TokenTransport::Cookie);
  logout(&store, &cookie_token).unwrap();
  assert_eq!(
    link(&store, &cookie_token, &identity).unwrap_err().status,
    StatusCode::UNAUTHORIZED
  );
  let fresh = email_login(&store, EMAIL, None, TokenTransport::Cookie).unwrap();
  link(&store, &token(&fresh, TokenTransport::Cookie), &identity).unwrap();
  assert_eq!(signed_in(&store, &identity).identity.user_id, cookie.identity.user_id);
}
