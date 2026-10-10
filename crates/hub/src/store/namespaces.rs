//! Administrator-owned routing names. These never establish encryption trust.
use super::*;
use tokn_hub_client_core::address::parse_machine_address;

const MAX_NAMESPACES: u32 = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Namespace {
  pub username: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MachineRecord {
  pub host_id: String,
  pub machine_address: String,
  pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCatalogEntry {
  pub host_id: String,
  pub name: String,
  pub access: String,
  pub secure_only: bool,
  pub machine_address: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceError {
  Invalid(String),
  Conflict(String),
  NotFound(String),
  Internal(String),
}

impl NamespaceError {
  fn database(error: rusqlite::Error) -> Self {
    Self::Internal(database_error(error))
  }
}

fn address(username: &str, machine_name: &str) -> Result<String, NamespaceError> {
  let address = format!("{username}:{machine_name}");
  parse_machine_address(&address).map_err(NamespaceError::Invalid)?;
  Ok(address)
}

impl Store {
  pub fn namespaces(&self) -> Result<Vec<Namespace>, NamespaceError> {
    let connection = self.connection().map_err(NamespaceError::Internal)?;
    let mut statement = connection
      .prepare("SELECT username FROM namespaces WHERE owner_id = (SELECT value FROM metadata WHERE key = 'owner_id') ORDER BY username")
      .map_err(NamespaceError::database)?;
    statement
      .query_map([], |row| Ok(Namespace { username: row.get(0)? }))
      .map_err(NamespaceError::database)?
      .collect::<Result<Vec<_>, _>>()
      .map_err(NamespaceError::database)
  }

  /// Only the administrator API calls this mutation. The Hub's stable owner
  /// identity owns the namespace; a host registration cannot claim it.
  pub fn create_namespace(&self, username: &str) -> Result<Namespace, NamespaceError> {
    address(username, "machine")?;
    let mut connection = self.connection().map_err(NamespaceError::Internal)?;
    let transaction = connection
      .transaction_with_behavior(TransactionBehavior::Immediate)
      .map_err(NamespaceError::database)?;
    let exists: bool = transaction
      .query_row(
        "SELECT EXISTS (SELECT 1 FROM namespaces WHERE username = ?1)",
        [username],
        |row| row.get(0),
      )
      .map_err(NamespaceError::database)?;
    if exists {
      return Err(NamespaceError::Conflict("This username is already reserved".into()));
    }
    let count: u32 = transaction
      .query_row("SELECT COUNT(*) FROM namespaces", [], |row| row.get(0))
      .map_err(NamespaceError::database)?;
    if count >= MAX_NAMESPACES {
      return Err(NamespaceError::Conflict(
        "Namespace capacity reached (64 usernames)".into(),
      ));
    }
    transaction
      .execute(
        "INSERT INTO namespaces (username, owner_id) VALUES (?1, (SELECT value FROM metadata WHERE key = 'owner_id'))",
        [username],
      )
      .map_err(NamespaceError::database)?;
    transaction.commit().map_err(NamespaceError::database)?;
    Ok(Namespace {
      username: username.into(),
    })
  }

  /// The unique address and host constraints reserve the mapping forever,
  /// including when remove_host deletes the active host row. The persistent
  /// encrypted UUID ledger is the foreign-key target rather than that row.
  pub fn bind_machine(
    &self,
    username: &str,
    machine_name: &str,
    host_id: &str,
  ) -> Result<MachineRecord, NamespaceError> {
    let machine_address = address(username, machine_name)?;
    if !crate::protocol::valid_host_uuid(host_id) {
      return Err(NamespaceError::Invalid(
        "Host ID must be a canonical version-4 UUID".into(),
      ));
    }
    let mut connection = self.connection().map_err(NamespaceError::Internal)?;
    let transaction = connection
      .transaction_with_behavior(TransactionBehavior::Immediate)
      .map_err(NamespaceError::database)?;
    let exists: bool = transaction
      .query_row(
        "SELECT EXISTS (SELECT 1 FROM namespaces WHERE username = ?1 AND owner_id = (SELECT value FROM metadata WHERE key = 'owner_id'))",
        [username],
        |row| row.get(0),
      )
      .map_err(NamespaceError::database)?;
    if !exists {
      return Err(NamespaceError::NotFound("Unknown namespace".into()));
    }
    let existing: Option<String> = transaction
      .query_row(
        "SELECT host_id FROM host_addresses WHERE username = ?1 AND machine_name = ?2",
        params![username, machine_name],
        |row| row.get(0),
      )
      .optional()
      .map_err(NamespaceError::database)?;
    if existing.as_deref().is_some_and(|saved| saved != host_id) {
      return Err(NamespaceError::Conflict(
        "This machine address is permanently reserved to another host".into(),
      ));
    }
    let has_other_address: bool = transaction
      .query_row(
        "SELECT EXISTS (SELECT 1 FROM host_addresses WHERE host_id = ?1 AND (username != ?2 OR machine_name != ?3))",
        params![host_id, username, machine_name],
        |row| row.get(0),
      )
      .map_err(NamespaceError::database)?;
    if has_other_address {
      return Err(NamespaceError::Conflict(
        "This host already has a permanent machine address".into(),
      ));
    }
    let name: Option<String> = transaction
      .query_row(
        "SELECT h.name FROM hosts h JOIN encrypted_hosts e ON h.host_id = e.host_id AND h.public_key = e.public_key WHERE h.host_id = ?1 AND e.revoked = 0",
        [host_id],
        |row| row.get(0),
      )
      .optional()
      .map_err(NamespaceError::database)?;
    let name = name.ok_or_else(|| NamespaceError::NotFound("Unknown active encrypted host".into()))?;
    if existing.is_none() {
      transaction
        .execute(
          "INSERT INTO host_addresses (username, machine_name, host_id) VALUES (?1, ?2, ?3)",
          params![username, machine_name, host_id],
        )
        .map_err(NamespaceError::database)?;
    }
    transaction.commit().map_err(NamespaceError::database)?;
    Ok(MachineRecord {
      host_id: host_id.into(),
      machine_address,
      name,
    })
  }

  /// Exact discovery only. No key is returned, and an offline host is still
  /// resolvable. The client establishes trust through OTP or a saved Noise pin.
  pub fn resolve_machine(&self, username: &str, machine_name: &str) -> Result<Option<MachineRecord>, NamespaceError> {
    let machine_address = address(username, machine_name)?;
    self
      .connection()
      .map_err(NamespaceError::Internal)?
      .query_row(
        "SELECT h.host_id, h.name FROM host_addresses a JOIN namespaces n ON a.username = n.username JOIN hosts h ON a.host_id = h.host_id JOIN encrypted_hosts e ON h.host_id = e.host_id AND h.public_key = e.public_key WHERE a.username = ?1 AND a.machine_name = ?2 AND e.revoked = 0 AND n.owner_id = (SELECT value FROM metadata WHERE key = 'owner_id')",
        params![username, machine_name],
        |row| Ok(MachineRecord { host_id: row.get(0)?, machine_address: machine_address.clone(), name: row.get(1)? }),
      )
      .optional()
      .map_err(NamespaceError::database)
  }

  /// Encryption eligibility comes from durable registration, not an online
  /// tunnel flag. This keeps an offline encrypted host from looking plaintext.
  pub fn host_catalog(&self) -> Result<Vec<HostCatalogEntry>, String> {
    let connection = self.connection()?;
    let mut statement = connection
      .prepare(
        "SELECT h.host_id, h.name, h.access, e.host_id IS NOT NULL, CASE WHEN e.host_id IS NOT NULL THEN a.username || ':' || a.machine_name END FROM hosts h LEFT JOIN encrypted_hosts e ON h.host_id = e.host_id AND h.public_key = e.public_key AND e.revoked = 0 LEFT JOIN host_addresses a ON h.host_id = a.host_id ORDER BY h.name, h.host_id",
      )
      .map_err(database_error)?;
    statement
      .query_map([], |row| {
        Ok(HostCatalogEntry {
          host_id: row.get(0)?,
          name: row.get(1)?,
          access: row.get(2)?,
          secure_only: row.get(3)?,
          machine_address: row.get(4)?,
        })
      })
      .map_err(database_error)?
      .collect::<Result<Vec<_>, _>>()
      .map_err(database_error)
  }
}

#[cfg(test)]
mod tests;
