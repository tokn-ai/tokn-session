use super::SessionTransport;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, sync::Mutex};

const MAX_BYTES: u64 = 256 * 1024;
const MAX_SESSIONS: usize = 1024;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Saved {
  version: u32,
  sessions: BTreeMap<String, SessionTransport>,
}

pub(super) struct ControlStore {
  path: Option<PathBuf>,
  memory: Mutex<Saved>,
}

impl ControlStore {
  pub(super) fn new(path: Option<PathBuf>) -> Result<Self, String> {
    let store = Self {
      path,
      memory: Mutex::new(Saved::default()),
    };
    Ok(store)
  }

  fn read(&self) -> Result<Saved, String> {
    let saved = if let Some(path) = &self.path {
      tokn_session_storage::read_private_json::<Saved>(path, MAX_BYTES)?.unwrap_or_default()
    } else {
      self.memory.lock().unwrap_or_else(|error| error.into_inner()).clone()
    };
    validate(&saved)?;
    Ok(saved)
  }

  pub(super) fn transport(&self, identity: &str) -> Result<SessionTransport, String> {
    Ok(self.read()?.sessions.get(identity).copied().unwrap_or_default())
  }

  pub(super) fn set_transport(&self, identity: &str, transport: SessionTransport) -> Result<(), String> {
    let update = |saved: Option<Saved>| {
      let mut saved = saved.unwrap_or_default();
      validate(&saved)?;
      if transport == SessionTransport::Rpc {
        saved.sessions.remove(identity);
      } else {
        if !saved.sessions.contains_key(identity) && saved.sessions.len() >= MAX_SESSIONS {
          return Err("Session transport preference limit reached".into());
        }
        saved.sessions.insert(identity.to_owned(), transport);
      }
      saved.version = 1;
      Ok(saved)
    };
    if let Some(path) = &self.path {
      tokn_session_storage::update_private_json(path, MAX_BYTES, update)?;
    } else {
      let mut memory = self.memory.lock().unwrap_or_else(|error| error.into_inner());
      *memory = update(Some(memory.clone()))?;
    }
    Ok(())
  }
}

fn validate(saved: &Saved) -> Result<(), String> {
  if saved.version > 1 || saved.sessions.len() > MAX_SESSIONS || saved.sessions.keys().any(|key| key.len() > 2048) {
    return Err("Unsupported or oversized session control preferences; preserve and repair the file".into());
  }
  Ok(())
}
