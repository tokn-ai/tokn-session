use super::*;
use rand::{RngCore, rngs::OsRng};
use tokn_hub_client_core::{address::parse_machine_address, discovery::DiscoveryGroup};

pub(crate) const ACCOUNT_REQUIRED: &str = "Sign in with tokn-session-hub auth login, then register this machine with tokn-session-hub register --machine <name>";
pub(crate) const ADDRESS_TAKEN: &str = "Machine name is already reserved on this account";
pub(crate) const HOST_CLAIMED: &str = "This host already has an account machine assignment";
pub(crate) const MACHINE_NOT_OWNED: &str = "This machine is not registered to this account";
pub(crate) const STALE_MACHINE_ADDRESS: &str = "The machine address changed; refresh it before renaming";

#[derive(Serialize, Deserialize)]
pub(crate) struct AccountMachineClaim {
  pub host_id: String,
  pub machine_address: String,
  pub group: DiscoveryGroup,
}

pub(super) fn is_owned(connection: &Connection, host_id: &str) -> Result<bool, String> {
  connection
    .query_row(
      "SELECT EXISTS (
         SELECT 1 FROM account_machines m JOIN hub_accounts a ON a.user_id = m.user_id
         WHERE m.host_id = ?1
       )",
      [host_id],
      |row| row.get(0),
    )
    .map_err(database_error)
}

pub(super) fn bind(
  connection: &Connection,
  host_id: &str,
  user_id: &str,
  machine_name: &str,
) -> Result<AccountMachineClaim, String> {
  let username: String = connection
    .query_row(
      "SELECT username FROM hub_accounts WHERE user_id = ?1",
      [user_id],
      |row| row.get(0),
    )
    .map_err(database_error)?;
  let machine_address = format!("{username}:{machine_name}");
  parse_machine_address(&machine_address)?;
  let existing: Option<(String, String)> = connection
    .query_row(
      "SELECT user_id, machine_name FROM account_machines WHERE host_id = ?1",
      [host_id],
      |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(database_error)?;
  match existing {
    Some((owner, name)) if owner == user_id && name == machine_name => {}
    Some(_) => return Err(HOST_CLAIMED.into()),
    None => {
      let taken: bool = connection
        .query_row(
          "SELECT EXISTS (SELECT 1 FROM account_machines WHERE user_id = ?1 AND machine_name = ?2)",
          params![user_id, machine_name],
          |row| row.get(0),
        )
        .map_err(database_error)?;
      if taken {
        return Err(ADDRESS_TAKEN.into());
      }
      connection
        .execute(
          "INSERT INTO account_machines (host_id, user_id, machine_name) VALUES (?1, ?2, ?3)",
          params![host_id, user_id, machine_name],
        )
        .map_err(database_error)?;
    }
  }
  let mut capability = [0u8; 32];
  OsRng.fill_bytes(&mut capability);
  connection
    .execute(
      "INSERT INTO account_discovery (user_id, secret_digest) VALUES (?1, ?2) ON CONFLICT(user_id) DO NOTHING",
      params![user_id, crate::protocol::encode(&capability)],
    )
    .map_err(database_error)?;
  let secret_digest: String = connection
    .query_row(
      "SELECT secret_digest FROM account_discovery WHERE user_id = ?1",
      [user_id],
      |row| row.get(0),
    )
    .map_err(database_error)?;
  Ok(AccountMachineClaim {
    host_id: host_id.into(),
    machine_address,
    group: DiscoveryGroup {
      namespace: username,
      secret_digest,
    },
  })
}

impl Store {
  pub(crate) fn rename_account_host(
    &self,
    user_id: &str,
    host_id: &str,
    public_key: &str,
    expected_machine_address: &str,
    machine_name: &str,
  ) -> Result<AccountMachineClaim, String> {
    let mut connection = self.connection()?;
    let transaction = connection
      .transaction_with_behavior(TransactionBehavior::Immediate)
      .map_err(database_error)?;
    let owned: Option<(String, String)> = transaction
      .query_row(
        "SELECT a.username, m.machine_name FROM account_machines m
         JOIN hub_accounts a ON a.user_id = m.user_id
         JOIN encrypted_hosts e ON e.host_id = m.host_id
         JOIN hosts h ON h.host_id = m.host_id AND h.public_key = e.public_key
         WHERE m.host_id = ?1 AND m.user_id = ?2 AND e.public_key = ?3 AND e.revoked = 0",
        params![host_id, user_id, public_key],
        |row| Ok((row.get(0)?, row.get(1)?)),
      )
      .optional()
      .map_err(database_error)?;
    let (username, current_name) = owned.ok_or(MACHINE_NOT_OWNED)?;
    let (expected_username, _) = parse_machine_address(expected_machine_address)?;
    let machine_address = format!("{username}:{machine_name}");
    parse_machine_address(&machine_address)?;
    if expected_username != username
      || (format!("{username}:{current_name}") != expected_machine_address && current_name != machine_name)
    {
      return Err(STALE_MACHINE_ADDRESS.into());
    }
    if current_name != machine_name {
      let taken: bool = transaction
        .query_row(
          "SELECT EXISTS (SELECT 1 FROM account_machines WHERE user_id = ?1 AND machine_name = ?2)",
          params![user_id, machine_name],
          |row| row.get(0),
        )
        .map_err(database_error)?;
      if taken {
        return Err(ADDRESS_TAKEN.into());
      }
      transaction
        .execute(
          "UPDATE account_machines SET machine_name = ?2 WHERE host_id = ?1",
          params![host_id, machine_name],
        )
        .map_err(database_error)?;
    }
    let claim = bind(&transaction, host_id, user_id, machine_name)?;
    transaction.commit().map_err(database_error)?;
    Ok(claim)
  }

  pub(crate) fn account_machines(&self, user_id: &str) -> Result<Vec<MachineRecord>, String> {
    let connection = self.connection()?;
    let mut statement = connection
      .prepare(
        "SELECT h.host_id, a.username || ':' || m.machine_name, h.name FROM account_machines m
         JOIN hub_accounts a ON a.user_id = m.user_id
         JOIN hosts h ON h.host_id = m.host_id
         JOIN encrypted_hosts e ON e.host_id = h.host_id AND e.public_key = h.public_key
         WHERE m.user_id = ?1 AND e.revoked = 0 ORDER BY m.machine_name",
      )
      .map_err(database_error)?;
    statement
      .query_map([user_id], |row| {
        Ok(MachineRecord {
          host_id: row.get(0)?,
          machine_address: row.get(1)?,
          name: row.get(2)?,
        })
      })
      .map_err(database_error)?
      .collect::<Result<Vec<_>, _>>()
      .map_err(database_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn host() -> HostRecord {
    HostRecord {
      host_id: Uuid::new_v4().to_string(),
      name: "Workstation".into(),
      public_key: crate::protocol::encode(
        ed25519_dalek::SigningKey::generate(&mut OsRng)
          .verifying_key()
          .as_bytes(),
      ),
      access: "view".into(),
    }
  }

  fn user(store: &Store, username: &str) -> String {
    let user_id = Uuid::new_v4().to_string();
    store
      .connection()
      .unwrap()
      .execute(
        "INSERT INTO hub_accounts (user_id, username, email) VALUES (?1, ?2, ?3)",
        params![user_id, username, format!("{username}@example.invalid")],
      )
      .unwrap();
    user_id
  }

  #[test]
  fn account_assignment_is_atomic_unique_and_survives_reconnect_and_revocation() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("state.sqlite")).unwrap();
    let first = user(&store, "alice");
    let second = user(&store, "bob");
    let laptop = host();
    let other = host();
    assert_eq!(
      store.register_tunnel_host(laptop.clone(), None).unwrap_err(),
      ACCOUNT_REQUIRED
    );
    let assignment = store.claim_account_host(laptop.clone(), &first, "laptop").unwrap();
    assert_eq!(assignment.machine_address, "alice:laptop");
    assert_eq!(
      store
        .claim_account_host(laptop.clone(), &first, "laptop")
        .unwrap()
        .group,
      assignment.group
    );
    assert_eq!(
      store.claim_account_host(other.clone(), &first, "laptop").err().unwrap(),
      ADDRESS_TAKEN
    );
    assert!(store.host(&other.host_id).unwrap().is_none());
    assert_eq!(
      store
        .claim_account_host(laptop.clone(), &second, "laptop")
        .err()
        .unwrap(),
      HOST_CLAIMED
    );
    let other_assignment = store.claim_account_host(other.clone(), &second, "laptop").unwrap();
    assert_ne!(other_assignment.group, assignment.group);
    assert_eq!(store.register_tunnel_host(laptop.clone(), None).unwrap(), laptop);
    assert_eq!(
      store.resolve_machine("alice", "laptop").unwrap().unwrap().host_id,
      laptop.host_id
    );
    assert_eq!(store.account_machines(&second).unwrap()[0].host_id, other.host_id);
    store.remove_host(&laptop.host_id).unwrap();
    assert!(store.resolve_machine("alice", "laptop").unwrap().is_none());
    assert_eq!(
      store.claim_account_host(host(), &first, "laptop").err().unwrap(),
      ADDRESS_TAKEN
    );
  }

  #[test]
  fn renaming_requires_the_current_owner_key_and_address_and_has_one_concurrent_winner() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("state.sqlite")).unwrap();
    let owner = user(&store, "alice");
    let machine = host();
    store.claim_account_host(machine.clone(), &owner, "laptop").unwrap();
    store.claim_account_host(host(), &owner, "reserved").unwrap();
    assert_eq!(
      store
        .rename_account_host(
          &owner,
          &machine.host_id,
          &machine.public_key,
          "alice:laptop",
          "reserved"
        )
        .err()
        .unwrap(),
      ADDRESS_TAKEN
    );
    assert_eq!(
      store
        .rename_account_host(&owner, &machine.host_id, &host().public_key, "alice:laptop", "other")
        .err()
        .unwrap(),
      MACHINE_NOT_OWNED
    );
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let tasks = ["studio", "office"].map(|name| {
      let store = store.clone();
      let machine = machine.clone();
      let owner = owner.clone();
      let barrier = barrier.clone();
      std::thread::spawn(move || {
        barrier.wait();
        store.rename_account_host(&owner, &machine.host_id, &machine.public_key, "alice:laptop", name)
      })
    });
    let results = tasks.map(|task| task.join().unwrap());
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert!(results.iter().any(|result| {
      result
        .as_ref()
        .err()
        .is_some_and(|error| error == STALE_MACHINE_ADDRESS)
    }));
    assert_eq!(store.host(&machine.host_id).unwrap(), Some(machine));
    assert!(store.resolve_machine("alice", "laptop").unwrap().is_none());
  }

  #[test]
  fn discovery_lists_only_machines_owned_by_the_requested_account() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("state.sqlite")).unwrap();
    let owner = user(&store, "alice");
    let laptop = host();
    let assignment = store.claim_account_host(laptop.clone(), &owner, "laptop").unwrap();
    let server = host();
    assert_eq!(
      store
        .claim_account_host(server.clone(), &owner, "server")
        .unwrap()
        .group,
      assignment.group
    );
    let unrelated = host();
    let other_account = store
      .claim_account_host(unrelated, &user(&store, "bob"), "unrelated")
      .unwrap();
    let discovered = store.discover_machines(&assignment.group).unwrap();
    assert_eq!(
      discovered.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(),
      ["laptop", "server"]
    );
    assert_eq!(
      discovered
        .iter()
        .map(|entry| entry.machine_address.as_deref().unwrap())
        .collect::<Vec<_>>(),
      ["alice:laptop", "alice:server"]
    );
    let wrong_namespace = DiscoveryGroup {
      namespace: other_account.group.namespace,
      ..assignment.group
    };
    assert!(store.discover_machines(&wrong_namespace).unwrap().is_empty());
  }
}
