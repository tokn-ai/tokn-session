//! Durable host approvals and the single owner's public passkey credentials.

use std::{
  fs::{self, OpenOptions},
  path::Path,
  sync::{Arc, Mutex, MutexGuard},
  time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use webauthn_rs::prelude::{AuthenticationResult, Passkey};

mod account_machines;
mod namespaces;
pub(crate) use account_machines::{ACCOUNT_REQUIRED, ADDRESS_TAKEN, AccountMachineClaim, HOST_CLAIMED};
pub use namespaces::{HostCatalogEntry, MachineRecord, NamespaceError};

const MAX_PASSKEYS: u32 = 32;
const MAX_REGISTERED_HOSTS: u32 = 64;
const MAX_REGISTRATION_LEDGER: u32 = MAX_REGISTERED_HOSTS + 1024;
const REGISTRATION_WINDOW: Duration = Duration::from_secs(60);
const NEW_REGISTRATIONS_PER_WINDOW: u32 = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRecord {
  pub host_id: String,
  pub name: String,
  pub public_key: String,
  /// The maximum access approved at enrollment: `view` or `control`.
  pub access: String,
}

#[derive(Clone)]
pub struct Store {
  connection: Arc<Mutex<Connection>>,
  registrations: Arc<Mutex<RegistrationBudget>>,
}

enum HostRegistration<'a> {
  Reconnect,
  Claim { user_id: &'a str, machine_name: &'a str },
}

struct RegistrationBudget {
  window_start: Instant,
  used: u32,
}

impl RegistrationBudget {
  fn reserve(&mut self) -> Result<(), String> {
    let now = Instant::now();
    if now.duration_since(self.window_start) >= REGISTRATION_WINDOW {
      self.window_start = now;
      self.used = 0;
    }
    if self.used >= NEW_REGISTRATIONS_PER_WINDOW {
      return Err("New host registration rate limit reached; retry in one minute".into());
    }
    self.used += 1;
    Ok(())
  }
}

impl Store {
  pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
    let path = path.as_ref();
    prepare_private_file(path)?;
    let connection = Connection::open(path).map_err(database_error)?;
    connection
      .busy_timeout(Duration::from_secs(5))
      .map_err(database_error)?;
    connection
      .execute_batch(
        "PRAGMA journal_mode = DELETE;
         PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS metadata (
           key TEXT PRIMARY KEY,
           value TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS passkeys (
           credential_id TEXT PRIMARY KEY,
           credential_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS hosts (
           host_id TEXT PRIMARY KEY,
           name TEXT NOT NULL,
           public_key TEXT NOT NULL UNIQUE,
           access TEXT NOT NULL CHECK (access IN ('view', 'control'))
         );
         CREATE TABLE IF NOT EXISTS encrypted_hosts (
           host_id TEXT PRIMARY KEY,
           public_key TEXT NOT NULL UNIQUE,
           revoked INTEGER NOT NULL DEFAULT 0 CHECK (revoked IN (0, 1))
         );
         CREATE TABLE IF NOT EXISTS hub_accounts (
           user_id TEXT PRIMARY KEY,
           username TEXT NOT NULL UNIQUE,
           email TEXT NOT NULL UNIQUE
         );
         CREATE TABLE IF NOT EXISTS account_discovery (
           user_id TEXT PRIMARY KEY REFERENCES hub_accounts(user_id),
           secret_digest TEXT NOT NULL UNIQUE
         );
         CREATE TABLE IF NOT EXISTS account_machines (
           host_id TEXT PRIMARY KEY REFERENCES encrypted_hosts(host_id),
           user_id TEXT NOT NULL REFERENCES hub_accounts(user_id),
           machine_name TEXT NOT NULL,
           UNIQUE(user_id, machine_name)
         );",
      )
      .map_err(database_error)?;
    connection
      .execute(
        "INSERT OR IGNORE INTO metadata (key, value) VALUES ('owner_id', ?1)",
        [Uuid::new_v4().to_string()],
      )
      .map_err(database_error)?;
    Ok(Self {
      connection: Arc::new(Mutex::new(connection)),
      registrations: Arc::new(Mutex::new(RegistrationBudget {
        window_start: Instant::now(),
        used: 0,
      })),
    })
  }

  pub(crate) fn connection(&self) -> Result<MutexGuard<'_, Connection>, String> {
    self
      .connection
      .lock()
      .map_err(|_| "Hub database lock poisoned".to_owned())
  }

  pub(crate) fn owner_id(&self) -> Result<Uuid, String> {
    let value: String = self
      .connection()?
      .query_row("SELECT value FROM metadata WHERE key = 'owner_id'", [], |row| {
        row.get(0)
      })
      .map_err(database_error)?;
    Uuid::parse_str(&value).map_err(|_| "Invalid Hub owner identity in database".to_owned())
  }

  pub(crate) fn configured(&self) -> Result<bool, String> {
    self
      .connection()?
      .query_row("SELECT EXISTS (SELECT 1 FROM passkeys)", [], |row| row.get(0))
      .map_err(database_error)
  }

  pub(crate) fn passkeys(&self) -> Result<Vec<Passkey>, String> {
    let connection = self.connection()?;
    read_passkeys(&connection)
  }

  /// The empty-account condition is checked inside the write transaction, so two
  /// simultaneous first registrations cannot both claim this installation.
  pub(crate) fn register_passkey(&self, passkey: &Passkey, first: bool) -> Result<(), String> {
    let credential_id = URL_SAFE_NO_PAD.encode(passkey.cred_id());
    let credential_json = serde_json::to_string(passkey).map_err(|error| error.to_string())?;
    let mut connection = self.connection()?;
    let transaction = connection
      .transaction_with_behavior(TransactionBehavior::Immediate)
      .map_err(database_error)?;
    let count: u32 = transaction
      .query_row("SELECT COUNT(*) FROM passkeys", [], |row| row.get(0))
      .map_err(database_error)?;
    if first && count != 0 {
      return Err("Hub owner is already configured".to_owned());
    }
    if !first && count == 0 {
      return Err("Hub owner is not configured".to_owned());
    }
    if count >= MAX_PASSKEYS {
      return Err("Maximum number of owner passkeys reached".to_owned());
    }
    transaction
      .execute(
        "INSERT INTO passkeys (credential_id, credential_json) VALUES (?1, ?2)",
        params![credential_id, credential_json],
      )
      .map_err(database_error)?;
    transaction.commit().map_err(database_error)
  }

  pub(crate) fn update_passkey(&self, result: &AuthenticationResult) -> Result<(), String> {
    let mut connection = self.connection()?;
    let transaction = connection
      .transaction_with_behavior(TransactionBehavior::Immediate)
      .map_err(database_error)?;
    let mut matched = false;
    for mut passkey in read_passkeys(&transaction)? {
      if let Some(changed) = passkey.update_credential(result) {
        matched = true;
        if changed {
          let credential_json = serde_json::to_string(&passkey).map_err(|error| error.to_string())?;
          transaction
            .execute(
              "UPDATE passkeys SET credential_json = ?1 WHERE credential_id = ?2",
              params![credential_json, URL_SAFE_NO_PAD.encode(passkey.cred_id())],
            )
            .map_err(database_error)?;
        }
        break;
      }
    }
    if !matched {
      return Err("Passkey is no longer registered".to_owned());
    }
    transaction.commit().map_err(database_error)
  }

  #[cfg(test)]
  pub(crate) fn hosts(&self) -> Result<Vec<HostRecord>, String> {
    let connection = self.connection()?;
    let mut statement = connection
      .prepare(
        "SELECT h.host_id, h.name, h.public_key, h.access FROM hosts h
         JOIN encrypted_hosts e ON e.host_id = h.host_id AND e.public_key = h.public_key
         JOIN account_machines m ON m.host_id = h.host_id
         JOIN hub_accounts a ON a.user_id = m.user_id
         WHERE e.revoked = 0 ORDER BY h.name, h.host_id",
      )
      .map_err(database_error)?;
    statement
      .query_map([], host_from_row)
      .map_err(database_error)?
      .collect::<Result<Vec<_>, _>>()
      .map_err(database_error)
  }

  pub fn host(&self, host_id: &str) -> Result<Option<HostRecord>, String> {
    self
      .connection()?
      .query_row(
        "SELECT h.host_id, h.name, h.public_key, h.access FROM hosts h
         JOIN encrypted_hosts e ON e.host_id = h.host_id AND e.public_key = h.public_key
         JOIN account_machines m ON m.host_id = h.host_id
         JOIN hub_accounts a ON a.user_id = m.user_id
         WHERE h.host_id = ?1 AND e.revoked = 0",
        [host_id],
        host_from_row,
      )
      .optional()
      .map_err(database_error)
  }

  pub fn remove_host(&self, host_id: &str) -> Result<(), String> {
    let mut connection = self.connection()?;
    let transaction = connection
      .transaction_with_behavior(TransactionBehavior::Immediate)
      .map_err(database_error)?;
    transaction
      .execute("UPDATE encrypted_hosts SET revoked = 1 WHERE host_id = ?1", [host_id])
      .map_err(database_error)?;
    transaction
      .execute("DELETE FROM hosts WHERE host_id = ?1", [host_id])
      .map_err(database_error)?;
    transaction.commit().map_err(database_error)
  }

  /// Seed an older, unclaimed encrypted registration for rejection tests.
  #[cfg(test)]
  pub(crate) fn register_encrypted_host(&self, host: HostRecord) -> Result<HostRecord, String> {
    let mut connection = self.connection()?;
    let transaction = connection.transaction().map_err(database_error)?;
    transaction
      .execute(
        "INSERT INTO hosts (host_id, name, public_key, access) VALUES (?1, ?2, ?3, ?4)",
        params![host.host_id, host.name, host.public_key, host.access],
      )
      .map_err(database_error)?;
    transaction
      .execute(
        "INSERT INTO encrypted_hosts (host_id, public_key) VALUES (?1, ?2)",
        params![host.host_id, host.public_key],
      )
      .map_err(database_error)?;
    transaction.commit().map_err(database_error)?;
    Ok(host)
  }

  #[cfg(test)]
  pub(crate) fn claim_fixture_host(&self, host: HostRecord) -> AccountMachineClaim {
    let machine_name = host.host_id.clone();
    self
      .claim_account_host(host, fixture_account(self), &machine_name)
      .unwrap()
  }

  pub(crate) fn register_tunnel_host(
    &self,
    host: HostRecord,
    discovery_group: Option<&tokn_hub_client_core::discovery::DiscoveryGroup>,
  ) -> Result<HostRecord, String> {
    if let Some(group) = discovery_group {
      group.validate()?;
    }
    self
      .register_host_inner(host, HostRegistration::Reconnect)
      .map(|(host, _)| host)
  }

  pub(crate) fn claim_account_host(
    &self,
    host: HostRecord,
    user_id: &str,
    machine_name: &str,
  ) -> Result<AccountMachineClaim, String> {
    self
      .register_host_inner(host, HostRegistration::Claim { user_id, machine_name })?
      .1
      .ok_or_else(|| "Missing account machine assignment".into())
  }

  fn register_host_inner(
    &self,
    host: HostRecord,
    admission: HostRegistration<'_>,
  ) -> Result<(HostRecord, Option<AccountMachineClaim>), String> {
    if !crate::protocol::valid_host_uuid(&host.host_id)
      || host.name.trim().is_empty()
      || host.name.len() > 128
      || host.name.chars().any(char::is_control)
      || !matches!(host.access.as_str(), "view" | "control")
    {
      return Err("Invalid encrypted host registration".into());
    }
    crate::secure::decode_public_key(&host.public_key)?;
    let mut connection = self.connection()?;
    let transaction = connection
      .transaction_with_behavior(TransactionBehavior::Immediate)
      .map_err(database_error)?;
    let registration: Option<(String, bool)> = transaction
      .query_row(
        "SELECT public_key, revoked FROM encrypted_hosts WHERE host_id = ?1",
        [&host.host_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
      )
      .optional()
      .map_err(database_error)?;
    if let Some((public_key, revoked)) = &registration {
      if *revoked || public_key != &host.public_key {
        return Err("Host UUID registration is revoked or belongs to a different identity".into());
      }
    }
    let claimed = registration.is_some() && account_machines::is_owned(&transaction, &host.host_id)?;
    if !claimed && matches!(admission, HostRegistration::Reconnect) {
      return Err(ACCOUNT_REQUIRED.into());
    }
    if !claimed {
      let active: u32 = transaction
        .query_row(
          "SELECT COUNT(*) FROM account_machines m
           JOIN encrypted_hosts e ON e.host_id = m.host_id
           WHERE e.revoked = 0",
          [],
          |row| row.get(0),
        )
        .map_err(database_error)?;
      if active >= MAX_REGISTERED_HOSTS {
        return Err("Encrypted host registration capacity reached; a Hub administrator can revoke unused hosts".into());
      }
    }
    let existing = transaction
      .query_row(
        "SELECT host_id, name, public_key, access FROM hosts WHERE host_id = ?1",
        [&host.host_id],
        host_from_row,
      )
      .optional()
      .map_err(database_error)?;
    if let Some(existing) = existing {
      if registration.is_none() || existing.public_key != host.public_key {
        return Err("Host UUID is registered to a different identity".into());
      }
      if !matches!(admission, HostRegistration::Claim { .. }) {
        transaction
          .execute(
            "UPDATE hosts SET name = ?2, access = ?3 WHERE host_id = ?1",
            params![host.host_id, host.name, host.access],
          )
          .map_err(database_error)?;
      }
    } else {
      let total: u32 = transaction
        .query_row("SELECT COUNT(*) FROM encrypted_hosts", [], |row| row.get(0))
        .map_err(database_error)?;
      // Tombstones prevent revoked UUIDs from reclaiming their routes. Keep a
      // separate finite ledger so ordinary revocation reclaims active capacity
      // without erasing that protection or allowing unbounded database growth.
      if total >= MAX_REGISTRATION_LEDGER {
        return Err("Encrypted host revocation ledger is full; Hub administrator maintenance is required before registering new hosts".into());
      }
      transaction
        .execute(
          "INSERT INTO hosts (host_id, name, public_key, access) VALUES (?1, ?2, ?3, ?4)",
          params![host.host_id, host.name, host.public_key, host.access],
        )
        .map_err(database_error)?;
      transaction
        .execute(
          "INSERT INTO encrypted_hosts (host_id, public_key) VALUES (?1, ?2)",
          params![host.host_id, host.public_key],
        )
        .map_err(database_error)?;
    }
    let assignment = match admission {
      HostRegistration::Claim { user_id, machine_name } => Some(account_machines::bind(
        &transaction,
        &host.host_id,
        user_id,
        machine_name,
      )?),
      _ => None,
    };
    // Only valid new identities consume the budget. Existing hosts can
    // reconnect and name conflicts can be corrected without using a slot.
    if registration.is_none() {
      self
        .registrations
        .lock()
        .map_err(|_| "Hub registration budget lock poisoned")?
        .reserve()?;
    }
    transaction.commit().map_err(database_error)?;
    Ok((host, assignment))
  }

  pub fn discover_machines(
    &self,
    group: &tokn_hub_client_core::discovery::DiscoveryGroup,
  ) -> Result<Vec<tokn_hub_client_core::discovery::DiscoveredMachine>, String> {
    use tokn_hub_client_core::discovery::{DiscoveredMachine, MAX_DISCOVERY_MACHINES};
    group.validate()?;
    let connection = self.connection()?;
    let mut statement = connection
      .prepare(
        "SELECT h.host_id AS machine_id, m.machine_name AS display_name, a.username || ':' || m.machine_name
       FROM account_machines m
       JOIN hub_accounts a ON a.user_id = m.user_id
       JOIN account_discovery d ON d.user_id = a.user_id
       JOIN hosts h ON h.host_id = m.host_id
       JOIN encrypted_hosts e ON h.host_id = e.host_id AND h.public_key = e.public_key
       WHERE a.username = ?1 AND d.secret_digest = ?2 AND e.revoked = 0
       ORDER BY display_name, machine_id LIMIT ?3",
      )
      .map_err(database_error)?;
    statement
      .query_map(
        params![group.namespace, group.secret_digest, MAX_DISCOVERY_MACHINES as u32],
        |row| {
          Ok(DiscoveredMachine {
            host_id: row.get(0)?,
            name: row.get(1)?,
            online: false,
            machine_address: row.get(2)?,
          })
        },
      )
      .map_err(database_error)?
      .collect::<Result<Vec<_>, _>>()
      .map_err(database_error)
  }
}

#[cfg(test)]
fn fixture_account(store: &Store) -> &'static str {
  const USER_ID: &str = "00000000-0000-4000-8000-000000000001";
  store
    .connection()
    .unwrap()
    .execute(
      "INSERT OR IGNORE INTO hub_accounts (user_id, username, email) VALUES (?1, 'fixture', 'fixture@example.invalid')",
      [USER_ID],
    )
    .unwrap();
  USER_ID
}

fn host_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HostRecord> {
  Ok(HostRecord {
    host_id: row.get(0)?,
    name: row.get(1)?,
    public_key: row.get(2)?,
    access: row.get(3)?,
  })
}

fn read_passkeys(connection: &Connection) -> Result<Vec<Passkey>, String> {
  let mut statement = connection
    .prepare("SELECT credential_json FROM passkeys ORDER BY credential_id")
    .map_err(database_error)?;
  let rows = statement
    .query_map([], |row| row.get::<_, String>(0))
    .map_err(database_error)?;
  rows
    .map(|row| {
      let json = row.map_err(database_error)?;
      serde_json::from_str(&json).map_err(|error| format!("Invalid stored passkey: {error}"))
    })
    .collect()
}

fn database_error(error: rusqlite::Error) -> String {
  format!("Hub database: {error}")
}

fn prepare_private_file(path: &Path) -> Result<(), String> {
  if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
      use std::os::unix::fs::DirBuilderExt;
      builder.mode(0o700);
    }
    builder
      .create(parent)
      .map_err(|error| format!("Create Hub data directory: {error}"))?;
  }
  match fs::symlink_metadata(path) {
    Ok(metadata) if !metadata.file_type().is_file() => {
      return Err("Hub database must be a regular file, not a symlink".to_owned());
    }
    Ok(_) => {}
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
    Err(error) => return Err(format!("Inspect Hub database: {error}")),
  }
  let mut options = OpenOptions::new();
  options.create(true).read(true).write(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
  }
  let file = options
    .open(path)
    .map_err(|error| format!("Open Hub database: {error}"))?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    file
      .set_permissions(fs::Permissions::from_mode(0o600))
      .map_err(|error| format!("Protect Hub database: {error}"))?;
  }
  drop(file);
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn encrypted_uuid_registration_persists_and_rejects_identity_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("hub.sqlite3");
    let host = HostRecord {
      host_id: Uuid::new_v4().to_string(),
      name: "Workstation".into(),
      public_key: crate::protocol::encode(&[7; 32]),
      access: "view".into(),
    };
    let store = Store::open(&path).unwrap();
    store.claim_fixture_host(host.clone());
    assert!(
      store
        .register_tunnel_host(
          HostRecord {
            public_key: crate::protocol::encode(&[8; 32]),
            ..host.clone()
          },
          None
        )
        .is_err()
    );
    assert!(
      store
        .claim_account_host(
          HostRecord {
            host_id: Uuid::new_v4().to_string(),
            ..host.clone()
          },
          fixture_account(&store),
          "duplicate-key"
        )
        .is_err()
    );
    let mut reconnected = host.clone();
    reconnected.name = "Renamed".into();
    reconnected.access = "control".into();
    store.register_tunnel_host(reconnected.clone(), None).unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.host(&host.host_id).unwrap(), Some(reconnected.clone()));
    store.remove_host(&host.host_id).unwrap();
    assert!(
      store.register_tunnel_host(reconnected, None).is_err(),
      "revocation survives a proved reconnect"
    );
    assert!(
      store
        .claim_account_host(
          HostRecord {
            public_key: crate::protocol::encode(&[8; 32]),
            ..host.clone()
          },
          fixture_account(&store),
          "replacement"
        )
        .is_err(),
      "revoked UUID remains reserved to its original identity"
    );
  }

  #[test]
  fn encrypted_registration_storage_has_a_hard_capacity() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("hub.sqlite3")).unwrap();
    let user_id = fixture_account(&store);
    let mut first = None;
    let mut unclaimed = None;
    {
      let mut connection = store.connection().unwrap();
      let transaction = connection.transaction().unwrap();
      for index in 0..MAX_REGISTERED_HOSTS {
        let host_id = Uuid::new_v4().to_string();
        let public_key = crate::secure::NoiseIdentity::generate().unwrap().public_key();
        transaction
          .execute(
            "INSERT INTO hosts (host_id, name, public_key, access) VALUES (?1, 'test', ?2, 'view')",
            params![host_id, public_key],
          )
          .unwrap();
        transaction
          .execute(
            "INSERT INTO encrypted_hosts (host_id, public_key) VALUES (?1, ?2)",
            params![host_id, public_key],
          )
          .unwrap();
        let record = HostRecord {
          host_id,
          name: "test".into(),
          public_key,
          access: "view".into(),
        };
        if index == 0 {
          unclaimed = Some(record);
        } else {
          transaction
            .execute(
              "INSERT INTO account_machines (host_id, user_id, machine_name) VALUES (?1, ?2, ?1)",
              params![record.host_id, user_id],
            )
            .unwrap();
          first.get_or_insert(record);
        }
      }
      transaction.commit().unwrap();
    }
    let replacement = HostRecord {
      host_id: Uuid::new_v4().to_string(),
      name: "Replacement".into(),
      public_key: crate::secure::NoiseIdentity::generate().unwrap().public_key(),
      access: "view".into(),
    };
    store.claim_fixture_host(replacement);
    let unclaimed = unclaimed.unwrap();
    assert!(
      store
        .claim_account_host(unclaimed.clone(), user_id, "older-host")
        .err()
        .unwrap()
        .contains("capacity")
    );
    let removed = first.unwrap();
    store.remove_host(&removed.host_id).unwrap();
    assert!(store.host(&removed.host_id).unwrap().is_none());
    store.claim_account_host(unclaimed, user_id, "older-host").unwrap();
    assert_eq!(store.hosts().unwrap().len(), MAX_REGISTERED_HOSTS as usize);
    assert!(
      store
        .register_tunnel_host(removed, None)
        .unwrap_err()
        .contains("revoked"),
      "reclaiming active capacity must preserve the revoked routing identity"
    );
  }

  #[test]
  fn registration_budget_is_shared_and_preserves_reconnects_and_window_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("hub.sqlite3")).unwrap();
    let clone = store.clone();
    let mut first = None;
    for _ in 0..NEW_REGISTRATIONS_PER_WINDOW {
      let registered = HostRecord {
        host_id: Uuid::new_v4().to_string(),
        name: "Host".into(),
        public_key: crate::secure::NoiseIdentity::generate().unwrap().public_key(),
        access: "view".into(),
      };
      store.claim_fixture_host(registered.clone());
      first.get_or_insert(registered);
    }
    let next = HostRecord {
      host_id: Uuid::new_v4().to_string(),
      name: "Next host".into(),
      public_key: crate::secure::NoiseIdentity::generate().unwrap().public_key(),
      access: "view".into(),
    };
    assert!(
      clone
        .claim_account_host(next.clone(), fixture_account(&store), "next")
        .err()
        .unwrap()
        .contains("rate limit")
    );
    assert!(store.host(&next.host_id).unwrap().is_none());
    let mut reconnect = first.unwrap();
    reconnect.name = "Renamed while registration is limited".into();
    assert_eq!(clone.register_tunnel_host(reconnect.clone(), None).unwrap(), reconnect);
    clone.registrations.lock().unwrap().window_start = Instant::now() - REGISTRATION_WINDOW;
    store.claim_fixture_host(next);
    assert_eq!(
      store.hosts().unwrap().len(),
      (NEW_REGISTRATIONS_PER_WINDOW + 1) as usize
    );
  }

  #[test]
  fn registration_ledger_remains_bounded_after_hosts_are_revoked() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("hub.sqlite3")).unwrap();
    {
      let mut connection = store.connection().unwrap();
      let transaction = connection.transaction().unwrap();
      for index in 0..MAX_REGISTRATION_LEDGER {
        transaction
          .execute(
            "INSERT INTO encrypted_hosts (host_id, public_key, revoked) VALUES (?1, ?2, 1)",
            params![Uuid::new_v4().to_string(), format!("retired_key_{index}")],
          )
          .unwrap();
      }
      transaction.commit().unwrap();
    }
    assert!(store.hosts().unwrap().is_empty());
    let host = HostRecord {
      host_id: Uuid::new_v4().to_string(),
      name: "New host".into(),
      public_key: crate::secure::NoiseIdentity::generate().unwrap().public_key(),
      access: "view".into(),
    };
    assert!(
      store
        .claim_account_host(host.clone(), fixture_account(&store), "new-host")
        .err()
        .unwrap()
        .contains("revocation ledger is full")
    );
    assert!(store.host(&host.host_id).unwrap().is_none());
    let ledger_count: u32 = store
      .connection()
      .unwrap()
      .query_row("SELECT COUNT(*) FROM encrypted_hosts", [], |row| row.get(0))
      .unwrap();
    assert_eq!(ledger_count, MAX_REGISTRATION_LEDGER);
  }

  #[cfg(unix)]
  #[test]
  fn database_is_private_and_symlinks_are_rejected() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("private/hub.sqlite3");
    Store::open(&path).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(
      fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777,
      0o700
    );
    let link = directory.path().join("linked.sqlite3");
    symlink(path, &link).unwrap();
    assert!(Store::open(link).is_err());
  }
}
