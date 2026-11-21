//! GitHub is keyed by its immutable numeric account ID.

use super::{AccountError, AccountToken, Login, OptionalExtension, Serialize, SignIn, StatusCode, Store, federated};

#[cfg(test)]
use super::TokenTransport;

pub(in crate::accounts) struct Identity {
  pub(in crate::accounts) id: u64,
  pub(in crate::accounts) login: String,
  pub(in crate::accounts) verified_email: Option<String>,
}

impl Identity {
  fn credential(&self) -> Result<federated::Identity<'_>, AccountError> {
    let invalid = || AccountError::new(StatusCode::BAD_GATEWAY, "GitHub returned an invalid account identity");
    if self.id == 0
      || self.login.is_empty()
      || self.login.len() > 100
      || !self
        .login
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
      return Err(invalid());
    }
    Ok(federated::Identity {
      key: federated::Key::Github(i64::try_from(self.id).map_err(|_| invalid())?),
      login: Some(&self.login),
      verified_email: self.verified_email.as_deref(),
    })
  }
}

pub(in crate::accounts) fn sign_in(store: &Store, identity: &Identity) -> Result<SignIn, AccountError> {
  federated::sign_in(store, &identity.credential()?)
}

pub(in crate::accounts) fn sign_up(store: &Store, identity: &Identity, username: &str) -> Result<Login, AccountError> {
  federated::sign_up(store, &identity.credential()?, username)
}

pub(in crate::accounts) fn link(store: &Store, token: &AccountToken, identity: &Identity) -> Result<(), AccountError> {
  federated::link(store, token, &identity.credential()?)
}

#[derive(Serialize)]
pub(in crate::accounts) struct ConnectionInfo {
  github_login: String,
}

pub(in crate::accounts) fn connection(
  store: &Store,
  token: &AccountToken,
) -> Result<Option<ConnectionInfo>, AccountError> {
  let connection = store.connection().map_err(AccountError::internal)?;
  let identity = federated::require_browser(&connection, token)?;
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
