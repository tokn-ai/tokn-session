use super::*;

fn host(index: u8) -> HostRecord {
  HostRecord {
    host_id: Uuid::new_v4().to_string(),
    name: format!("Machine {index}"),
    public_key: crate::protocol::encode(&[index; 32]),
    access: "view".into(),
  }
}

#[test]
fn concurrent_account_address_claims_have_one_durable_winner() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("hub.sqlite");
  let store = Store::open(&path).unwrap();
  let user_id = fixture_account(&store);
  let other = Store::open(&path).unwrap();
  let barrier = Arc::new(std::sync::Barrier::new(2));
  let claims = [(store, host(1)), (other, host(2))]
    .into_iter()
    .map(|(store, host)| {
      let barrier = barrier.clone();
      std::thread::spawn(move || {
        barrier.wait();
        store.claim_account_host(host, user_id, "workstation")
      })
    })
    .collect::<Vec<_>>();
  let results = claims
    .into_iter()
    .map(|claim| claim.join().unwrap())
    .collect::<Vec<_>>();
  assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
  assert_eq!(
    results
      .iter()
      .filter(|result| result.as_ref().err().is_some_and(|error| error == ADDRESS_TAKEN))
      .count(),
    1
  );
  let store = Store::open(&path).unwrap();
  let claimed = results.into_iter().find_map(Result::ok).unwrap();
  assert_eq!(
    store
      .resolve_machine("fixture", "workstation")
      .unwrap()
      .unwrap()
      .host_id,
    claimed.host_id
  );
  assert_eq!(store.host_catalog().unwrap().len(), 1);
}

#[test]
fn upgraded_registry_hides_old_addresses_and_groups_until_the_host_is_claimed() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("hub.sqlite");
  let store = Store::open(&path).unwrap();
  let owner = store.owner_id().unwrap();
  let registered = host(1);
  store.register_encrypted_host(registered.clone()).unwrap();
  let group = tokn_hub_client_core::discovery::DiscoveryGroup {
    namespace: "legacy".into(),
    secret_digest: crate::protocol::encode(&[4; 32]),
  };
  {
    let connection = store.connection().unwrap();
    connection
      .execute_batch(
        "CREATE TABLE namespaces (username TEXT PRIMARY KEY, owner_id TEXT NOT NULL);
         CREATE TABLE host_addresses (username TEXT NOT NULL, machine_name TEXT NOT NULL, host_id TEXT NOT NULL);
         CREATE TABLE host_discovery (host_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, secret_digest TEXT NOT NULL);",
      )
      .unwrap();
    connection
      .execute("INSERT INTO namespaces VALUES ('legacy', ?1)", [owner.to_string()])
      .unwrap();
    connection
      .execute(
        "INSERT INTO host_addresses VALUES ('legacy', 'workstation', ?1)",
        [&registered.host_id],
      )
      .unwrap();
    connection
      .execute(
        "INSERT INTO host_discovery VALUES (?1, ?2, ?3)",
        params![registered.host_id, group.namespace, group.secret_digest],
      )
      .unwrap();
  }
  drop(store);

  let store = Store::open(&path).unwrap();
  assert_eq!(store.owner_id().unwrap(), owner);
  assert!(store.host(&registered.host_id).unwrap().is_none());
  assert!(store.host_catalog().unwrap().is_empty());
  assert!(store.resolve_machine("legacy", "workstation").unwrap().is_none());
  assert!(store.discover_machines(&group).unwrap().is_empty());
  assert_eq!(
    store
      .register_tunnel_host(registered.clone(), Some(&group))
      .unwrap_err(),
    ACCOUNT_REQUIRED
  );

  let claimed = store.claim_fixture_host(registered.clone());
  assert_eq!(
    store.register_tunnel_host(registered.clone(), None).unwrap(),
    registered
  );
  assert_eq!(store.host(&registered.host_id).unwrap(), Some(registered));
  assert_eq!(
    store.host_catalog().unwrap()[0].machine_address,
    claimed.machine_address
  );
  assert!(store.resolve_machine("legacy", "workstation").unwrap().is_none());
  assert!(store.discover_machines(&group).unwrap().is_empty());
  assert_eq!(store.discover_machines(&claimed.group).unwrap().len(), 1);
}
