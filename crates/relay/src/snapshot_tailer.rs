use std::collections::{HashMap, HashSet};

use tokn_session_client::AgentClient;
use tokn_session_core::{NormalizedRecord, Provider};

use crate::{
  NewFileReplay, ProviderRoot, RecordOperation, RelayRecord, SessionContext, TailUpdate,
  file_version::{FileVersion, versions},
  tailer::apply_replay_policy,
};

/// File snapshot adapters whose normalization can depend on future records.
/// Retain serialized record fingerprints rather than decoded snapshots.
pub(crate) struct SnapshotTailer {
  root: ProviderRoot,
  sessions: HashMap<String, Observation>,
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Write;
  use tokn_session_core::AgentEvent;

  #[test]
  fn qoder_snapshots_follow_complete_appends_without_replaying_history() {
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("fixture-project");
    std::fs::create_dir(&project).unwrap();
    let path = project.join("session-main.jsonl");
    let user = serde_json::json!({
      "type": "user", "uuid": "user-1", "parentUuid": null,
      "sessionId": "session-main", "cwd": fixture.path(), "isSidechain": false,
      "timestamp": "2026-11-20T10:00:00Z",
      "message": {"role": "user", "content": "Hello"}
    });
    std::fs::write(&path, format!("{user}\n")).unwrap();
    let mut tailer = SnapshotTailer::new(ProviderRoot::new(Provider::Qoder, fixture.path().to_path_buf()));
    let seed = tailer.scan(false, false, NewFileReplay::All).unwrap();
    assert!(seed.warnings.is_empty(), "{:?}", seed.warnings);
    assert!(seed.records.is_empty());

    let assistant = serde_json::json!({
      "type": "assistant", "uuid": "assistant-1", "parentUuid": "user-1",
      "sessionId": "session-main", "cwd": fixture.path(), "isSidechain": false,
      "timestamp": "2026-11-20T10:00:01Z",
      "message": {"id": "message-1", "role": "assistant", "model": "fixture-model",
        "content": [{"type": "text", "text": "Hello back"}]}
    });
    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(file, "{assistant}").unwrap();
    file.flush().unwrap();
    let update = tailer.scan(true, false, NewFileReplay::All).unwrap();
    assert!(update.warnings.is_empty(), "{:?}", update.warnings);
    let messages: Vec<_> = update
      .records
      .iter()
      .flat_map(|record| {
        assert_eq!(record.topic, "qoder.session-main");
        record.record.events.iter().filter_map(|event| match event {
          AgentEvent::Message(message) => Some(message.text.as_str()),
          _ => None,
        })
      })
      .collect();
    assert_eq!(messages, ["Hello back"]);
    assert!(tailer.scan(true, false, NewFileReplay::All).unwrap().records.is_empty());
  }
}

struct Observation {
  version: Vec<Option<FileVersion>>,
  fingerprints: HashMap<String, String>,
}

impl SnapshotTailer {
  pub fn new(root: ProviderRoot) -> Self {
    Self {
      root,
      sessions: HashMap::new(),
    }
  }

  pub fn matches_path(&self, path: &std::path::Path) -> bool {
    !path.file_name().is_some_and(|name| name == "workbuddy.db-shm")
      && (path.starts_with(&self.root.path) || self.root.path.starts_with(path))
  }

  pub fn scan(&mut self, publish: bool, native: bool, replay: NewFileReplay) -> Result<TailUpdate, String> {
    let mut update = TailUpdate::default();
    let headers = match AgentClient::list_session_headers(
      crate::providers::source(self.root.provider),
      Some(self.root.path.clone()),
    ) {
      Ok(headers) => headers,
      Err(error) => {
        update.warnings.push(error);
        return Ok(update);
      }
    };
    let mut seen = HashSet::new();
    for header in headers {
      let key = serde_json::to_string(&(&header.path, &header.id)).map_err(|err| err.to_string())?;
      seen.insert(key.clone());
      if !header.path.is_file() {
        continue;
      }
      let mut version = versions(&header.path, false);
      if self.root.provider == Provider::Dsh {
        for dependency in tokn_session_dsh::storage_dependency_paths(&header.path)? {
          if dependency != header.path {
            version.extend(versions(&dependency, false));
          }
        }
      }
      if self.root.provider == Provider::WorkBuddy {
        version.extend(versions(&self.root.path.join("workbuddy.db"), true));
      }
      let previous = self.sessions.get(&key);
      if previous.is_some_and(|previous| previous.version == version) {
        continue;
      }
      let loaded = match self.root.provider {
        Provider::WorkBuddy => tokn_session_workbuddy::WorkBuddySessionSource::new(Some(self.root.path.clone()))
          .load_session_records_path(&header.path, native, 128 * 1024 * 1024),
        Provider::Dsh => tokn_session_dsh::DshSessionSource::new(Some(self.root.path.clone()))
          .load_session_records_path_with_id(&header.path, &header.id, native, 128 * 1024 * 1024),
        Provider::Qoder => tokn_session_qoder::QoderSessionSource::new(Some(self.root.path.clone()))
          .load_session_records_path(&header.path, native, 128 * 1024 * 1024),
        _ => unreachable!(),
      };
      let loaded = match loaded {
        Ok(loaded) => loaded,
        Err(error) => {
          update.warnings.push(error);
          continue;
        }
      };
      let context = SessionContext::from_session_ref(self.root.provider, &loaded.reference);
      let records: Vec<_> = loaded
        .records
        .into_iter()
        .map(|record| RelayRecord {
          path: loaded.reference.path.clone(),
          topic: format!(
            "{}.{}",
            crate::providers::source(self.root.provider).as_str(),
            context.session_id
          ),
          session: context.clone(),
          operation: RecordOperation::Upsert,
          record,
        })
        .collect();
      let mut fingerprints = HashMap::new();
      let mut changed = Vec::new();
      for record in &records {
        let fingerprint = serde_json::to_string(&record.record).map_err(|err| err.to_string())?;
        if publish && previous.and_then(|old| old.fingerprints.get(&record.record.record_id)) != Some(&fingerprint) {
          changed.push(record.clone());
        }
        fingerprints.insert(record.record.record_id.clone(), fingerprint);
      }
      if publish && let Some(previous) = previous {
        let mut removed: Vec<_> = previous
          .fingerprints
          .keys()
          .filter(|id| !fingerprints.contains_key(*id))
          .collect();
        removed.sort();
        if let Some(context) = records.first() {
          changed.extend(removed.into_iter().map(|id| RelayRecord {
            path: context.path.clone(),
            topic: context.topic.clone(),
            session: context.session.clone(),
            operation: RecordOperation::Remove,
            record: NormalizedRecord {
              record_id: id.clone(),
              native: None,
              events: Vec::new(),
            },
          }));
        }
      }
      if previous.is_none() {
        apply_replay_policy(&mut changed, replay);
      }
      update.records.extend(changed);
      self.sessions.insert(key, Observation { version, fingerprints });
    }
    self.sessions.retain(|key, _| seen.contains(key));
    Ok(update)
  }
}
