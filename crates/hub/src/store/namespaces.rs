//! Directory entries for account-owned machines.
use super::*;
use tokn_hub_client_core::address::parse_machine_address;

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
  pub machine_address: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceError {
  Invalid(String),
  Internal(String),
}

impl NamespaceError {
  fn database(error: rusqlite::Error) -> Self {
    Self::Internal(database_error(error))
  }
}

impl Store {
  /// An offline account machine still resolves. The client establishes trust
  /// through OTP or a saved Noise pin.
  pub fn resolve_machine(&self, username: &str, machine_name: &str) -> Result<Option<MachineRecord>, NamespaceError> {
    let machine_address = format!("{username}:{machine_name}");
    parse_machine_address(&machine_address).map_err(NamespaceError::Invalid)?;
    self
      .connection()
      .map_err(NamespaceError::Internal)?
      .query_row(
        "SELECT h.host_id, h.name FROM account_machines m
         JOIN hub_accounts a ON a.user_id = m.user_id
         JOIN hosts h ON m.host_id = h.host_id
         JOIN encrypted_hosts e ON h.host_id = e.host_id AND h.public_key = e.public_key
         WHERE a.username = ?1 AND m.machine_name = ?2 AND e.revoked = 0",
        params![username, machine_name],
        |row| {
          Ok(MachineRecord {
            host_id: row.get(0)?,
            machine_address: machine_address.clone(),
            name: row.get(1)?,
          })
        },
      )
      .optional()
      .map_err(NamespaceError::database)
  }

  pub fn host_catalog(&self) -> Result<Vec<HostCatalogEntry>, String> {
    let connection = self.connection()?;
    let mut statement = connection
      .prepare(
        "SELECT h.host_id, h.name, h.access, a.username || ':' || m.machine_name
         FROM account_machines m
         JOIN hub_accounts a ON a.user_id = m.user_id
         JOIN hosts h ON m.host_id = h.host_id
         JOIN encrypted_hosts e ON h.host_id = e.host_id AND h.public_key = e.public_key
         WHERE e.revoked = 0 ORDER BY h.name, h.host_id",
      )
      .map_err(database_error)?;
    statement
      .query_map([], |row| {
        Ok(HostCatalogEntry {
          host_id: row.get(0)?,
          name: row.get(1)?,
          access: row.get(2)?,
          machine_address: row.get(3)?,
        })
      })
      .map_err(database_error)?
      .collect::<Result<Vec<_>, _>>()
      .map_err(database_error)
  }
}

#[cfg(test)]
mod tests;
