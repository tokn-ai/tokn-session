use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use tokn_hub_client_core::address::parse_machine_address;
use uuid::Uuid;

use super::{
  AccountError, AccountIdentity, Hash, KeyRequest, Login, MACHINES_READ, MACHINES_REGISTER, hash, random_secret,
};
use crate::{secure_client::unix_time, store::Store};
use axum::http::StatusCode;

const READ_SCOPE: u8 = 1;
const REGISTER_SCOPE: u8 = 2;
const ALL_SCOPES: u8 = READ_SCOPE | REGISTER_SCOPE;
const BROWSER_TTL: i64 = 3600;
const CLI_TTL: i64 = 90 * 24 * 3600;
const MAX_KEY_TTL: i64 = 365 * 24 * 3600;
const MAX_ACCOUNTS: u32 = 4096;
const MAX_CREDENTIALS_PER_KIND: u32 = 32;

fn now() -> Result<i64, AccountError> {
  i64::try_from(unix_time().map_err(AccountError::internal)?).map_err(AccountError::internal)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CredentialKind {
  Browser,
  Cli,
  ApiKey,
}

impl CredentialKind {
  fn name(self) -> &'static str {
    match self {
      Self::Browser => "browser",
      Self::Cli => "cli",
      Self::ApiKey => "api_key",
    }
  }

  fn parse(name: &str) -> Result<Self, AccountError> {
    match name {
      "browser" => Ok(Self::Browser),
      "cli" => Ok(Self::Cli),
      "api_key" => Ok(Self::ApiKey),
      _ => Err(AccountError::internal("Invalid stored credential kind")),
    }
  }
}

pub(super) struct Credential {
  pub(super) identity: AccountIdentity,
  pub(super) kind: CredentialKind,
  scopes: u8,
}

impl Credential {
  pub(super) fn permits(&self, scope: &str) -> bool {
    scope_mask(scope).is_some_and(|mask| self.scopes & mask == mask)
  }
}

pub(super) fn initialize(store: &Store) -> Result<(), String> {
  store
    .connection()?
    .execute_batch(
      "CREATE TABLE IF NOT EXISTS hub_account_credentials (
         credential_id TEXT PRIMARY KEY,
         token_hash BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
         user_id TEXT NOT NULL REFERENCES hub_accounts(user_id),
         kind TEXT NOT NULL CHECK (kind IN ('browser', 'cli', 'api_key')),
         name TEXT,
         scopes INTEGER NOT NULL CHECK (scopes BETWEEN 1 AND 3),
         expires_at INTEGER NOT NULL
       );
       CREATE INDEX IF NOT EXISTS hub_account_credentials_owner
       ON hub_account_credentials(user_id, kind, expires_at);",
    )
    .map_err(|error| error.to_string())
}

fn scope_mask(scope: &str) -> Option<u8> {
  match scope {
    MACHINES_READ => Some(READ_SCOPE),
    MACHINES_REGISTER => Some(REGISTER_SCOPE),
    _ => None,
  }
}

fn scope_names(scopes: u8) -> Vec<&'static str> {
  [(READ_SCOPE, MACHINES_READ), (REGISTER_SCOPE, MACHINES_REGISTER)]
    .into_iter()
    .filter_map(|(mask, name)| (scopes & mask != 0).then_some(name))
    .collect()
}

fn authenticate(connection: &Connection, token: &Hash, now: i64) -> Result<Credential, AccountError> {
  let (user_id, username, kind, scopes): (String, String, String, u8) = connection
    .query_row(
      "SELECT a.user_id, a.username, c.kind, c.scopes FROM hub_account_credentials c
       JOIN hub_accounts a ON c.user_id = a.user_id
       WHERE c.token_hash = ?1 AND c.expires_at > ?2",
      params![token.as_slice(), now],
      |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )
    .optional()
    .map_err(AccountError::internal)?
    .ok_or_else(AccountError::unauthorized)?;
  Ok(Credential {
    identity: AccountIdentity { user_id, username },
    kind: CredentialKind::parse(&kind)?,
    scopes,
  })
}

pub(super) fn credential(store: &Store, token: &Hash) -> Result<Credential, AccountError> {
  let now = now()?;
  let connection = store.connection().map_err(AccountError::internal)?;
  authenticate(&connection, token, now)
}

fn browser_account(connection: &Connection, token: &Hash, now: i64) -> Result<AccountIdentity, AccountError> {
  let credential = authenticate(connection, token, now)?;
  if credential.kind != CredentialKind::Browser {
    return Err(AccountError::forbidden());
  }
  Ok(credential.identity)
}

fn issue(
  tx: &Transaction<'_>,
  user_id: &str,
  kind: CredentialKind,
  name: Option<&str>,
  scopes: u8,
  now: i64,
  lifetime: i64,
) -> Result<(String, String), AccountError> {
  tx.execute(
    "DELETE FROM hub_account_credentials WHERE user_id = ?1 AND expires_at <= ?2",
    params![user_id, now],
  )
  .map_err(AccountError::internal)?;
  let count: u32 = tx
    .query_row(
      "SELECT COUNT(*) FROM hub_account_credentials WHERE user_id = ?1 AND kind = ?2",
      params![user_id, kind.name()],
      |row| row.get(0),
    )
    .map_err(AccountError::internal)?;
  if count >= MAX_CREDENTIALS_PER_KIND {
    return Err(AccountError::new(
      StatusCode::CONFLICT,
      "Account credential limit reached",
    ));
  }
  let id = Uuid::new_v4().to_string();
  let token = format!("tkna_{}", random_secret());
  let hash = hash(&token);
  tx.execute(
    "INSERT INTO hub_account_credentials (credential_id, token_hash, user_id, kind, name, scopes, expires_at)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    params![id, hash.as_slice(), user_id, kind.name(), name, scopes, now + lifetime],
  )
  .map_err(AccountError::internal)?;
  Ok((id, token))
}

pub(super) fn email_login(store: &Store, email: &str, username: Option<&str>) -> Result<Login, AccountError> {
  let now = now()?;
  let mut connection = store.connection().map_err(AccountError::internal)?;
  let tx = connection
    .transaction_with_behavior(TransactionBehavior::Immediate)
    .map_err(AccountError::internal)?;
  let existing = tx
    .query_row(
      "SELECT user_id, username FROM hub_accounts WHERE email = ?1",
      [email],
      |row| {
        Ok(AccountIdentity {
          user_id: row.get(0)?,
          username: row.get(1)?,
        })
      },
    )
    .optional()
    .map_err(AccountError::internal)?;
  let identity = if let Some(identity) = existing {
    identity
  } else {
    let username = username.map(str::trim).ok_or(AccountError {
      status: StatusCode::CONFLICT,
      message: "Choose a username for your Hub account",
      username_required: true,
    })?;
    parse_machine_address(&format!("{username}:machine")).map_err(|_| {
      AccountError::new(
        StatusCode::BAD_REQUEST,
        "Use 1 to 63 lowercase letters, digits, or internal hyphens",
      )
    })?;
    let taken: bool = tx
      .query_row(
        "SELECT EXISTS (SELECT 1 FROM hub_accounts WHERE username = ?1)
         OR EXISTS (SELECT 1 FROM namespaces WHERE username = ?1)",
        [username],
        |row| row.get(0),
      )
      .map_err(AccountError::internal)?;
    if taken {
      return Err(AccountError::new(
        StatusCode::CONFLICT,
        "That username is already reserved",
      ));
    }
    let count: u32 = tx
      .query_row("SELECT COUNT(*) FROM hub_accounts", [], |row| row.get(0))
      .map_err(AccountError::internal)?;
    if count >= MAX_ACCOUNTS {
      return Err(AccountError::new(
        StatusCode::CONFLICT,
        "This Hub has reached its account limit",
      ));
    }
    let user_id = Uuid::new_v4().to_string();
    tx.execute(
      "INSERT INTO hub_accounts (user_id, username, email) VALUES (?1, ?2, ?3)",
      params![user_id, username, email],
    )
    .map_err(AccountError::internal)?;
    AccountIdentity {
      user_id,
      username: username.to_owned(),
    }
  };
  let (_, token) = issue(
    &tx,
    &identity.user_id,
    CredentialKind::Browser,
    None,
    ALL_SCOPES,
    now,
    BROWSER_TTL,
  )?;
  tx.commit().map_err(AccountError::internal)?;
  Ok(Login { identity, token })
}

pub(super) fn device_login(store: &Store, user_id: &str) -> Result<Login, AccountError> {
  let now = now()?;
  let mut connection = store.connection().map_err(AccountError::internal)?;
  let tx = connection
    .transaction_with_behavior(TransactionBehavior::Immediate)
    .map_err(AccountError::internal)?;
  let username = tx
    .query_row(
      "SELECT username FROM hub_accounts WHERE user_id = ?1",
      [user_id],
      |row| row.get(0),
    )
    .optional()
    .map_err(AccountError::internal)?
    .ok_or_else(AccountError::unauthorized)?;
  let (_, token) = issue(&tx, user_id, CredentialKind::Cli, None, ALL_SCOPES, now, CLI_TTL)?;
  tx.commit().map_err(AccountError::internal)?;
  Ok(Login {
    identity: AccountIdentity {
      user_id: user_id.to_owned(),
      username,
    },
    token,
  })
}

pub(super) fn logout(store: &Store, token: &Hash) -> Result<(), AccountError> {
  let connection = store.connection().map_err(AccountError::internal)?;
  connection
    .execute(
      "DELETE FROM hub_account_credentials WHERE token_hash = ?1",
      [token.as_slice()],
    )
    .map_err(AccountError::internal)?;
  Ok(())
}

#[derive(Serialize)]
pub(super) struct KeyMetadata {
  key_id: String,
  name: String,
  scopes: Vec<&'static str>,
  expires_at: i64,
}

#[derive(Serialize)]
pub(super) struct CreatedKey {
  #[serde(flatten)]
  metadata: KeyMetadata,
  token: String,
}

pub(super) fn keys(store: &Store, token: &Hash) -> Result<Vec<KeyMetadata>, AccountError> {
  let now = now()?;
  let connection = store.connection().map_err(AccountError::internal)?;
  let account = browser_account(&connection, token, now)?;
  let mut query = connection
    .prepare(
      "SELECT credential_id, name, scopes, expires_at FROM hub_account_credentials
       WHERE user_id = ?1 AND kind = 'api_key' AND expires_at > ?2 ORDER BY name, credential_id",
    )
    .map_err(AccountError::internal)?;
  query
    .query_map(params![account.user_id, now], |row| {
      Ok(KeyMetadata {
        key_id: row.get(0)?,
        name: row.get(1)?,
        scopes: scope_names(row.get(2)?),
        expires_at: row.get(3)?,
      })
    })
    .map_err(AccountError::internal)?
    .collect::<Result<Vec<_>, _>>()
    .map_err(AccountError::internal)
}

pub(super) fn create_key(store: &Store, token: &Hash, request: KeyRequest) -> Result<CreatedKey, AccountError> {
  let name = request.name.trim();
  if name.is_empty() || name.len() > 80 || name.chars().any(char::is_control) {
    return Err(AccountError::new(
      StatusCode::BAD_REQUEST,
      "Enter an API key name of up to 80 bytes",
    ));
  }
  if request.scopes.is_empty() || request.scopes.len() > 2 {
    return Err(AccountError::new(
      StatusCode::BAD_REQUEST,
      "Choose one or more API key scopes",
    ));
  }
  let mut scopes = 0;
  for scope in &request.scopes {
    scopes |= scope_mask(scope).ok_or_else(|| AccountError::new(StatusCode::BAD_REQUEST, "Unknown API key scope"))?;
  }
  let lifetime = match request.expires_in {
    Some(seconds) if (1..=MAX_KEY_TTL as u64).contains(&seconds) => seconds as i64,
    None => CLI_TTL,
    _ => {
      return Err(AccountError::new(
        StatusCode::BAD_REQUEST,
        "API keys must expire within one year",
      ));
    }
  };
  let now = now()?;
  let mut connection = store.connection().map_err(AccountError::internal)?;
  let tx = connection
    .transaction_with_behavior(TransactionBehavior::Immediate)
    .map_err(AccountError::internal)?;
  let account = browser_account(&tx, token, now)?;
  let (key_id, token) = issue(
    &tx,
    &account.user_id,
    CredentialKind::ApiKey,
    Some(name),
    scopes,
    now,
    lifetime,
  )?;
  tx.commit().map_err(AccountError::internal)?;
  Ok(CreatedKey {
    metadata: KeyMetadata {
      key_id,
      name: name.to_owned(),
      scopes: scope_names(scopes),
      expires_at: now + lifetime,
    },
    token,
  })
}

pub(super) fn revoke_key(store: &Store, token: &Hash, key_id: &str) -> Result<(), AccountError> {
  let now = now()?;
  let mut connection = store.connection().map_err(AccountError::internal)?;
  let tx = connection
    .transaction_with_behavior(TransactionBehavior::Immediate)
    .map_err(AccountError::internal)?;
  let account = browser_account(&tx, token, now)?;
  let changed = tx
    .execute(
      "DELETE FROM hub_account_credentials WHERE credential_id = ?1 AND user_id = ?2 AND kind = 'api_key'",
      params![key_id, account.user_id],
    )
    .map_err(AccountError::internal)?;
  if changed == 0 {
    return Err(AccountError::new(StatusCode::NOT_FOUND, "API key was not found"));
  }
  tx.commit().map_err(AccountError::internal)
}
