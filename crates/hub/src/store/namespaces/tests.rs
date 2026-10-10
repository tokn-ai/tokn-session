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
fn namespace_addresses_survive_restart_and_remain_reserved_after_revocation() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("hub.sqlite");
  let store = Store::open(&path).unwrap();
  let first = host(1);
  let second = host(2);
  store.register_encrypted_host(first.clone()).unwrap();
  store.register_encrypted_host(second.clone()).unwrap();
  store.create_namespace("alice").unwrap();
  let bound = store.bind_machine("alice", "workstation", &first.host_id).unwrap();
  assert_eq!(bound.machine_address, "alice:workstation");
  assert_eq!(
    store.bind_machine("alice", "workstation", &first.host_id).unwrap(),
    bound
  );
  store.bind_machine("alice", "laptop", &second.host_id).unwrap();
  assert!(matches!(
    store.bind_machine("alice", "workstation", &second.host_id),
    Err(NamespaceError::Conflict(_))
  ));
  assert!(matches!(
    store.bind_machine("alice", "other", &first.host_id),
    Err(NamespaceError::Conflict(_))
  ));
  drop(store);

  let store = Store::open(&path).unwrap();
  assert_eq!(
    store.namespaces().unwrap(),
    vec![Namespace {
      username: "alice".into()
    }]
  );
  assert_eq!(store.resolve_machine("alice", "workstation").unwrap(), Some(bound));
  let mut renamed = first.clone();
  renamed.name = "New display name".into();
  store.register_encrypted_host(renamed.clone()).unwrap();
  assert_eq!(
    store.resolve_machine("alice", "workstation").unwrap().unwrap().name,
    renamed.name
  );
  let catalog = store.host_catalog().unwrap();
  assert!(
    catalog
      .iter()
      .all(|host| host.secure_only && host.machine_address.is_some())
  );

  store.remove_host(&first.host_id).unwrap();
  assert!(store.resolve_machine("alice", "workstation").unwrap().is_none());
  drop(store);
  let store = Store::open(&path).unwrap();
  let replacement = host(3);
  store.register_encrypted_host(replacement.clone()).unwrap();
  assert!(matches!(
    store.bind_machine("alice", "workstation", &replacement.host_id),
    Err(NamespaceError::Conflict(_))
  ));
  assert!(store.register_encrypted_host(first).is_err());
  assert!(matches!(
    store.bind_machine("alice", "workstation", &renamed.host_id),
    Err(NamespaceError::NotFound(_))
  ));
}

#[test]
fn namespaces_are_bounded_and_bindings_require_active_encrypted_registrations() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path().join("hub.sqlite")).unwrap();
  for invalid in ["Alice", "a_b", "-alice", "alice-", " alice", "alice:extra", ""] {
    assert!(
      matches!(store.create_namespace(invalid), Err(NamespaceError::Invalid(_))),
      "{invalid}"
    );
  }
  assert!(matches!(
    store.create_namespace(&"a".repeat(64)),
    Err(NamespaceError::Invalid(_))
  ));
  store.create_namespace("alice").unwrap();
  assert!(matches!(
    store.create_namespace("alice"),
    Err(NamespaceError::Conflict(_))
  ));
  let legacy = host(1);
  store.approve_host(legacy.clone()).unwrap();
  assert!(matches!(
    store.bind_machine("alice", "legacy", &legacy.host_id),
    Err(NamespaceError::NotFound(_))
  ));
  assert!(!store.host_catalog().unwrap()[0].secure_only);
  assert!(matches!(
    store.bind_machine("unknown", "machine", &Uuid::new_v4().to_string()),
    Err(NamespaceError::NotFound(_))
  ));
  assert!(matches!(
    store.bind_machine("alice", "unknown", &Uuid::new_v4().to_string()),
    Err(NamespaceError::NotFound(_))
  ));
  assert!(matches!(
    store.bind_machine("alice", "machine", "invalid"),
    Err(NamespaceError::Invalid(_))
  ));
  assert!(matches!(
    store.resolve_machine("alice", "a_b"),
    Err(NamespaceError::Invalid(_))
  ));
  for index in 1..MAX_NAMESPACES {
    store.create_namespace(&format!("user-{index}")).unwrap();
  }
  assert_eq!(store.namespaces().unwrap().len(), MAX_NAMESPACES as usize);
  assert!(matches!(
    store.create_namespace("overflow"),
    Err(NamespaceError::Conflict(_))
  ));
}

#[test]
fn concurrent_address_claims_have_one_durable_winner() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("hub.sqlite");
  let store = Store::open(&path).unwrap();
  store.create_namespace("alice").unwrap();
  let first = host(1);
  let second = host(2);
  store.register_encrypted_host(first.clone()).unwrap();
  store.register_encrypted_host(second.clone()).unwrap();
  let other = Store::open(&path).unwrap();
  let barrier = Arc::new(std::sync::Barrier::new(2));
  let claims = [(store, first.host_id), (other, second.host_id)]
    .into_iter()
    .map(|(store, host_id)| {
      let barrier = barrier.clone();
      std::thread::spawn(move || {
        barrier.wait();
        store.bind_machine("alice", "workstation", &host_id)
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
      .filter(|result| matches!(result, Err(NamespaceError::Conflict(_))))
      .count(),
    1
  );
  let store = Store::open(&path).unwrap();
  assert_eq!(
    store.resolve_machine("alice", "workstation").unwrap(),
    results.into_iter().find_map(Result::ok)
  );
}

#[test]
fn an_existing_registry_migrates_without_replacing_identity_or_host_data() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("hub.sqlite");
  let original_owner = Uuid::new_v4();
  let registered = host(1);
  let connection = Connection::open(&path).unwrap();
  connection.execute_batch(
    "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
     CREATE TABLE hosts (host_id TEXT PRIMARY KEY, name TEXT NOT NULL, public_key TEXT NOT NULL UNIQUE, access TEXT NOT NULL);
     CREATE TABLE encrypted_hosts (host_id TEXT PRIMARY KEY, public_key TEXT NOT NULL UNIQUE, revoked INTEGER NOT NULL DEFAULT 0);",
  ).unwrap();
  connection
    .execute(
      "INSERT INTO metadata (key, value) VALUES ('owner_id', ?1)",
      [original_owner.to_string()],
    )
    .unwrap();
  connection
    .execute(
      "INSERT INTO hosts (host_id, name, public_key, access) VALUES (?1, ?2, ?3, ?4)",
      params![
        registered.host_id,
        registered.name,
        registered.public_key,
        registered.access
      ],
    )
    .unwrap();
  connection
    .execute(
      "INSERT INTO encrypted_hosts (host_id, public_key) VALUES (?1, ?2)",
      params![registered.host_id, registered.public_key],
    )
    .unwrap();
  drop(connection);
  let store = Store::open(&path).unwrap();
  assert_eq!(store.owner_id().unwrap(), original_owner);
  assert!(store.namespaces().unwrap().is_empty());
  assert_eq!(store.host(&registered.host_id).unwrap(), Some(registered.clone()));
  store.create_namespace("alice").unwrap();
  store.bind_machine("alice", "workstation", &registered.host_id).unwrap();
  assert_eq!(store.host(&registered.host_id).unwrap(), Some(registered));
  assert_eq!(store.owner_id().unwrap(), original_owner);
}
