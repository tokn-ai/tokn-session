//! An Apell Account link is identified by the verified OIDC issuer and subject.

use super::{
  AccountError, AccountToken, Login, OptionalExtension, Serialize, SignIn, StatusCode, Store, federated, params,
};

pub(in crate::accounts) struct Identity {
  pub(in crate::accounts) issuer: String,
  pub(in crate::accounts) subject: String,
  pub(in crate::accounts) login: Option<String>,
  pub(in crate::accounts) verified_email: Option<String>,
}

impl Identity {
  fn credential(&self) -> Result<federated::Identity<'_>, AccountError> {
    if !(1..=2048).contains(&self.issuer.len())
      || !(1..=255).contains(&self.subject.len())
      || !self.subject.bytes().all(|byte| byte.is_ascii_graphic())
    {
      return Err(AccountError::new(
        StatusCode::BAD_GATEWAY,
        "Apell returned an invalid account identity",
      ));
    }
    Ok(federated::Identity {
      key: federated::Key::Apell {
        issuer: &self.issuer,
        subject: &self.subject,
      },
      login: self
        .login
        .as_deref()
        .filter(|name| !name.trim().is_empty() && name.len() <= 128 && !name.chars().any(char::is_control)),
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
  apell_login: Option<String>,
}

pub(in crate::accounts) fn connection(
  store: &Store,
  token: &AccountToken,
  issuer: &str,
) -> Result<Option<ConnectionInfo>, AccountError> {
  let connection = store.connection().map_err(AccountError::internal)?;
  let identity = federated::require_browser(&connection, token)?;
  connection
    .query_row(
      "SELECT login FROM hub_account_apell WHERE user_id = ?1 AND issuer = ?2",
      params![identity.user_id, issuer],
      |row| {
        Ok(ConnectionInfo {
          apell_login: row.get(0)?,
        })
      },
    )
    .optional()
    .map_err(AccountError::internal)
}

#[cfg(test)]
mod tests;
