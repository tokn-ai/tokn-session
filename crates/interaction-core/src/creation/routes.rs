//! Immutable provider binding for creation UUIDs across independent workflows.
//! Legacy exclusive workers and shared RPC creation retain their own receipts.
use crate::Provider;
use serde::{Deserialize, Serialize};
use std::{
  collections::BTreeMap,
  path::{Path, PathBuf},
  sync::{Arc, Mutex},
};

const MAX_ROUTES: usize = 1024;
const MAX_STATE_BYTES: u64 = 256 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Saved {
  version: u8,
  #[serde(deserialize_with = "deserialize_routes")]
  routes: BTreeMap<String, Provider>,
}

fn deserialize_routes<'de, D: serde::Deserializer<'de>>(
  deserializer: D,
) -> Result<BTreeMap<String, Provider>, D::Error> {
  struct Routes;
  impl<'de> serde::de::Visitor<'de> for Routes {
    type Value = BTreeMap<String, Provider>;
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      formatter.write_str("a bounded map of unique creation UUIDs to providers")
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
      let mut routes = BTreeMap::new();
      while let Some((id, provider)) = map.next_entry::<String, Provider>()? {
        if routes.len() >= MAX_ROUTES || routes.insert(id, provider).is_some() {
          return Err(serde::de::Error::custom(
            "Creation provider ledger has duplicate or excessive UUIDs",
          ));
        }
      }
      Ok(routes)
    }
  }
  deserializer.deserialize_map(Routes)
}

impl Default for Saved {
  fn default() -> Self {
    Self {
      version: 1,
      routes: BTreeMap::new(),
    }
  }
}

#[derive(Clone)]
pub struct CreationRouteLedger(Arc<Inner>);
struct Inner {
  path: Option<PathBuf>,
  ephemeral: Mutex<BTreeMap<String, Provider>>,
}

impl CreationRouteLedger {
  /// Blocking setup. Inspection validates existing state without creating a
  /// directory or file. None uses a shared in-memory ledger with the same bounds.
  pub fn new(path: Option<PathBuf>) -> Result<Self, String> {
    if let Some(path) = &path {
      if let Some(saved) = tokn_session_storage::read_private_json::<Saved>(path, MAX_STATE_BYTES)? {
        validate(&saved)?;
      }
    }
    Ok(Self(Arc::new(Inner {
      path,
      ephemeral: Mutex::new(BTreeMap::new()),
    })))
  }

  /// Bind before any provider-specific admission or native effect. A matching
  /// claim is idempotent; another provider can never reuse this UUID. Claims are
  /// intentionally retained without eviction, so saturation rejects new UUIDs.
  pub fn claim(&self, request_id: &str, provider: Provider) -> Result<(), String> {
    let request_id = canonical_id(request_id)?;
    if let Some(path) = &self.0.path {
      update(path, |saved| {
        let mut saved = saved.unwrap_or_default();
        validate(&saved)?;
        claim(&mut saved.routes, &request_id, provider)?;
        Ok(saved)
      })
      .map(|_| ())
    } else {
      let mut routes = self
        .0
        .ephemeral
        .lock()
        .map_err(|_| "Creation provider ledger is unavailable")?;
      claim(&mut routes, &request_id, provider)
    }
  }
}

fn canonical_id(id: &str) -> Result<String, String> {
  uuid::Uuid::parse_str(id)
    .map(|id| id.to_string())
    .map_err(|_| "Invalid creation request ID".into())
}

fn claim(routes: &mut BTreeMap<String, Provider>, id: &str, provider: Provider) -> Result<(), String> {
  if let Some(existing) = routes.get(id) {
    return if *existing == provider {
      Ok(())
    } else {
      Err("Creation request ID was already used with another provider".into())
    };
  }
  if routes.len() >= MAX_ROUTES {
    return Err("Creation provider ledger limit reached".into());
  }
  routes.insert(id.to_owned(), provider);
  Ok(())
}

fn validate(saved: &Saved) -> Result<(), String> {
  if saved.version != 1 || saved.routes.len() > MAX_ROUTES {
    return Err("Unsupported creation provider ledger".into());
  }
  for id in saved.routes.keys() {
    if canonical_id(id)? != *id {
      return Err("Creation provider ledger contains a noncanonical UUID".into());
    }
  }
  Ok(())
}

fn update(path: &Path, mut update: impl FnMut(Option<Saved>) -> Result<Saved, String>) -> Result<Saved, String> {
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
  loop {
    match tokn_session_storage::update_private_json(path, MAX_STATE_BYTES, &mut update) {
      Err(error) if error.starts_with("Storage is in use;") && std::time::Instant::now() < deadline => {
        std::thread::sleep(std::time::Duration::from_millis(10));
      }
      result => return result,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn write_private_fixture(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
  }

  #[test]
  fn memory_claims_are_shared_bounded_and_canonical() {
    let ledger = CreationRouteLedger::new(None).unwrap();
    let copy = ledger.clone();
    let id = uuid::Uuid::new_v4();
    ledger.claim(&id.to_string(), Provider::Pi).unwrap();
    copy
      .claim(&id.simple().to_string().to_uppercase(), Provider::Pi)
      .unwrap();
    assert!(
      copy
        .claim(&id.to_string().to_uppercase(), Provider::Codex)
        .unwrap_err()
        .contains("another provider")
    );
    assert!(ledger.claim("invalid", Provider::Pi).is_err());
    for _ in 1..MAX_ROUTES {
      ledger.claim(&uuid::Uuid::new_v4().to_string(), Provider::Dsh).unwrap();
    }
    assert!(
      ledger
        .claim(&uuid::Uuid::new_v4().to_string(), Provider::Dsh)
        .unwrap_err()
        .contains("limit reached")
    );
    copy.claim(&id.to_string(), Provider::Pi).unwrap();
  }

  #[test]
  fn independent_instances_atomically_preserve_the_winning_provider() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("viewer/session-creation-routes.json");
    let first = CreationRouteLedger::new(Some(path.clone())).unwrap();
    assert!(!path.exists());
    let second = CreationRouteLedger::new(Some(path.clone())).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = [(first, Provider::Pi), (second, Provider::Codex)]
      .into_iter()
      .map(|(ledger, provider)| {
        let barrier = barrier.clone();
        let id = id.clone();
        std::thread::spawn(move || {
          barrier.wait();
          (provider, ledger.claim(&id, provider))
        })
      })
      .collect();
    let results: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|(_, result)| result.is_ok()).count(), 1);
    let winner = results.into_iter().find(|(_, result)| result.is_ok()).unwrap().0;
    let restarted = CreationRouteLedger::new(Some(path.clone())).unwrap();
    restarted.claim(&id, winner).unwrap();
    assert!(restarted.claim(&id, Provider::OpenCode).is_err());
    let saved: Saved = tokn_session_storage::read_private_json(&path, MAX_STATE_BYTES)
      .unwrap()
      .unwrap();
    assert_eq!(saved.routes.len(), 1);
    assert_eq!(saved.routes[&id], winner);
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    }
  }

  #[test]
  fn invalid_oversized_and_future_catalogs_fail_without_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("routes.json");
    for bytes in [
      b"{broken".to_vec(),
      br#"{"version":2,"routes":{}}"#.to_vec(),
      br#"{"version":1,"routes":{"bad":"pi"}}"#.to_vec(),
      br#"{"version":1,"routes":{},"unknown":true}"#.to_vec(),
      br#"{"version":1,"routes":{"64f80e49-1898-462c-8a21-598ad0cd405f":"pi","64f80e49-1898-462c-8a21-598ad0cd405f":"codex"}}"#.to_vec(),
      vec![b' '; MAX_STATE_BYTES as usize + 1],
    ] {
      write_private_fixture(&path, &bytes);
      assert!(CreationRouteLedger::new(Some(path.clone())).is_err());
      assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    let mut saved = Saved::default();
    for _ in 0..MAX_ROUTES {
      saved.routes.insert(uuid::Uuid::new_v4().to_string(), Provider::Pi);
    }
    write_private_fixture(&path, serde_json::to_vec(&saved).unwrap());
    let ledger = CreationRouteLedger::new(Some(path.clone())).unwrap();
    ledger.claim(saved.routes.keys().next().unwrap(), Provider::Pi).unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(ledger.claim(&uuid::Uuid::new_v4().to_string(), Provider::Pi).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    saved.routes.insert(uuid::Uuid::new_v4().to_string(), Provider::Pi);
    write_private_fixture(&path, serde_json::to_vec(&saved).unwrap());
    assert!(CreationRouteLedger::new(Some(path.clone())).is_err());
  }

  #[test]
  fn claims_revalidate_catalog_changes_after_construction() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("routes.json");
    let ledger = CreationRouteLedger::new(Some(path.clone())).unwrap();
    write_private_fixture(&path, b"{broken");
    assert!(ledger.claim(&uuid::Uuid::new_v4().to_string(), Provider::Pi).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"{broken");
  }

  #[test]
  fn separate_processes_cannot_bind_one_uuid_to_two_providers() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("routes.json");
    let id = uuid::Uuid::new_v4().to_string();
    let mut children: Vec<_> = ["pi", "codex"]
      .into_iter()
      .map(|provider| {
        std::process::Command::new(std::env::current_exe().unwrap())
          .args([
            "--exact",
            "creation::routes::tests::route_claim_child_process",
            "--ignored",
          ])
          .env("TOKN_TEST_CREATION_ROUTE_PATH", &path)
          .env("TOKN_TEST_CREATION_ROUTE_ID", &id)
          .env("TOKN_TEST_CREATION_ROUTE_PROVIDER", provider)
          .stdout(std::process::Stdio::null())
          .stderr(std::process::Stdio::null())
          .spawn()
          .unwrap()
      })
      .collect();
    let statuses: Vec<_> = children.iter_mut().map(|child| child.wait().unwrap()).collect();
    assert_eq!(statuses.iter().filter(|status| status.success()).count(), 1);
    let saved: Saved = tokn_session_storage::read_private_json(&path, MAX_STATE_BYTES)
      .unwrap()
      .unwrap();
    assert_eq!(saved.routes.len(), 1);
    let winner = saved.routes[&id];
    CreationRouteLedger::new(Some(path))
      .unwrap()
      .claim(&id, winner)
      .unwrap();
  }

  #[test]
  #[ignore = "subprocess entry point for the cross-process claim test"]
  fn route_claim_child_process() {
    let path = PathBuf::from(std::env::var_os("TOKN_TEST_CREATION_ROUTE_PATH").unwrap());
    let id = std::env::var("TOKN_TEST_CREATION_ROUTE_ID").unwrap();
    let provider = match std::env::var("TOKN_TEST_CREATION_ROUTE_PROVIDER").unwrap().as_str() {
      "pi" => Provider::Pi,
      "codex" => Provider::Codex,
      _ => panic!("Invalid test provider"),
    };
    CreationRouteLedger::new(Some(path))
      .unwrap()
      .claim(&id, provider)
      .unwrap();
  }
}
