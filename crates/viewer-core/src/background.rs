//! Compact live interests are independent of loaded conversation replicas.
//! Registration never opens a source or initializes a history snapshot.
use crate::{
  ViewerService,
  model::{SessionLocator, ViewerProvider, decode_session_key, encode_session_key},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
  collections::{BTreeMap, HashMap, HashSet},
  time::{Duration, Instant},
};

const LEASE: Duration = Duration::from_secs(90);
const MAX_SUBSCRIPTIONS: usize = 24;
const MAX_SIDEBAR_KEYS: usize = 4096;
const MAX_OBSERVATIONS: usize = 10_000;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundScope {
  #[default]
  All,
  Sidebar,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundUpdateLevel {
  Final,
  #[default]
  Messages,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundUpdatesRequest {
  pub subscription_id: String,
  #[serde(default)]
  pub scope: BackgroundScope,
  #[serde(default)]
  pub level: BackgroundUpdateLevel,
  #[serde(default)]
  pub session_keys: Vec<String>,
  #[serde(default)]
  pub unsubscribe: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BackgroundAssistantMessage {
  pub message_id: Option<String>,
  pub delivery: Option<String>,
  pub phase: Option<String>,
  pub preview: Option<String>,
}

struct Subscription {
  owner: Option<String>,
  scope: BackgroundScope,
  level: BackgroundUpdateLevel,
  keys: HashSet<String>,
  generation: String,
  revision: u64,
  accessed: Instant,
}

#[derive(Clone)]
struct Observation {
  message_fingerprint: Option<[u8; 32]>,
  activity_fingerprint: [u8; 32],
  used: u64,
}

#[derive(Default)]
struct ObservationChanges {
  message: bool,
  activity: bool,
}

impl ObservationChanges {
  fn is_empty(&self) -> bool {
    !self.message && !self.activity
  }
}

/// Evicted entries contain only exact keys and digests. SQLite's small page
/// cache keeps historical coverage independent of the bounded hot RAM tier.
struct FingerprintSpill {
  // Field drop order closes SQLite before unlinking its private temporary file.
  connection: Connection,
  _file: tempfile::NamedTempFile,
}

impl FingerprintSpill {
  fn new() -> Result<Self, String> {
    let file = tempfile::Builder::new()
      .prefix("tokn-background-fingerprints-")
      .tempfile()
      .map_err(|error| error.to_string())?;
    let connection = Connection::open(file.path()).map_err(|error| error.to_string())?;
    connection
      .execute_batch(
        "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA cache_size=-1024; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;
      CREATE TABLE observations (
        session_key TEXT PRIMARY KEY NOT NULL,
        message_fingerprint BLOB CHECK(message_fingerprint IS NULL OR length(message_fingerprint) = 32),
        activity_fingerprint BLOB NOT NULL CHECK(length(activity_fingerprint) = 32)
      ) WITHOUT ROWID;",
      )
      .map_err(|error| error.to_string())?;
    Ok(Self {
      connection,
      _file: file,
    })
  }

  fn fingerprint(bytes: Vec<u8>, column: usize) -> rusqlite::Result<[u8; 32]> {
    bytes.try_into().map_err(|_| {
      rusqlite::Error::FromSqlConversionFailure(
        column,
        rusqlite::types::Type::Blob,
        Box::new(std::io::Error::new(
          std::io::ErrorKind::InvalidData,
          "invalid background fingerprint length",
        )),
      )
    })
  }

  fn get(&self, key: &str) -> Result<Option<Observation>, String> {
    self
      .connection
      .query_row(
        "SELECT message_fingerprint, activity_fingerprint FROM observations WHERE session_key = ?1",
        [key],
        |row| {
          let message: Option<Vec<u8>> = row.get(0)?;
          Ok(Observation {
            message_fingerprint: message.map(|bytes| Self::fingerprint(bytes, 0)).transpose()?,
            activity_fingerprint: Self::fingerprint(row.get(1)?, 1)?,
            used: 0,
          })
        },
      )
      .optional()
      .map_err(|error| error.to_string())
  }

  fn put(&self, key: &str, observation: &Observation) -> Result<(), String> {
    self.connection.execute("INSERT INTO observations(session_key, message_fingerprint, activity_fingerprint) VALUES(?1, ?2, ?3)
      ON CONFLICT(session_key) DO UPDATE SET message_fingerprint=excluded.message_fingerprint, activity_fingerprint=excluded.activity_fingerprint",
      params![key, observation.message_fingerprint.as_ref().map(|digest| digest.as_slice()), observation.activity_fingerprint.as_slice()])
      .map(|_| ()).map_err(|error| error.to_string())
  }

  fn keys(&self) -> Result<Vec<String>, String> {
    let mut statement = self
      .connection
      .prepare("SELECT session_key FROM observations")
      .map_err(|error| error.to_string())?;
    let rows = statement
      .query_map([], |row| row.get(0))
      .map_err(|error| error.to_string())?;
    rows
      .collect::<rusqlite::Result<Vec<String>>>()
      .map_err(|error| error.to_string())
  }
}

pub(crate) struct BackgroundStore {
  subscriptions: HashMap<String, Subscription>,
  observations: HashMap<String, Observation>,
  // Least recently written, rather than read: a sibling full reconciliation
  // must not displace hot message updates merely by touching historical rows.
  observation_order: BTreeMap<u64, String>,
  capacity: usize,
  spill: Option<FingerprintSpill>,
  failure: Option<String>,
  clock: u64,
}

impl Default for BackgroundStore {
  fn default() -> Self {
    Self::with_limit(MAX_OBSERVATIONS)
  }
}

impl BackgroundStore {
  fn with_limit(capacity: usize) -> Self {
    Self {
      subscriptions: HashMap::new(),
      observations: HashMap::new(),
      observation_order: BTreeMap::new(),
      capacity: capacity.max(1),
      spill: None,
      failure: None,
      clock: 0,
    }
  }

  fn ensure_healthy(&self) -> Result<(), String> {
    self.failure.as_ref().map_or(Ok(()), |error| Err(error.clone()))
  }

  fn fail(&mut self, error: String) {
    if self.failure.is_none() {
      self.failure = Some(format!("Background fingerprint cache unavailable: {error}"));
    }
  }

  fn observation(&self, key: &str) -> Result<Option<Observation>, String> {
    if let Some(observation) = self.observations.get(key) {
      return Ok(Some(observation.clone()));
    }
    self
      .spill
      .as_ref()
      .map(|spill| spill.get(key))
      .transpose()
      .map(Option::flatten)
  }

  fn insert_observation(&mut self, key: String, observation: Observation) -> Result<(), String> {
    if let Some(previous) = self.observations.get(&key) {
      self.observation_order.remove(&previous.used);
    } else if self.observations.len() >= self.capacity {
      let (used, oldest) = self
        .observation_order
        .first_key_value()
        .map(|(used, key)| (*used, key.clone()))
        .expect("full observation tier has an eviction candidate");
      if self.spill.is_none() {
        self.spill = Some(FingerprintSpill::new()?);
      }
      // Persist before removing the hot entry: an I/O failure cannot silently
      // turn a known historical message into a first observation.
      self.spill.as_ref().unwrap().put(&oldest, &self.observations[&oldest])?;
      self.observations.remove(&oldest);
      self.observation_order.remove(&used);
    }
    self.observation_order.insert(observation.used, key.clone());
    self.observations.insert(key, observation);
    Ok(())
  }

  fn observation_keys(&mut self) -> Vec<String> {
    if self.ensure_healthy().is_err() {
      return Vec::new();
    }
    let mut keys: HashSet<_> = self.observations.keys().cloned().collect();
    if let Some(spill) = &self.spill {
      match spill.keys() {
        Ok(spilled) => keys.extend(spilled),
        Err(error) => {
          self.fail(error);
          return Vec::new();
        }
      }
    }
    keys.into_iter().collect()
  }

  fn prune(&mut self) {
    self.subscriptions.retain(|_, value| value.accessed.elapsed() < LEASE);
  }

  fn subscribe(&mut self, request: BackgroundUpdatesRequest, owner: Option<String>) -> Result<Value, String> {
    self.prune();
    let id = request.subscription_id;
    if let Some(existing) = self.subscriptions.get(&id) {
      if existing.owner != owner {
        return Err("Background subscription belongs to another connection".into());
      }
    }
    if request.unsubscribe {
      self.subscriptions.remove(&id);
      return Ok(json!({"subscription_id":id,"unsubscribed":true}));
    }
    self.ensure_healthy()?;
    if !self.subscriptions.contains_key(&id) && self.subscriptions.len() >= MAX_SUBSCRIPTIONS {
      return Err("Too many background subscriptions".into());
    }
    let keys: HashSet<_> = request.session_keys.into_iter().collect();
    let subscription = self.subscriptions.entry(id.clone()).or_insert_with(|| Subscription {
      owner,
      scope: request.scope,
      level: request.level,
      keys: keys.clone(),
      generation: uuid::Uuid::new_v4().to_string(),
      revision: 0,
      accessed: Instant::now(),
    });
    if subscription.scope != request.scope || subscription.level != request.level || subscription.keys != keys {
      subscription.generation = uuid::Uuid::new_v4().to_string();
      subscription.revision = 0;
    }
    subscription.scope = request.scope;
    subscription.level = request.level;
    subscription.keys = keys;
    subscription.accessed = Instant::now();
    Ok(
      json!({"subscription_id":id,"scope":subscription.scope,"level":subscription.level,"generation":subscription.generation,
      "revision":subscription.revision.to_string(),"subscribed":true}),
    )
  }

  fn remember(&mut self, key: String, notification: &Value) -> Result<ObservationChanges, String> {
    self.ensure_healthy()?;
    let mut message_fingerprint = (!notification["message_revision"].is_null())
      .then(|| Sha256::digest(notification["latest_assistant_message"].to_string().as_bytes()).into());
    let activity_fingerprint = Sha256::digest(json!({
      "removed": notification["removed"], "has_unread": notification["has_unread"],
      "unread_final_count": notification["unread_final_count"], "is_running": notification["is_running"],
      "has_running_descendant": notification["has_running_descendant"], "question_attention": notification["question_attention"],
    }).to_string().as_bytes()).into();
    let previous = self.observation(&key)?;
    if let Some(previous) = &previous {
      // Unknown staged bodies do not erase the last known message digest.
      message_fingerprint = message_fingerprint.or(previous.message_fingerprint);
    }
    let changes = ObservationChanges {
      message: previous
        .as_ref()
        .is_none_or(|value| value.message_fingerprint != message_fingerprint),
      activity: previous
        .as_ref()
        .is_none_or(|value| value.activity_fingerprint != activity_fingerprint),
    };
    if changes.is_empty() {
      return Ok(changes);
    }
    self.clock = self
      .clock
      .checked_add(1)
      .ok_or("Background fingerprint generation overflow")?;
    self.insert_observation(
      key,
      Observation {
        message_fingerprint,
        activity_fingerprint,
        used: self.clock,
      },
    )?;
    Ok(changes)
  }

  fn seed(&mut self, notification: Value) {
    let Some(key) = notification["session_key"].as_str() else {
      return;
    };
    if self.ensure_healthy().is_err() {
      return;
    }
    // A newer observation can live in either tier. A stale startup response
    // must never overwrite the live digest after hot-tier eviction.
    let result = self.observation(key).and_then(|previous| {
      if previous.is_some() {
        Ok(ObservationChanges::default())
      } else {
        self.remember(key.into(), &notification)
      }
    });
    if let Err(error) = result {
      self.fail(error);
    }
  }

  fn publish(&mut self, notification: Value) -> Vec<Value> {
    self.prune();
    let Some(key) = notification["session_key"].as_str().map(str::to_owned) else {
      return Vec::new();
    };
    // Source checkpoints can advance for tools without changing a message or
    // attention. Compare display fields, not the source cursor carried on wire.
    let changes = match self.remember(key.clone(), &notification) {
      Ok(changes) if !changes.is_empty() => changes,
      Ok(_) => return Vec::new(),
      Err(error) => {
        self.fail(error);
        return Vec::new();
      }
    };
    self
      .subscriptions
      .iter_mut()
      .filter_map(|(id, subscription)| {
        if subscription.scope != BackgroundScope::All && !subscription.keys.contains(&key) {
          return None;
        }
        let mut payload = notification.clone();
        if subscription.level == BackgroundUpdateLevel::Final
          && !notification["latest_assistant_message"].is_null()
          && notification["latest_assistant_message"]["delivery"] != "final"
          && notification["removed"] != true
        {
          if !changes.activity {
            return None;
          }
          // Attention remains live at every level. A missing message revision
          // preserves the client's last preview instead of clearing it when
          // this projection filters an intermediate assistant reply.
          payload["latest_assistant_message"] = Value::Null;
          payload["message_revision"] = Value::Null;
        }
        subscription.revision = subscription.revision.saturating_add(1);
        payload["subscription_id"] = json!(id);
        payload["level"] = json!(subscription.level);
        payload["generation"] = json!(subscription.generation);
        payload["revision"] = json!(subscription.revision.to_string());
        Some(payload)
      })
      .collect()
  }
}

impl ViewerService {
  pub fn subscribe_background(&self, request: BackgroundUpdatesRequest) -> Result<Value, String> {
    self.subscribe_background_owned(request, None)
  }

  pub fn subscribe_background_owned(
    &self,
    request: BackgroundUpdatesRequest,
    owner: Option<String>,
  ) -> Result<Value, String> {
    if request.subscription_id.is_empty()
      || request.subscription_id.len() > 128
      || request.session_keys.len() > MAX_SIDEBAR_KEYS
      || request.session_keys.iter().map(String::len).sum::<usize>() > 256 * 1024
      || (request.scope == BackgroundScope::All && !request.session_keys.is_empty())
    {
      return Err("Invalid background subscription".into());
    }
    if self.session_scope.is_some() && request.scope == BackgroundScope::All && !request.unsubscribe {
      return Err("All-session background updates are unavailable for a session share".into());
    }
    if !request.unsubscribe && self.relay.status().settings.mode == crate::relay::RelayMode::External {
      return Err("Background updates unavailable in External reader mode".into());
    }
    for key in &request.session_keys {
      decode_session_key(key)?;
      self.check_session_scope(key)?;
    }
    self
      .background
      .lock()
      .map_err(|_| "Background store lock poisoned")?
      .subscribe(request, owner)
  }

  pub fn renew_background_subscriptions(&self, ids: &[String]) -> Result<Value, String> {
    if ids.len() > MAX_SUBSCRIPTIONS || ids.iter().any(|id| id.is_empty() || id.len() > 128) {
      return Err("Invalid background subscription renewal".into());
    }
    if ids.is_empty() {
      return Ok(json!({"renewed":true}));
    }
    if self.relay.status().settings.mode == crate::relay::RelayMode::External {
      return Err("Background updates unavailable in External reader mode".into());
    }
    let mut store = self.background.lock().map_err(|_| "Background store lock poisoned")?;
    store.prune();
    store.ensure_healthy()?;
    if ids.iter().any(|id| !store.subscriptions.contains_key(id)) {
      return Err("Background subscription expired; subscribe again".into());
    }
    for id in ids {
      if let Some(subscription) = store.subscriptions.get_mut(id) {
        subscription.accessed = Instant::now();
      }
    }
    Ok(json!({"renewed":true}))
  }

  /// Socket renewal acknowledges only live interests still owned by that
  /// connection. Rich and compact subscriptions keep independent inventories.
  pub fn renew_background_subscriptions_owned(
    &self,
    ids: &[String],
    owner: Option<&str>,
  ) -> Result<crate::updates::SessionSubscriptionRenewal, String> {
    if ids.len() > MAX_SUBSCRIPTIONS || ids.iter().any(|id| id.is_empty() || id.len() > 128) {
      return Err("Invalid background subscription renewal".into());
    }
    let mut renewal = crate::updates::SessionSubscriptionRenewal::default();
    if ids.is_empty() {
      return Ok(renewal);
    }
    if self.relay.status().settings.mode == crate::relay::RelayMode::External {
      return Err("Background updates unavailable in External reader mode".into());
    }
    let mut store = self.background.lock().map_err(|_| "Background store lock poisoned")?;
    store.prune();
    store.ensure_healthy()?;
    let now = Instant::now();
    let mut seen = HashSet::new();
    for id in ids {
      if !seen.insert(id) {
        continue;
      }
      if let Some(subscription) = store.subscriptions.get_mut(id)
        && owner.is_none_or(|owner| subscription.owner.as_deref() == Some(owner))
      {
        subscription.accessed = now;
        renewal.renewed_ids.push(id.clone());
      } else {
        renewal.missing_ids.push(id.clone());
      }
    }
    Ok(renewal)
  }

  pub fn release_background_subscriptions(&self, owner: &str) {
    if let Ok(mut store) = self.background.lock() {
      store
        .subscriptions
        .retain(|_, value| value.owner.as_deref() != Some(owner));
      store.prune();
    }
  }

  pub(crate) fn has_background_subscriptions(&self) -> bool {
    self.background.lock().is_ok_and(|mut store| {
      store.prune();
      !store.subscriptions.is_empty()
    })
  }

  /// Called only by an index follower/topology recovery, never by subscribe.
  pub(crate) fn background_notification_keys(&self, include_present: bool) -> Vec<String> {
    let (all, mut keys) = match self.background.lock() {
      Ok(mut store) => {
        store.prune();
        // Follower fingerprints cover the machine independently of delivery
        // scope, so widening Sidebar to All cannot replay earlier changes.
        let all = include_present
          || store
            .subscriptions
            .values()
            .any(|subscription| subscription.scope == BackgroundScope::All);
        let mut keys: HashSet<String> = store
          .subscriptions
          .values()
          .flat_map(|subscription| subscription.keys.iter().cloned())
          .collect();
        if all {
          keys.extend(store.observation_keys());
        }
        (all, keys)
      }
      Err(_) => return Vec::new(),
    };
    if all && include_present {
      keys.extend(self.background_baseline_keys());
    }
    keys
      .into_iter()
      .filter(|key| self.check_session_scope(key).is_ok())
      .collect()
  }

  /// Source-free startup fingerprints avoid replaying unchanged metadata to a
  /// later subscriber, including a viewer following another indexer process.
  pub(crate) fn background_baseline_keys(&self) -> Vec<String> {
    self
      .session_index
      .list_present_sessions()
      .map(|rows| {
        rows
          .into_iter()
          .filter_map(|row| {
            let Ok(provider) = serde_json::from_value::<ViewerProvider>(json!(row.key.provider)) else {
              return None;
            };
            let locator = SessionLocator {
              version: 1,
              provider,
              session_id: row.key.session_id,
              source_path: row.source_path.into(),
            };
            encode_session_key(&locator)
              .ok()
              .filter(|key| self.check_session_scope(key).is_ok())
          })
          .collect()
      })
      .unwrap_or_default()
  }

  fn compact_background_notification(&self, notification: &Value) -> Option<Value> {
    let Some(key) = notification["session_key"].as_str() else {
      return None;
    };
    if self.check_session_scope(key).is_err() {
      return None;
    }
    // Allowlist the compact projection, never forward source/native/tool data.
    let mut message = notification["latest_assistant_message"].clone();
    if !message.is_null() {
      let parsed = serde_json::from_value::<BackgroundAssistantMessage>(message.clone());
      let Ok(mut parsed) = parsed else {
        return None;
      };
      parsed.preview = parsed.preview.map(|text| text.chars().take(240).collect());
      parsed.message_id = parsed.message_id.map(|id| id.chars().take(1024).collect());
      parsed.delivery = parsed.delivery.map(|text| text.chars().take(64).collect());
      parsed.phase = parsed.phase.map(|text| text.chars().take(64).collect());
      message = json!(parsed);
    }
    Some(json!({"session_key":key,"latest_assistant_message":message,
      "message_revision":notification["message_revision"],"removed":notification["removed"] == true,
      "has_unread":notification["has_unread"],"unread_final_count":notification["unread_final_count"],
      "is_running":notification["is_running"],"has_running_descendant":notification["has_running_descendant"],
      "question_attention":notification["question_attention"]}))
  }

  pub(crate) fn seed_background_notification(&self, notification: &Value) {
    if let Some(payload) = self.compact_background_notification(notification)
      && let Ok(mut store) = self.background.lock()
    {
      store.seed(payload);
    }
  }

  pub(crate) fn publish_background_notification(&self, notification: &Value) -> Vec<Value> {
    let Some(payload) = self.compact_background_notification(notification) else {
      return Vec::new();
    };
    self
      .background
      .lock()
      .map(|mut store| store.publish(payload))
      .unwrap_or_default()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn owned_renewal_reports_expired_missing_and_reclaimed_compact_interests_without_body_reads() {
    let root = tempfile::tempdir().unwrap();
    let service = ViewerService::native(root.path().join("index.sqlite")).unwrap();
    for (id, owner) in [("live", "mine"), ("expired", "mine"), ("reclaimed", "other")] {
      service
        .subscribe_background_owned(request(id, BackgroundScope::All, &[]), Some(owner.into()))
        .unwrap();
    }
    let previous_access = {
      let mut store = service.background.lock().unwrap();
      store.subscriptions.get_mut("expired").unwrap().accessed = Instant::now() - LEASE;
      store.subscriptions.get_mut("reclaimed").unwrap().accessed = Instant::now() - LEASE / 2;
      store.subscriptions["reclaimed"].accessed
    };
    let renewal = service
      .renew_background_subscriptions_owned(
        &["live".into(), "expired".into(), "missing".into(), "reclaimed".into()],
        Some("mine"),
      )
      .unwrap();
    assert_eq!(renewal.renewed_ids, ["live"]);
    assert_eq!(renewal.missing_ids, ["expired", "missing", "reclaimed"]);
    let store = service.background.lock().unwrap();
    assert!(!store.subscriptions.contains_key("expired"));
    assert_eq!(store.subscriptions["reclaimed"].accessed, previous_access);
    drop(store);
    assert_eq!(
      service.renew_background_subscriptions(&["live".into()]).unwrap(),
      json!({"renewed":true})
    );
  }

  fn request(id: &str, scope: BackgroundScope, keys: &[&str]) -> BackgroundUpdatesRequest {
    BackgroundUpdatesRequest {
      subscription_id: id.into(),
      scope,
      level: BackgroundUpdateLevel::Messages,
      session_keys: keys.iter().map(|key| (*key).into()).collect(),
      unsubscribe: false,
    }
  }
  fn notification(key: &str, preview: Option<&str>) -> Value {
    json!({"session_key":key,"message_revision":"source:1","latest_assistant_message":preview.map(|preview| json!({"message_id":null,"delivery":"commentary","phase":"finished","preview":preview}))})
  }

  #[test]
  fn background_levels_default_to_messages_and_reject_other_projections() {
    let mut value = json!({"subscription_id":"one","scope":"all"});
    let legacy = serde_json::from_value::<BackgroundUpdatesRequest>(value.clone()).unwrap();
    assert_eq!(legacy.level, BackgroundUpdateLevel::Messages);
    for (label, level) in [
      ("final", BackgroundUpdateLevel::Final),
      ("messages", BackgroundUpdateLevel::Messages),
    ] {
      value["level"] = json!(label);
      assert_eq!(
        serde_json::from_value::<BackgroundUpdatesRequest>(value.clone())
          .unwrap()
          .level,
        level
      );
    }
    for invalid in [json!("steps"), json!("details"), json!("all"), json!(null)] {
      value["level"] = invalid;
      assert!(serde_json::from_value::<BackgroundUpdatesRequest>(value.clone()).is_err());
    }
  }

  #[test]
  fn final_and_messages_interests_filter_independently_without_revision_gaps() {
    let mut store = BackgroundStore::default();
    let mut baseline = notification("session", Some("Previous final"));
    baseline["latest_assistant_message"]["delivery"] = json!("final");
    baseline["is_running"] = json!(false);
    baseline["question_attention"] = json!({"required_count":0,"available_count":0});
    store.seed(baseline.clone());
    let final_ack = store
      .subscribe(
        BackgroundUpdatesRequest {
          level: BackgroundUpdateLevel::Final,
          ..request("final", BackgroundScope::All, &[])
        },
        None,
      )
      .unwrap();
    assert_eq!(final_ack["level"], "final");
    let messages_ack = store
      .subscribe(request("messages", BackgroundScope::All, &[]), None)
      .unwrap();
    assert_eq!(messages_ack["level"], "messages");

    let mut commentary = baseline.clone();
    commentary["latest_assistant_message"]["delivery"] = json!("commentary");
    commentary["latest_assistant_message"]["preview"] = json!("Checking schema");
    let message_only = store.publish(commentary.clone());
    assert_eq!(message_only.len(), 1);
    assert_eq!(message_only[0]["subscription_id"], "messages");
    assert_eq!(message_only[0]["revision"], "1");
    assert_eq!(message_only[0]["level"], "messages");
    assert!(store.publish(commentary.clone()).is_empty());

    commentary["is_running"] = json!(true);
    commentary["question_attention"] = json!({"required_count":1,"available_count":0});
    let attention = store.publish(commentary.clone());
    assert_eq!(attention.len(), 2);
    let final_update = attention
      .iter()
      .find(|update| update["subscription_id"] == "final")
      .unwrap();
    assert_eq!(final_update["level"], "final");
    assert_eq!(final_update["revision"], "1");
    assert_eq!(final_update["is_running"], true);
    assert_eq!(final_update["question_attention"]["required_count"], 1);
    assert!(final_update["latest_assistant_message"].is_null());
    assert!(
      final_update["message_revision"].is_null(),
      "filtered text must not clear the previous preview"
    );
    assert_eq!(
      attention
        .iter()
        .find(|update| update["subscription_id"] == "messages")
        .unwrap()["revision"],
      "2"
    );
    assert!(store.publish(commentary.clone()).is_empty());

    commentary["latest_assistant_message"]["preview"] = json!("Checking indexes");
    let skipped = store.publish(commentary.clone());
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["subscription_id"], "messages");
    assert_eq!(skipped[0]["revision"], "3");

    let mut completed = baseline;
    completed["latest_assistant_message"]["preview"] = json!("Schema verified");
    let final_updates = store.publish(completed);
    assert_eq!(final_updates.len(), 2);
    let final_update = final_updates
      .iter()
      .find(|update| update["subscription_id"] == "final")
      .unwrap();
    assert_eq!(final_update["revision"], "2");
    assert_eq!(final_update["latest_assistant_message"]["preview"], "Schema verified");
    assert!(!final_update["message_revision"].is_null());
    assert_eq!(
      final_updates
        .iter()
        .find(|update| update["subscription_id"] == "messages")
        .unwrap()["revision"],
      "4"
    );
  }

  #[test]
  fn final_interest_keeps_authoritative_empty_messages_and_removals() {
    let mut store = BackgroundStore::default();
    store.seed(notification("session", Some("Historical commentary")));
    store
      .subscribe(
        BackgroundUpdatesRequest {
          level: BackgroundUpdateLevel::Final,
          ..request("final", BackgroundScope::All, &[])
        },
        None,
      )
      .unwrap();
    let mut unknown = notification("session", None);
    unknown["message_revision"] = Value::Null;
    assert!(store.publish(unknown.clone()).is_empty());
    let cleared = store.publish(notification("session", None));
    assert_eq!(cleared.len(), 1);
    assert_eq!(cleared[0]["revision"], "1");
    assert!(cleared[0]["latest_assistant_message"].is_null());
    assert!(!cleared[0]["message_revision"].is_null());
    unknown["removed"] = json!(true);
    let removed = store.publish(unknown.clone());
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0]["revision"], "2");
    assert_eq!(removed[0]["removed"], true);
    assert!(store.publish(unknown).is_empty());
  }

  #[test]
  fn changing_level_resets_generation_without_replaying_prior_messages() {
    let mut store = BackgroundStore::default();
    store.seed(notification("session", Some("Historical commentary")));
    let before = store
      .subscribe(request("one", BackgroundScope::All, &[]), None)
      .unwrap();
    let first = notification("session", Some("Live commentary"));
    assert_eq!(store.publish(first.clone())[0]["revision"], "1");
    let final_ack = store
      .subscribe(
        BackgroundUpdatesRequest {
          level: BackgroundUpdateLevel::Final,
          ..request("one", BackgroundScope::All, &[])
        },
        None,
      )
      .unwrap();
    assert_ne!(before["generation"], final_ack["generation"]);
    assert_eq!(final_ack["revision"], "0");
    assert!(store.publish(first).is_empty());
    let suppressed = notification("session", Some("Filtered live commentary"));
    assert!(store.publish(suppressed.clone()).is_empty());
    let messages_ack = store
      .subscribe(request("one", BackgroundScope::All, &[]), None)
      .unwrap();
    assert_ne!(final_ack["generation"], messages_ack["generation"]);
    assert_eq!(messages_ack["revision"], "0");
    assert!(
      store.publish(suppressed).is_empty(),
      "filtered messages stay in the shared silent baseline"
    );
    assert_eq!(
      store.publish(notification("session", Some("New commentary")))[0]["revision"],
      "1"
    );
  }

  #[test]
  fn spilled_fingerprints_do_not_replay_history_on_full_reconciliation_or_reconnect() {
    let mut store = BackgroundStore::with_limit(2);
    let mut baseline = (0..5)
      .map(|index| notification(&format!("session-{index}"), Some("Existing reply")))
      .collect::<Vec<_>>();
    for payload in &baseline {
      store.seed(payload.clone());
    }
    assert_eq!(store.observations.len(), 2);
    assert!(store.spill.is_some());
    store
      .subscribe(request("one", BackgroundScope::All, &[]), None)
      .unwrap();
    for _ in 0..3 {
      for payload in &baseline {
        assert!(store.publish(payload.clone()).is_empty());
      }
      assert_eq!(store.observations.len(), 2);
    }
    // Genuine updates to cold rows still publish once and become hot. Their
    // newer digest survives subsequent eviction and a stale startup seed.
    for (index, payload) in baseline.iter_mut().enumerate().take(3) {
      let historical = payload.clone();
      *payload = notification(&format!("session-{index}"), Some("New commentary"));
      assert_eq!(store.publish(payload.clone()).len(), 1);
      store.seed(historical);
    }
    assert!(store.spill.as_ref().unwrap().get("session-0").unwrap().is_some());
    store.seed(notification("session-0", Some("Stale startup reply")));
    for _ in 0..2 {
      for payload in &baseline {
        assert!(store.publish(payload.clone()).is_empty());
      }
    }
    store
      .subscribe(
        BackgroundUpdatesRequest {
          unsubscribe: true,
          ..request("one", BackgroundScope::All, &[])
        },
        None,
      )
      .unwrap();
    store
      .subscribe(request("reconnected", BackgroundScope::All, &[]), None)
      .unwrap();
    for payload in &baseline {
      assert!(store.publish(payload.clone()).is_empty());
    }
    let update = store.publish(notification("session-0", Some("After reconnect")));
    assert_eq!(update.len(), 1);
    assert_eq!(update[0]["subscription_id"], "reconnected");
    assert_eq!(
      store
        .publish(notification("genuinely-new-cold-session", Some("First live message")))
        .len(),
      1
    );
    assert_eq!(store.observations.len(), 2);
    assert_eq!(store.observation_order.len(), 2);
  }

  #[test]
  fn cap_plus_one_rows_remain_exact_and_spilled_keys_cover_tombstones() {
    let mut store = BackgroundStore::with_limit(2);
    for key in ["oldest", "middle", "newest"] {
      store.seed(notification(key, Some("Existing reply")));
    }
    store
      .subscribe(request("one", BackgroundScope::All, &[]), None)
      .unwrap();
    let keys = store.observation_keys().into_iter().collect::<HashSet<_>>();
    assert_eq!(keys, HashSet::from(["oldest".into(), "middle".into(), "newest".into()]));
    for _ in 0..5 {
      for key in ["oldest", "middle", "newest"] {
        assert!(store.publish(notification(key, Some("Existing reply"))).is_empty());
      }
    }
    let mut staged = notification("oldest", None);
    staged["message_revision"] = Value::Null;
    assert!(
      store.publish(staged).is_empty(),
      "unknown staging preserves spilled known message digest"
    );
    let mut removed = notification("oldest", None);
    removed["removed"] = json!(true);
    assert_eq!(store.publish(removed.clone()).len(), 1);
    assert!(store.publish(removed).is_empty());
    assert_eq!(store.observations.len(), 2);
  }

  #[test]
  fn spill_is_private_contains_only_digests_and_is_removed_on_drop() {
    let mut store = BackgroundStore::with_limit(1);
    store.seed(notification("one", Some("Private message text")));
    assert!(store.spill.is_none(), "spill is lazy");
    store.seed(notification("two", Some("Different private text")));
    let spill = store.spill.as_ref().unwrap();
    let path = spill._file.path().to_path_buf();
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    assert_eq!(
      spill
        .connection
        .query_row("PRAGMA cache_size", [], |row| row.get::<_, i64>(0))
        .unwrap(),
      -1024
    );
    let columns = spill
      .connection
      .prepare("PRAGMA table_info(observations)")
      .unwrap()
      .query_map([], |row| row.get::<_, String>(1))
      .unwrap()
      .collect::<rusqlite::Result<Vec<_>>>()
      .unwrap();
    assert_eq!(columns, ["session_key", "message_fingerprint", "activity_fingerprint"]);
    let digest: Vec<u8> = spill
      .connection
      .query_row(
        "SELECT message_fingerprint FROM observations WHERE session_key='one'",
        [],
        |row| row.get(0),
      )
      .unwrap();
    assert_eq!(
      digest,
      Sha256::digest(
        notification("one", Some("Private message text"))["latest_assistant_message"]
          .to_string()
          .as_bytes()
      )
      .as_slice()
    );
    drop(store);
    assert!(!path.exists(), "SQLite closes before its temporary file is unlinked");
  }

  #[test]
  fn spill_failure_stops_uncertain_publication_and_surfaces_on_subscribe_and_renew() {
    let root = tempfile::tempdir().unwrap();
    let service = ViewerService::native(root.path().join("index.sqlite")).unwrap();
    *service.background.lock().unwrap() = BackgroundStore::with_limit(1);
    service
      .subscribe_background(request("one", BackgroundScope::All, &[]))
      .unwrap();
    service.seed_background_notification(&notification("oldest", Some("Existing reply")));
    service.seed_background_notification(&notification("hot", Some("Latest reply")));
    service
      .background
      .lock()
      .unwrap()
      .spill
      .as_ref()
      .unwrap()
      .connection
      .execute("DROP TABLE observations", [])
      .unwrap();
    assert!(
      service
        .publish_background_notification(&notification("oldest", Some("Changed reply")))
        .is_empty()
    );
    assert!(
      service
        .subscribe_background(request("next", BackgroundScope::All, &[]))
        .unwrap_err()
        .contains("Background fingerprint cache unavailable")
    );
    assert!(
      service
        .renew_background_subscriptions(&["one".into()])
        .unwrap_err()
        .contains("Background fingerprint cache unavailable")
    );
    assert!(
      service.renew_background_subscriptions(&[]).is_ok(),
      "rich-only connections do not depend on failed background storage"
    );
    assert!(
      service
        .subscribe_background(BackgroundUpdatesRequest {
          unsubscribe: true,
          ..request("one", BackgroundScope::All, &[])
        })
        .is_ok(),
      "cleanup remains available after a storage failure"
    );
  }

  #[test]
  fn subscription_has_no_snapshot_and_updates_without_a_history_baseline() {
    let mut store = BackgroundStore::default();
    let ack = store
      .subscribe(request("one", BackgroundScope::All, &[]), None)
      .unwrap();
    assert_eq!(ack["revision"], "0");
    assert!(ack.get("items").is_none());
    let update = store.publish(notification("never-opened", Some("Checking schema.")));
    assert_eq!(update.len(), 1);
    assert_eq!(update[0]["revision"], "1");
    assert_eq!(update[0]["subscription_id"], "one");
    assert_eq!(update[0]["latest_assistant_message"]["preview"], "Checking schema.");
    assert!(
      store
        .publish(notification("never-opened", Some("Checking schema.")))
        .is_empty()
    );
    assert_eq!(
      store.publish(notification("never-opened", Some("Done.")))[0]["revision"],
      "2"
    );
  }

  #[test]
  fn silent_index_baseline_does_not_replay_history_on_subscribe_or_reconnect() {
    let mut store = BackgroundStore::default();
    let historical = notification("unopened", Some("Existing reply"));
    store.seed(historical.clone());
    store
      .subscribe(request("one", BackgroundScope::All, &[]), None)
      .unwrap();
    assert!(store.publish(historical.clone()).is_empty());
    let live = notification("unopened", Some("New commentary"));
    assert_eq!(store.publish(live.clone())[0]["revision"], "1");
    // A startup SQLite response captured earlier cannot replace a live digest.
    store.seed(historical);
    assert!(store.publish(live.clone()).is_empty());
    store
      .subscribe(
        BackgroundUpdatesRequest {
          unsubscribe: true,
          ..request("one", BackgroundScope::All, &[])
        },
        None,
      )
      .unwrap();
    store
      .subscribe(request("reconnected", BackgroundScope::All, &[]), None)
      .unwrap();
    assert!(store.publish(live).is_empty());
    assert_eq!(
      store.publish(notification("unopened", Some("Next reply")))[0]["subscription_id"],
      "reconnected"
    );
  }

  #[test]
  fn tool_only_checkpoints_and_unknown_staging_do_not_repeat_messages() {
    let mut store = BackgroundStore::default();
    store
      .subscribe(request("one", BackgroundScope::All, &[]), None)
      .unwrap();
    let mut known = notification("session", Some("Checking schema."));
    assert_eq!(store.publish(known.clone()).len(), 1);
    known["message_revision"] = json!("tool-only-checkpoint:2");
    assert!(store.publish(known.clone()).is_empty());
    let mut staged = known.clone();
    staged["message_revision"] = Value::Null;
    staged["latest_assistant_message"] = Value::Null;
    assert!(store.publish(staged).is_empty());
    assert!(store.publish(known.clone()).is_empty());
    known["is_running"] = json!(true);
    assert_eq!(store.publish(known).len(), 1, "activity changes remain live");
    let cleared = notification("session", None);
    assert_eq!(store.publish(cleared)[0]["latest_assistant_message"], Value::Null);
  }

  #[test]
  fn sidebar_interest_filters_sessions_and_identity_ownership_is_verified() {
    let mut store = BackgroundStore::default();
    store
      .subscribe(
        request("one", BackgroundScope::Sidebar, &["listed"]),
        Some("socket:one".into()),
      )
      .unwrap();
    assert!(store.publish(notification("other", Some("Hidden"))).is_empty());
    assert_eq!(store.publish(notification("listed", Some("Visible"))).len(), 1);
    assert!(
      store
        .subscribe(request("one", BackgroundScope::All, &[]), Some("socket:other".into()))
        .is_err()
    );
    store.subscriptions.get_mut("one").unwrap().accessed = Instant::now() - LEASE;
    assert!(store.publish(notification("listed", Some("Expired"))).is_empty());
  }

  #[test]
  fn changing_scope_resets_generation() {
    let mut store = BackgroundStore::default();
    let before = store
      .subscribe(request("one", BackgroundScope::All, &[]), Some("owner".into()))
      .unwrap();
    let after = store
      .subscribe(
        request("one", BackgroundScope::Sidebar, &["listed"]),
        Some("owner".into()),
      )
      .unwrap();
    assert_ne!(before["generation"], after["generation"]);
    assert!(store.publish(notification("other", Some("Hidden"))).is_empty());
    assert_eq!(
      store.publish(notification("listed", None))[0]["latest_assistant_message"],
      Value::Null
    );
  }

  #[test]
  fn all_interest_registers_without_reading_a_catalog_or_body() {
    let root = tempfile::tempdir().unwrap();
    let service = ViewerService::native(root.path().join("index.sqlite")).unwrap();
    assert!(
      service
        .subscribe_background(request("one", BackgroundScope::All, &[]))
        .is_ok()
    );
    let update = service.publish_background_notification(&json!({"session_key":"one", "message_revision":"source:1",
      "latest_assistant_message":{"message_id":null,"delivery":"commentary","phase":"finished","preview":"x".repeat(1000)},
      "native":"private", "tool_output":"large"}));
    assert_eq!(
      update[0]["latest_assistant_message"]["preview"].as_str().unwrap().len(),
      240
    );
    assert!(update[0].get("native").is_none());
    assert!(update[0].get("tool_output").is_none());
  }

  #[test]
  fn scoped_service_rejects_host_wide_or_outside_interests() {
    let root = tempfile::tempdir().unwrap();
    let service = ViewerService::native(root.path().join("index.sqlite")).unwrap();
    let key = |id: &str| {
      encode_session_key(&SessionLocator {
        version: 1,
        provider: ViewerProvider::Codex,
        session_id: id.into(),
        source_path: root.path().join(format!("{id}.jsonl")),
      })
      .unwrap()
    };
    let allowed = key("allowed");
    let outside = key("outside");
    let scoped = service.scoped_to_sessions(std::slice::from_ref(&allowed)).unwrap();
    assert!(
      scoped
        .subscribe_background(request("all", BackgroundScope::All, &[]))
        .is_err()
    );
    assert!(
      scoped
        .subscribe_background(request("outside", BackgroundScope::Sidebar, &[&outside]))
        .is_err()
    );
    assert!(
      scoped
        .subscribe_background(request("scoped", BackgroundScope::Sidebar, &[&allowed]))
        .is_ok()
    );
    assert!(
      scoped
        .publish_background_notification(&notification(&outside, Some("Private")))
        .is_empty()
    );
    assert_eq!(
      scoped
        .publish_background_notification(&notification(&allowed, Some("Visible")))
        .len(),
      1
    );
  }

  #[test]
  fn disconnect_cannot_release_another_connections_interests() {
    let root = tempfile::tempdir().unwrap();
    let service = ViewerService::native(root.path().join("index.sqlite")).unwrap();
    service
      .subscribe_background_owned(request("old", BackgroundScope::All, &[]), Some("old-socket".into()))
      .unwrap();
    service
      .subscribe_background_owned(request("new", BackgroundScope::All, &[]), Some("new-socket".into()))
      .unwrap();
    service.release_background_subscriptions("old-socket");
    let update = service.publish_background_notification(&notification("session", Some("Current")));
    assert_eq!(update.len(), 1);
    assert_eq!(update[0]["subscription_id"], "new");
    service.release_background_subscriptions("new-socket");
    assert!(!service.has_background_subscriptions());
  }

  #[test]
  fn expired_lease_requires_a_fresh_interest_instead_of_silent_renewal() {
    let root = tempfile::tempdir().unwrap();
    let service = ViewerService::native(root.path().join("index.sqlite")).unwrap();
    service
      .subscribe_background(request("expired", BackgroundScope::All, &[]))
      .unwrap();
    service
      .background
      .lock()
      .unwrap()
      .subscriptions
      .get_mut("expired")
      .unwrap()
      .accessed = Instant::now() - LEASE;
    assert_eq!(
      service.renew_background_subscriptions(&["expired".into()]).unwrap_err(),
      "Background subscription expired; subscribe again"
    );
    service
      .subscribe_background(request("fresh", BackgroundScope::All, &[]))
      .unwrap();
    assert!(service.renew_background_subscriptions(&["fresh".into()]).is_ok());
    assert_eq!(
      service.publish_background_notification(&notification("session", Some("Still live")))[0]["subscription_id"],
      "fresh"
    );
  }
}
