//! Provider identities add sign-in methods to a Hub account. A verified Apell
//! email may authorize a link after explicit browser confirmation.

use super::{
  AccountError, AccountIdentity, AccountToken, Connection, Login, OptionalExtension, StatusCode, Store, TokenTransport,
  TransactionBehavior, account_by_email, browser_account, create_account, finish_browser_login, identity_row, now,
  params,
};
use crate::accounts::mail::mailbox;

#[derive(Clone, Copy)]
pub(super) enum Key<'a> {
  Github(i64),
  Apell { issuer: &'a str, subject: &'a str },
}

pub(super) struct Identity<'a> {
  pub(super) key: Key<'a>,
  pub(super) login: Option<&'a str>,
  pub(super) verified_email: Option<&'a str>,
}

pub(in crate::accounts) enum SignIn {
  SignedIn(Login),
  SignupRequired,
  LinkRequired(AccountIdentity),
}

impl Key<'_> {
  fn owner(self, connection: &Connection) -> Result<Option<AccountIdentity>, AccountError> {
    match self {
      Self::Github(id) => connection.query_row(
        "SELECT a.user_id, a.username FROM hub_account_github g JOIN hub_accounts a ON a.user_id = g.user_id
         WHERE g.github_id = ?1",
        [id],
        identity_row,
      ),
      Self::Apell { issuer, subject } => connection.query_row(
        "SELECT a.user_id, a.username FROM hub_account_apell p JOIN hub_accounts a ON a.user_id = p.user_id
         WHERE p.issuer = ?1 AND p.subject = ?2",
        params![issuer, subject],
        identity_row,
      ),
    }
    .optional()
    .map_err(AccountError::internal)
  }
}

impl Identity<'_> {
  fn signup_email(&self) -> Result<String, AccountError> {
    self.verified_email.and_then(mailbox).ok_or_else(|| {
      AccountError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        match self.key {
          Key::Github(_) => "GitHub sign-up needs a verified primary email address",
          Key::Apell { .. } => "Apell Account sign-up needs a verified email address",
        },
      )
    })
  }

  fn insert(&self, connection: &Connection, owner: &AccountIdentity) -> Result<(), AccountError> {
    let inserted = match self.key {
      Key::Github(id) => connection.execute(
        "INSERT INTO hub_account_github (github_id, user_id, github_login) VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
        params![id, owner.user_id, self.login],
      ),
      Key::Apell { issuer, subject } => connection.execute(
        "INSERT INTO hub_account_apell (issuer, subject, user_id, login) VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
        params![issuer, subject, owner.user_id, self.login],
      ),
    }.map_err(AccountError::internal)?;
    if inserted == 0 {
      return Err(AccountError::new(
        StatusCode::CONFLICT,
        "An account already has a connection to this sign-in provider",
      ));
    }
    Ok(())
  }

  fn update_login(&self, connection: &Connection) -> Result<(), AccountError> {
    match self.key {
      Key::Github(id) => connection.execute(
        "UPDATE hub_account_github SET github_login = ?2 WHERE github_id = ?1 AND github_login <> ?2",
        params![id, self.login],
      ),
      Key::Apell { issuer, subject } => connection.execute(
        "UPDATE hub_account_apell SET login = ?3 WHERE issuer = ?1 AND subject = ?2 AND login IS NOT ?3",
        params![issuer, subject, self.login],
      ),
    }
    .map_err(AccountError::internal)?;
    Ok(())
  }
}

pub(super) fn require_browser(connection: &Connection, token: &AccountToken) -> Result<AccountIdentity, AccountError> {
  if !token.from_cookie || token.expected_user_id.is_none() {
    return Err(AccountError::forbidden());
  }
  browser_account(connection, token, now()?)
}

pub(super) fn sign_in(store: &Store, provider: &Identity<'_>) -> Result<SignIn, AccountError> {
  let mut connection = store.connection().map_err(AccountError::internal)?;
  let tx = connection
    .transaction_with_behavior(TransactionBehavior::Immediate)
    .map_err(AccountError::internal)?;
  if let Some(identity) = provider.key.owner(&tx)? {
    provider.update_login(&tx)?;
    return finish_browser_login(tx, identity, TokenTransport::Cookie).map(SignIn::SignedIn);
  }
  if let Some(matched) = account_by_email(&tx, &provider.signup_email()?)? {
    return Ok(SignIn::LinkRequired(matched));
  }
  Ok(SignIn::SignupRequired)
}

pub(super) fn confirm_email_link(
  store: &Store,
  provider: &Identity<'_>,
  expected_user_id: &str,
) -> Result<Login, AccountError> {
  let email = provider.signup_email()?;
  let mut connection = store.connection().map_err(AccountError::internal)?;
  let tx = connection
    .transaction_with_behavior(TransactionBehavior::Immediate)
    .map_err(AccountError::internal)?;
  let identity = account_by_email(&tx, &email)?
    .filter(|identity| identity.user_id == expected_user_id)
    .ok_or_else(|| {
      AccountError::new(
        StatusCode::CONFLICT,
        "The matching Hub account changed. Start sign-in again",
      )
    })?;

  if let Some(owner) = provider.key.owner(&tx)? {
    if owner.user_id != identity.user_id {
      return Err(provider_identity_in_use());
    }
    provider.update_login(&tx)?;
  } else {
    provider.insert(&tx, &identity)?;
  }
  finish_browser_login(tx, identity, TokenTransport::Cookie)
}

pub(super) fn sign_up(store: &Store, provider: &Identity<'_>, username: &str) -> Result<Login, AccountError> {
  let email = provider.signup_email()?;
  let mut connection = store.connection().map_err(AccountError::internal)?;
  let tx = connection
    .transaction_with_behavior(TransactionBehavior::Immediate)
    .map_err(AccountError::internal)?;
  if provider.key.owner(&tx)?.is_some() {
    return Err(AccountError::new(
      StatusCode::CONFLICT,
      "This account is already connected. Sign in again",
    ));
  }
  if account_by_email(&tx, &email)?.is_some() {
    return Err(AccountError::new(
      StatusCode::CONFLICT,
      "A Hub account already uses this email. Sign in to that account first",
    ));
  }
  let identity = create_account(&tx, &email, username)?;
  provider.insert(&tx, &identity)?;
  finish_browser_login(tx, identity, TokenTransport::Cookie)
}

pub(super) fn link(store: &Store, token: &AccountToken, provider: &Identity<'_>) -> Result<(), AccountError> {
  let mut connection = store.connection().map_err(AccountError::internal)?;
  let tx = connection
    .transaction_with_behavior(TransactionBehavior::Immediate)
    .map_err(AccountError::internal)?;
  let identity = require_browser(&tx, token)?;
  if let Some(owner) = provider.key.owner(&tx)? {
    if owner.user_id != identity.user_id {
      return Err(provider_identity_in_use());
    }
    provider.update_login(&tx)?;
    return tx.commit().map_err(AccountError::internal);
  }
  if let Some(email) = provider.verified_email.and_then(mailbox)
    && account_by_email(&tx, &email)?.is_some_and(|owner| owner.user_id != identity.user_id)
  {
    return Err(AccountError::new(
      StatusCode::CONFLICT,
      "Sign in to the Hub account for this email before connecting",
    ));
  }
  provider.insert(&tx, &identity)?;
  tx.commit().map_err(AccountError::internal)
}

fn provider_identity_in_use() -> AccountError {
  AccountError::new(
    StatusCode::CONFLICT,
    "This sign-in identity is already connected to a Hub account",
  )
}
