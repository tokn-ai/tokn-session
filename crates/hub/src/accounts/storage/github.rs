//! GitHub is an alternate sign-in method for a Hub identity. Provider email
//! can reserve a new account, but only the immutable GitHub ID resumes a link.

use super::{
  AccountError, AccountIdentity, AccountToken, Connection, Login, OptionalExtension, Serialize, StatusCode, Store,
  TokenTransport, TransactionBehavior, account_by_email, browser_account, create_account, finish_browser_login,
  identity_row, now, params,
};
use crate::accounts::mail::mailbox;

pub(in crate::accounts) struct Identity {
  pub(in crate::accounts) id: u64,
  pub(in crate::accounts) login: String,
  pub(in crate::accounts) verified_email: Option<String>,
}

pub(in crate::accounts) enum SignIn {
  SignedIn(Login),
  SignupRequired,
  LinkRequired,
}

#[derive(Serialize)]
pub(in crate::accounts) struct ConnectionInfo {
  github_login: String,
}

fn github_id(identity: &Identity) -> Result<i64, AccountError> {
  let invalid = || AccountError::new(StatusCode::BAD_GATEWAY, "GitHub returned an invalid account identity");
  if identity.id == 0
    || identity.login.is_empty()
    || identity.login.len() > 100
    || !identity
      .login
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
  {
    return Err(invalid());
  }
  i64::try_from(identity.id).map_err(|_| invalid())
}

fn signup_email(identity: &Identity) -> Result<String, AccountError> {
  identity.verified_email.as_deref().and_then(mailbox).ok_or_else(|| {
    AccountError::new(
      StatusCode::UNPROCESSABLE_ENTITY,
      "GitHub sign-up needs a verified primary email address",
    )
  })
}

fn by_github_id(connection: &Connection, github_id: i64) -> Result<Option<AccountIdentity>, AccountError> {
  connection
    .query_row(
      "SELECT a.user_id, a.username FROM hub_account_github g
       JOIN hub_accounts a ON a.user_id = g.user_id WHERE g.github_id = ?1",
      [github_id],
      identity_row,
    )
    .optional()
    .map_err(AccountError::internal)
}

fn require_browser(connection: &Connection, token: &AccountToken) -> Result<AccountIdentity, AccountError> {
  if !token.from_cookie || token.expected_user_id.is_none() {
    return Err(AccountError::forbidden());
  }
  browser_account(connection, token, now()?)
}

fn insert_connection(
  connection: &Connection,
  id: i64,
  owner: &AccountIdentity,
  login: &str,
) -> Result<(), AccountError> {
  let inserted = connection
    .execute(
      "INSERT INTO hub_account_github (github_id, user_id, github_login) VALUES (?1, ?2, ?3)
       ON CONFLICT DO NOTHING",
      params![id, owner.user_id, login],
    )
    .map_err(AccountError::internal)?;
  if inserted == 0 {
    return Err(AccountError::new(
      StatusCode::CONFLICT,
      "This Hub account or GitHub account already has a GitHub connection",
    ));
  }
  Ok(())
}

fn update_login(connection: &Connection, id: i64, login: &str) -> Result<(), AccountError> {
  connection
    .execute(
      "UPDATE hub_account_github SET github_login = ?2 WHERE github_id = ?1 AND github_login <> ?2",
      params![id, login],
    )
    .map_err(AccountError::internal)?;
  Ok(())
}

pub(in crate::accounts) fn sign_in(store: &Store, github: &Identity) -> Result<SignIn, AccountError> {
  let id = github_id(github)?;
  let mut connection = store.connection().map_err(AccountError::internal)?;
  let tx = connection
    .transaction_with_behavior(TransactionBehavior::Immediate)
    .map_err(AccountError::internal)?;
  if let Some(identity) = by_github_id(&tx, id)? {
    update_login(&tx, id, &github.login)?;
    return finish_browser_login(tx, identity, TokenTransport::Cookie).map(SignIn::SignedIn);
  }

  let email = signup_email(github)?;
  if account_by_email(&tx, &email)?.is_some() {
    return Ok(SignIn::LinkRequired);
  }
  Ok(SignIn::SignupRequired)
}

pub(in crate::accounts) fn sign_up(store: &Store, github: &Identity, username: &str) -> Result<Login, AccountError> {
  let id = github_id(github)?;
  let email = signup_email(github)?;
  let mut connection = store.connection().map_err(AccountError::internal)?;
  let tx = connection
    .transaction_with_behavior(TransactionBehavior::Immediate)
    .map_err(AccountError::internal)?;
  if by_github_id(&tx, id)?.is_some() {
    return Err(AccountError::new(
      StatusCode::CONFLICT,
      "This GitHub account is already linked. Sign in with GitHub again",
    ));
  }
  if account_by_email(&tx, &email)?.is_some() {
    return Err(AccountError::new(
      StatusCode::CONFLICT,
      "A Hub account already uses this email. Sign in by email, then connect GitHub from your account",
    ));
  }
  let identity = create_account(&tx, &email, username)?;
  insert_connection(&tx, id, &identity, &github.login)?;
  finish_browser_login(tx, identity, TokenTransport::Cookie)
}

pub(in crate::accounts) fn link(store: &Store, token: &AccountToken, github: &Identity) -> Result<(), AccountError> {
  let id = github_id(github)?;
  let mut connection = store.connection().map_err(AccountError::internal)?;
  let tx = connection
    .transaction_with_behavior(TransactionBehavior::Immediate)
    .map_err(AccountError::internal)?;
  let identity = require_browser(&tx, token)?;
  if let Some(owner) = by_github_id(&tx, id)? {
    if owner.user_id != identity.user_id {
      return Err(AccountError::new(
        StatusCode::CONFLICT,
        "This GitHub account is already connected to a Hub account",
      ));
    }
    update_login(&tx, id, &github.login)?;
    return tx.commit().map_err(AccountError::internal);
  }

  if let Some(email) = github.verified_email.as_deref().and_then(mailbox)
    && account_by_email(&tx, &email)?.is_some_and(|owner| owner.user_id != identity.user_id)
  {
    return Err(AccountError::new(
      StatusCode::CONFLICT,
      "Sign in to the Hub account for your GitHub email before connecting GitHub",
    ));
  }
  insert_connection(&tx, id, &identity, &github.login)?;
  tx.commit().map_err(AccountError::internal)
}

pub(in crate::accounts) fn connection(
  store: &Store,
  token: &AccountToken,
) -> Result<Option<ConnectionInfo>, AccountError> {
  let connection = store.connection().map_err(AccountError::internal)?;
  let identity = require_browser(&connection, token)?;
  connection
    .query_row(
      "SELECT github_login FROM hub_account_github WHERE user_id = ?1",
      [identity.user_id],
      |row| {
        Ok(ConnectionInfo {
          github_login: row.get(0)?,
        })
      },
    )
    .optional()
    .map_err(AccountError::internal)
}

#[cfg(test)]
mod tests;
