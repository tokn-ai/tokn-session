use super::*;
use crate::accounts::{TokenTransport, hash, storage};

const ISSUER: &str = "https://account.example.test/oidc";
const OTHER_ISSUER: &str = "https://other.example.test/oidc";
const EMAIL: &str = "owner@example.test";
const OTHER_EMAIL: &str = "other@example.test";

fn fixture() -> (tempfile::TempDir, Store) {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path().join("hub.sqlite")).unwrap();
  storage::initialize(&store).unwrap();
  (directory, store)
}

fn token(login: &Login) -> AccountToken {
  AccountToken {
    hash: hash(&login.token),
    from_cookie: true,
    expected_user_id: Some(login.identity.user_id.clone()),
  }
}

fn identity(issuer: &str, subject: &str, email: Option<&str>) -> Identity {
  Identity {
    issuer: issuer.into(),
    subject: subject.into(),
    login: None,
    verified_email: email.map(str::to_owned),
  }
}

fn existing(store: &Store, identity: &Identity) -> Login {
  match sign_in(store, identity).unwrap() {
    SignIn::SignedIn(login) => login,
    _ => panic!("expected an existing account"),
  }
}

#[test]
fn apell_email_match_requires_confirmation_and_preserves_the_hub_account() {
  let (_directory, store) = fixture();
  let owner = storage::email_login(&store, EMAIL, Some("existing"), TokenTransport::Cookie).unwrap();
  let token = token(&owner);
  let github = storage::github::Identity {
    id: 42,
    login: "github-user".into(),
    verified_email: Some(EMAIL.into()),
  };
  storage::github::link(&store, &token, &github).unwrap();
  let apell = identity(ISSUER, "subject", Some("owner@EXAMPLE.test"));
  assert!(matches!(
    sign_in(&store, &apell).unwrap(),
    SignIn::LinkRequired(matched) if matched.user_id == owner.identity.user_id && matched.username == "existing"
  ));
  assert!(connection(&store, &token, ISSUER).unwrap().is_none());
  assert_eq!(
    sign_up(&store, &apell, "another").err().unwrap().status,
    StatusCode::CONFLICT
  );
  assert_eq!(
    confirm_link(&store, &apell, "different-account").err().unwrap().status,
    StatusCode::CONFLICT
  );
  assert!(connection(&store, &token, ISSUER).unwrap().is_none());
  let signed_in = confirm_link(&store, &apell, &owner.identity.user_id).unwrap();
  assert_eq!(signed_in.identity.user_id, owner.identity.user_id);
  let metadata = connection(&store, &token, ISSUER).unwrap().unwrap();
  assert!(metadata.apell_login.is_none());
  assert!(storage::github::connection(&store, &token).unwrap().is_some());
  let later = Identity {
    login: Some("Apell user".into()),
    verified_email: None,
    ..apell
  };
  assert_eq!(existing(&store, &later).identity.user_id, owner.identity.user_id);
  assert_eq!(
    connection(&store, &token, ISSUER)
      .unwrap()
      .unwrap()
      .apell_login
      .as_deref(),
    Some("Apell user")
  );
}

#[test]
fn apell_confirmation_cannot_replace_another_provider_link() {
  let (_directory, store) = fixture();
  let owner = storage::email_login(&store, EMAIL, Some("first"), TokenTransport::Cookie).unwrap();
  let other = storage::email_login(&store, OTHER_EMAIL, Some("second"), TokenTransport::Cookie).unwrap();
  let pending = identity(ISSUER, "pending-subject", Some(EMAIL));
  let linked = identity(ISSUER, "linked-subject", Some(EMAIL));
  assert!(matches!(sign_in(&store, &pending).unwrap(), SignIn::LinkRequired(_)));
  link(&store, &token(&owner), &linked).unwrap();
  assert_eq!(
    confirm_link(&store, &pending, &owner.identity.user_id)
      .err()
      .unwrap()
      .status,
    StatusCode::CONFLICT
  );
  assert_eq!(existing(&store, &linked).identity.user_id, owner.identity.user_id);

  // A separate authenticated profile connected the same subject before consent.
  // An unverified email from that profile flow never established ownership.
  let other_link = identity(ISSUER, "pending-subject", None);
  link(&store, &token(&other), &other_link).unwrap();
  assert_eq!(
    confirm_link(&store, &pending, &owner.identity.user_id)
      .err()
      .unwrap()
      .status,
    StatusCode::CONFLICT
  );
  assert_eq!(existing(&store, &pending).identity.user_id, other.identity.user_id);
}

#[test]
fn apell_subject_is_scoped_to_its_issuer_and_new_accounts_need_verified_email() {
  let (_directory, store) = fixture();
  let first = identity(ISSUER, "same-subject", Some(EMAIL));
  let owner = sign_up(&store, &first, "first").unwrap();
  let other = identity(OTHER_ISSUER, "same-subject", Some(OTHER_EMAIL));
  assert!(matches!(sign_in(&store, &other).unwrap(), SignIn::SignupRequired));
  assert!(connection(&store, &token(&owner), OTHER_ISSUER).unwrap().is_none());
  let different_owner = sign_up(&store, &other, "second").unwrap();
  assert_ne!(different_owner.identity.user_id, owner.identity.user_id);
  assert_eq!(
    existing(&store, &other).identity.user_id,
    different_owner.identity.user_id
  );
  let no_email = identity(ISSUER, "unverified", None);
  assert_eq!(
    sign_in(&store, &no_email).err().unwrap().status,
    StatusCode::UNPROCESSABLE_ENTITY
  );
  assert_eq!(
    sign_up(&store, &no_email, "third").err().unwrap().status,
    StatusCode::UNPROCESSABLE_ENTITY
  );
  assert_eq!(
    confirm_link(&store, &no_email, &owner.identity.user_id)
      .err()
      .unwrap()
      .status,
    StatusCode::UNPROCESSABLE_ENTITY
  );
  assert_eq!(
    link(&store, &token(&different_owner), &first).unwrap_err().status,
    StatusCode::CONFLICT
  );
}
