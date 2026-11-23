//! Semantic delivery levels. Source history stays authoritative; this bounded
//! store holds only subscribed display objects and replaces snapshots on gaps.
use crate::{
  ViewerService,
  model::{EventPageRequest, HistoryWindowMode, PageDirection},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
  collections::{BTreeMap, HashMap, HashSet},
  sync::Arc,
  time::{Duration, Instant},
};

mod scope;
pub use scope::{HistoryScope, UpdateScope};

const MAX_SUBSCRIPTIONS: usize = 24;
const MAX_DORMANT: usize = 8;
const LEASE: Duration = Duration::from_secs(90);
const MEMORY_TARGET: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq, Ord, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum UpdateLevel {
  Final,
  Messages,
  #[default]
  Steps,
  All,
}

#[derive(Clone, Debug, Deserialize)]
pub struct OpenSessionRequest {
  pub subscription_id: String,
  pub session_key: String,
  #[serde(default)]
  pub level: UpdateLevel,
  /// One-shot retained-history expansion, separate from the revision cursor.
  #[serde(default)]
  pub history_cursor: Option<String>,
  #[serde(default)]
  pub scope: Option<UpdateScope>,
  pub resume: Option<SessionResume>,
}

/// Identifies exactly the projection the client still holds. A revision alone
/// can coincide across sessions, delivery levels and host restarts.
#[derive(Clone, Debug, Deserialize)]
pub struct SessionResume {
  pub subscription_id: String,
  pub generation: String,
  pub revision: String,
  pub state_hash: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SessionReleaseRequest {
  pub subscription_id: String,
  pub session_key: String,
}

type SessionScope = Option<Arc<HashSet<String>>>;

struct Opening {
  session_key: String,
  level: UpdateLevel,
  session_scope: SessionScope,
}

/// Identity-only acknowledgement. Missing leases require registration and a
/// fresh baseline before the client can rely on live source updates again.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct SessionSubscriptionRenewal {
  pub renewed_ids: Vec<String>,
  pub missing_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct OrderEdit {
  pub start: usize,
  pub delete_count: usize,
  pub insert: Vec<String>,
}

impl OrderEdit {
  /// Replace the portion between the two unchanged ends. An append carries
  /// only new IDs, even when the receiver has thousands of retained items.
  fn between(before: &[String], after: &[String]) -> Option<Self> {
    let start = before.iter().zip(after).take_while(|(a, b)| a == b).count();
    if start == before.len() && start == after.len() {
      return None;
    }
    let suffix = before[start..]
      .iter()
      .rev()
      .zip(after[start..].iter().rev())
      .take_while(|(a, b)| a == b)
      .count();
    Some(Self {
      start,
      delete_count: before.len() - start - suffix,
      insert: after[start..after.len() - suffix].to_vec(),
    })
  }
}

#[derive(Debug, Default, Serialize)]
pub struct OrderEdits {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub item_order: Option<OrderEdit>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub semantic_order: Option<OrderEdit>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub event_order: Option<OrderEdit>,
}

impl OrderEdits {
  fn is_empty(&self) -> bool {
    self.item_order.is_none() && self.semantic_order.is_none() && self.event_order.is_none()
  }
}

#[derive(Serialize)]
enum OrderChange<'a> {
  Snapshot([&'a [String]; 3]),
  Delta(&'a OrderEdits),
}

#[derive(Debug, Serialize)]
pub struct SessionUpdate {
  pub subscription_id: String,
  pub session_key: String,
  pub level: UpdateLevel,
  pub generation: String,
  pub base_revision: Option<String>,
  pub revision: String,
  pub base_state_hash: Option<String>,
  pub state_hash: String,
  pub snapshot: bool,
  /// Semantic objects: user_message, assistant_message, notification,
  /// tool_summary, or detail. Existing card summaries are a presentation adapter.
  pub items: Vec<Value>,
  /// Optional grouping adapter for the existing conversation UI; semantic
  /// consumers use items and semantic_order without knowing about folds.
  pub groups: Vec<Value>,
  pub semantic_order: Option<Vec<String>>,
  /// Individual source records, including lifecycle and tool-result fragments.
  pub event_order: Option<Vec<String>>,
  pub removed_items: Vec<String>,
  pub item_order: Option<Vec<String>>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub order_edits: Option<OrderEdits>,
  pub state: Value,
}

struct Subscription {
  session_scope: SessionScope,
  request: OpenSessionRequest,
  generation: String,
  revision: u64,
  state_hash: String,
  items: BTreeMap<String, Value>,
  order: Vec<String>,
  semantic_order: Vec<String>,
  event_order: Vec<String>,
  state: Value,
  accessed: Instant,
  bytes: usize,
}

#[derive(Default)]
pub(crate) struct UpdateStore {
  subscriptions: HashMap<String, Subscription>,
  opening: HashMap<String, Arc<Opening>>,
  // Released projections share the active memory budget. They neither hold
  // a source lease nor receive publications while the user is elsewhere.
  dormant: HashMap<String, Subscription>,
}

fn classify(summary: &Value) -> (&'static str, UpdateLevel) {
  if summary["is_error"] == true {
    return ("notification", UpdateLevel::Final);
  }
  match summary["type"].as_str().unwrap_or("unknown") {
    "message" if summary["role"] == "user" => ("user_message", UpdateLevel::Final),
    "message" if summary["role"] == "assistant" => (
      "assistant_message",
      if summary["delivery"] == "final" {
        UpdateLevel::Final
      } else {
        UpdateLevel::Messages
      },
    ),
    "question_reply" => ("user_message", UpdateLevel::Final),
    "question_request" | "error" | "model_changed" => ("notification", UpdateLevel::Final),
    "tool_call" | "trajectory" | "activity_group" => ("tool_summary", UpdateLevel::Steps),
    _ => ("notification", UpdateLevel::Steps),
  }
}

fn weight(value: &Value) -> usize {
  match value {
    Value::String(value) => value.len().saturating_mul(2),
    Value::Array(values) => values.iter().map(weight).sum::<usize>().saturating_add(24),
    Value::Object(values) => values
      .iter()
      .map(|(key, value)| key.len().saturating_mul(2).saturating_add(weight(value)))
      .sum::<usize>()
      .saturating_add(32),
    _ => 8,
  }
}

fn default_history_scope(request: &mut OpenSessionRequest) {
  request.scope.get_or_insert_with(|| UpdateScope {
    history: if request.history_cursor.is_some() {
      HistoryScope::Retained
    } else {
      HistoryScope::RecentTurns
    },
    ..Default::default()
  });
}

fn source_generation(page: &Value) -> Option<&str> {
  page["source_revision"]
    .as_str()?
    .rsplit_once(':')
    .map(|(generation, _)| generation)
}

impl UpdateStore {
  fn begin_open(&mut self, request: &OpenSessionRequest, session_scope: &SessionScope) -> Result<Arc<Opening>, String> {
    let ticket = Arc::new(Opening {
      session_key: request.session_key.clone(),
      level: request.level,
      session_scope: session_scope.clone(),
    });
    self.expire_leases(Instant::now());
    let id = &request.subscription_id;
    if self.subscriptions.get(id).is_some_and(|s| {
      s.request.session_key != ticket.session_key
        || s.request.level != ticket.level
        || s.session_scope != ticket.session_scope
    }) || self.opening.get(id).is_some_and(|s| {
      s.session_key != ticket.session_key || s.level != ticket.level || s.session_scope != ticket.session_scope
    }) {
      return Err("Subscription identity changed; use a new subscription id".into());
    }
    if (!self.opening.contains_key(id) && self.opening.len() >= MAX_SUBSCRIPTIONS)
      || (!self.subscriptions.contains_key(id) && self.subscriptions.len() >= MAX_SUBSCRIPTIONS)
    {
      return Err("Too many session subscriptions".into());
    }
    self.opening.insert(id.clone(), ticket.clone());
    Ok(ticket)
  }

  fn finish_open(&mut self, id: &str, ticket: &Arc<Opening>) -> Result<(), String> {
    if !self.opening.get(id).is_some_and(|current| Arc::ptr_eq(current, ticket)) {
      return Err("Session open was superseded".into());
    }
    self.opening.remove(id);
    Ok(())
  }

  fn open(
    &mut self,
    request: &OpenSessionRequest,
    session_scope: &SessionScope,
    resume: Option<&SessionResume>,
  ) -> (&mut Subscription, Option<String>) {
    let id = request.subscription_id.clone();
    // Only reclaim an active interest under its own identity. A late release
    // or an unrelated viewer using another active ID must remain independent.
    let subscription = self.subscriptions.entry(id).or_insert_with(|| {
      if let Some(proof) = resume
        && self
          .dormant
          .get(&proof.subscription_id)
          .is_some_and(|s| s.session_scope == *session_scope && s.matches_resume(request, proof))
      {
        return self
          .dormant
          .remove(&proof.subscription_id)
          .expect("verified dormant entry");
      }
      Subscription::new(request, session_scope.clone())
    });
    let cursor = resume
      .filter(|proof| subscription.matches_resume(request, proof))
      .map(|proof| proof.revision.clone());
    (subscription, cursor)
  }

  fn release(&mut self, id: &str) {
    if let Some(mut subscription) = self.subscriptions.remove(id)
      && subscription.revision != 0
    {
      subscription.accessed = Instant::now();
      self.dormant.insert(id.to_owned(), subscription);
      if self.dormant.len() > MAX_DORMANT {
        Self::evict_oldest(&mut self.dormant);
      }
    }
  }

  fn expire_leases(&mut self, now: Instant) {
    let expired: Vec<_> = self
      .subscriptions
      .iter()
      .filter(|(_, subscription)| now.saturating_duration_since(subscription.accessed) >= LEASE)
      .map(|(id, _)| id.clone())
      .collect();
    for id in expired {
      self.release(&id);
    }
  }

  fn evict_oldest(entries: &mut HashMap<String, Subscription>) {
    if let Some(key) = entries
      .iter()
      .min_by_key(|(_, sub)| sub.accessed)
      .map(|(key, _)| key.clone())
    {
      entries.remove(&key);
    }
  }

  fn enforce_budget(&mut self) {
    while self
      .subscriptions
      .values()
      .chain(self.dormant.values())
      .map(|s| s.bytes)
      .sum::<usize>()
      > MEMORY_TARGET
    {
      if self.dormant.is_empty() {
        Self::evict_oldest(&mut self.subscriptions);
      } else {
        Self::evict_oldest(&mut self.dormant);
      }
    }
  }
}

impl Subscription {
  fn new(request: &OpenSessionRequest, session_scope: SessionScope) -> Self {
    Self {
      session_scope,
      request: request.clone(),
      generation: uuid::Uuid::new_v4().to_string(),
      revision: 0,
      state_hash: String::new(),
      items: BTreeMap::new(),
      order: Vec::new(),
      semantic_order: Vec::new(),
      event_order: Vec::new(),
      state: Value::Null,
      accessed: Instant::now(),
      bytes: 0,
    }
  }

  fn matches_resume(&self, request: &OpenSessionRequest, resume: &SessionResume) -> bool {
    self.revision != 0
      && self.request.subscription_id == resume.subscription_id
      && self.request.session_key == request.session_key
      && self.request.level == request.level
      && self.generation == resume.generation
      && self.revision.to_string() == resume.revision
      && self.state_hash == resume.state_hash
  }

  /// Commit to a full projection on reset and only the actual changes thereafter.
  /// The short-lived subscription ID can change when this projection is resumed.
  fn next_hash(
    &self,
    parent: Option<&str>,
    upserts: &[&Value],
    removed: &[String],
    orders: OrderChange<'_>,
    state: &Value,
  ) -> String {
    let mut digest = Sha256::new();
    digest.update(b"tokn:session-projection:v1\0");
    serde_json::to_writer(
      &mut digest,
      &(
        parent,
        &self.request.session_key,
        self.request.level,
        &self.generation,
        self.revision,
        upserts,
        removed,
        orders,
        state,
      ),
    )
    .expect("projection contains only JSON values and an infallible hash writer");
    format!("{:x}", digest.finalize())
  }

  fn measure_bytes(&mut self) {
    self.bytes = self.request.subscription_id.len()
      + self.state_hash.len()
      + self.request.session_key.len()
      + self
        .session_scope
        .iter()
        .flat_map(|scope| scope.iter())
        .map(String::len)
        .sum::<usize>()
      + weight(&self.state)
      + self
        .items
        .iter()
        .map(|(key, value)| key.len() + weight(value))
        .sum::<usize>()
      + self
        .order
        .iter()
        .chain(&self.semantic_order)
        .chain(&self.event_order)
        .map(|key| key.len() + std::mem::size_of::<String>())
        .sum::<usize>();
  }

  fn failure(&mut self, error: String) -> Option<SessionUpdate> {
    let error = if self.session_scope.is_some() {
      "Session updates are temporarily unavailable".to_owned()
    } else {
      error
    };
    let id = "notification:source-error".to_string();
    let item = json!({"item_id":id,"kind":"notification","level":"final","notification":{"type":"source_error","message":error}});
    if self.items.get(&id) == Some(&item) {
      return None;
    }
    let base_revision = self.revision.to_string();
    let snapshot = self.revision == 0;
    let base_state_hash = (!snapshot).then(|| self.state_hash.clone());
    self.revision += 1;
    self.state["error"] = json!(error);
    let empty_edits = OrderEdits::default();
    let orders = if snapshot {
      OrderChange::Snapshot([&self.order, &self.semantic_order, &self.event_order])
    } else {
      OrderChange::Delta(&empty_edits)
    };
    self.state_hash = self.next_hash(base_state_hash.as_deref(), &[&item], &[], orders, &self.state);
    self.items.insert(id, item.clone());
    self.measure_bytes();
    Some(SessionUpdate {
      subscription_id: self.request.subscription_id.clone(),
      session_key: self.request.session_key.clone(),
      level: self.request.level,
      generation: self.generation.clone(),
      base_revision: (!snapshot).then_some(base_revision),
      revision: self.revision.to_string(),
      base_state_hash,
      state_hash: self.state_hash.clone(),
      snapshot,
      items: vec![item],
      groups: Vec::new(),
      semantic_order: snapshot.then(|| self.semantic_order.clone()),
      event_order: snapshot.then(|| self.event_order.clone()),
      removed_items: Vec::new(),
      item_order: snapshot.then(|| self.order.clone()),
      order_edits: None,
      state: self.state.clone(),
    })
  }

  #[cfg(test)]
  fn update(
    &mut self,
    page: Value,
    semantic: Vec<Value>,
    details: Vec<(String, Value)>,
    reset: bool,
    cursor: Option<&str>,
  ) -> SessionUpdate {
    self.update_with_events(page, semantic, details, Vec::new(), reset, cursor)
  }

  fn update_with_events(
    &mut self,
    page: Value,
    semantic: Vec<Value>,
    details: Vec<(String, Value)>,
    events: Vec<Value>,
    reset: bool,
    cursor: Option<&str>,
  ) -> SessionUpdate {
    let (page, semantic, events) = if let Some(scope) = &mut self.request.scope {
      scope::project(page, semantic, events, scope, self.request.level == UpdateLevel::All)
    } else {
      (page, semantic, events)
    };
    let mut state = page;
    let summaries = state
      .as_object_mut()
      .unwrap()
      .remove("events")
      .unwrap()
      .as_array()
      .unwrap()
      .clone();
    let mut items = BTreeMap::new();
    let mut order = Vec::new();
    let mut semantic_order = Vec::new();
    for summary in semantic {
      let (kind, level) = classify(&summary);
      if level > self.request.level {
        continue;
      }
      let item_id = summary["event_key"].as_str().unwrap().to_owned();
      semantic_order.push(item_id.clone());
      items.insert(
        item_id.clone(),
        json!({"item_id":item_id,"kind":kind,"level":level,"summary":summary}),
      );
    }
    for summary in summaries {
      let (_, level) = classify(&summary);
      if level > self.request.level {
        continue;
      }
      let item_id = summary["event_key"].as_str().unwrap().to_owned();
      order.push(item_id.clone());
      if !items.contains_key(&item_id) {
        items.insert(
          item_id.clone(),
          json!({"item_id":item_id,"kind":"work_summary","level":"steps","summary":summary}),
        );
      }
    }
    state["total_events"] = json!(order.len());
    for (key, detail) in details {
      if self.request.level == UpdateLevel::All && self.request.scope.is_some() && !items.contains_key(&key) {
        continue;
      }
      let item_id = format!("detail:{key}");
      items.insert(
        item_id.clone(),
        json!({"item_id":item_id,"kind":"detail","level":"details","event_key":key,"detail":detail}),
      );
    }
    let mut event_order = Vec::new();
    if self.request.level == UpdateLevel::All {
      for event in events {
        let event_key = event["event_key"].as_str().unwrap().to_owned();
        let item_id = format!("event:{event_key}");
        event_order.push(item_id.clone());
        items.insert(
          item_id.clone(),
          json!({"item_id":item_id,"kind":"event","level":"all","event_key":event_key,"event":event}),
        );
      }
    }
    let old_revision = self.revision.to_string();
    let reset = reset || self.revision == 0;
    let snapshot = reset || cursor != Some(old_revision.as_str());
    let base_state_hash = (!snapshot).then(|| self.state_hash.clone());
    let edits = if reset {
      OrderEdits::default()
    } else {
      OrderEdits {
        item_order: OrderEdit::between(&self.order, &order),
        semantic_order: OrderEdit::between(&self.semantic_order, &semantic_order),
        event_order: OrderEdit::between(&self.event_order, &event_order),
      }
    };
    let upserts: Vec<_> = items
      .iter()
      .filter(|(key, value)| reset || self.items.get(*key) != Some(*value))
      .map(|(_, value)| value)
      .collect();
    let removed: Vec<String> = if reset {
      Vec::new()
    } else {
      self
        .items
        .keys()
        .filter(|key| !items.contains_key(*key))
        .cloned()
        .collect()
    };
    let changed = reset || !upserts.is_empty() || !removed.is_empty() || !edits.is_empty() || self.state != state;
    if reset && self.revision != 0 {
      self.generation = uuid::Uuid::new_v4().to_string();
    }
    if changed {
      self.revision += 1;
      let orders = if reset {
        OrderChange::Snapshot([&order, &semantic_order, &event_order])
      } else {
        OrderChange::Delta(&edits)
      };
      self.state_hash = self.next_hash(
        (!reset).then_some(self.state_hash.as_str()),
        &upserts,
        &removed,
        orders,
        &state,
      );
    }
    let additions = if snapshot && !reset {
      // A cursor miss can require a full response from an existing generation.
      // Its commitment still describes the same state as a successful resume.
      items.values().collect()
    } else {
      upserts
    };
    let (groups, additions) = additions
      .into_iter()
      .cloned()
      .partition(|item| item["kind"] == "work_summary");
    self.event_order = event_order;
    self.semantic_order = semantic_order;
    self.items = items;
    self.order = order;
    self.state = state;
    self.measure_bytes();
    SessionUpdate {
      subscription_id: self.request.subscription_id.clone(),
      session_key: self.request.session_key.clone(),
      level: self.request.level,
      generation: self.generation.clone(),
      base_revision: (!snapshot).then_some(old_revision),
      revision: self.revision.to_string(),
      base_state_hash,
      state_hash: self.state_hash.clone(),
      snapshot,
      groups,
      items: additions,
      semantic_order: snapshot.then(|| self.semantic_order.clone()),
      event_order: snapshot.then(|| self.event_order.clone()),
      removed_items: if snapshot { Vec::new() } else { removed },
      item_order: snapshot.then(|| self.order.clone()),
      order_edits: (!snapshot && !edits.is_empty()).then_some(edits),
      state: self.state.clone(),
    }
  }
}

#[derive(Clone)]
struct Projection {
  page: Value,
  semantic: Vec<Value>,
  details: Vec<(String, Value)>,
  events: Vec<Value>,
}

impl ViewerService {
  /// Grant adapters namespace subscription identities before dispatch.
  /// Unrestricted clients must never publish under one of their return addresses.
  pub(crate) fn valid_subscription_id(&self, id: &str) -> bool {
    if id.starts_with("share_") {
      // The adapter prefixes the caller's bounded ID with its grant digest.
      return self.session_scope.is_some() && id.len() <= 208;
    }
    !id.is_empty() && id.len() <= 128
  }

  fn prepare_projection(
    &self,
    session_key: &str,
    include_all: bool,
    history_cursor: Option<&str>,
  ) -> Result<Projection, String> {
    let (page, semantic, payloads) = self.load_update_pages(
      EventPageRequest {
        session_key: session_key.to_string(),
        window_mode: Some(if history_cursor.is_some() {
          HistoryWindowMode::Earlier
        } else {
          HistoryWindowMode::Retained
        }),
        cursor: history_cursor.map(str::to_owned),
        offset: None,
        direction: PageDirection::Backward,
        limit: None,
      },
      include_all,
    )?;
    let mut page = serde_json::to_value(page).map_err(|e| e.to_string())?;
    page["is_running"] = json!(payloads.is_running);
    if let Some(revision) = payloads.source_revision {
      page["source_revision"] = json!(revision);
    }
    Ok(Projection {
      page,
      semantic: semantic
        .into_iter()
        .map(serde_json::to_value)
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?,
      details: payloads
        .details
        .into_iter()
        .map(|detail| {
          let key = detail.event_key.clone();
          serde_json::to_value(detail)
            .map(|detail| (key, detail))
            .map_err(|e| e.to_string())
        })
        .collect::<Result<_, _>>()?,
      events: payloads
        .events
        .into_iter()
        .map(serde_json::to_value)
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?,
    })
  }

  pub fn release_session(&self, request: SessionReleaseRequest) -> Result<Value, String> {
    if !self.valid_subscription_id(&request.subscription_id) {
      return Err("Invalid session subscription identity".into());
    }
    self.check_session_scope(&request.session_key)?;
    let mut store = self.updates.lock().map_err(|_| "Update store lock poisoned")?;
    let id = &request.subscription_id;
    if store
      .opening
      .get(id)
      .is_some_and(|s| s.session_key == request.session_key && s.session_scope == self.session_scope)
    {
      store.opening.remove(id);
    }
    if store
      .subscriptions
      .get(id)
      .is_some_and(|s| s.request.session_key == request.session_key && s.session_scope == self.session_scope)
    {
      store.release(id);
    }
    Ok(json!({"subscription_id": id, "unsubscribed": true}))
  }

  pub fn renew_session_subscriptions(&self, ids: &[String]) -> Result<SessionSubscriptionRenewal, String> {
    if ids.len() > MAX_SUBSCRIPTIONS || ids.iter().any(|id| !self.valid_subscription_id(id)) {
      return Err("Invalid session subscription renewal".into());
    }
    let mut store = self.updates.lock().map_err(|_| "Update store lock poisoned")?;
    let now = Instant::now();
    store.expire_leases(now);
    let mut renewal = SessionSubscriptionRenewal::default();
    let mut seen = std::collections::HashSet::new();
    for id in ids {
      if !seen.insert(id) {
        continue;
      }
      if let Some(subscription) = store.subscriptions.get_mut(id)
        && subscription.session_scope == self.session_scope
      {
        subscription.accessed = now;
        renewal.renewed_ids.push(id.clone());
      } else {
        renewal.missing_ids.push(id.clone());
      }
    }
    Ok(renewal)
  }

  pub fn open_session(&self, mut request: OpenSessionRequest) -> Result<SessionUpdate, String> {
    let resume = request.resume.take();
    if !self.valid_subscription_id(&request.subscription_id)
      || request.scope.as_ref().is_some_and(|s| s.group_keys.len() > 128)
      || resume.as_ref().is_some_and(|r| {
        !self.valid_subscription_id(&r.subscription_id) || r.generation.len() > 128 || r.revision.len() > 20
      })
    {
      return Err("Invalid session open request".into());
    }
    self.check_session_scope(&request.session_key)?;
    default_history_scope(&mut request);

    // Reserve the identity before the cold source read. Release or a newer
    // request can cancel this ticket without waiting for another session's I/O.
    let ticket = self
      .updates
      .lock()
      .map_err(|_| "Update store lock poisoned")?
      .begin_open(&request, &self.session_scope)?;
    let warmed = self.validate_session_key(&request.session_key).and_then(|()| {
      let locator = crate::model::decode_session_key(&request.session_key)?;
      if self.relay.covers(locator.provider) {
        self.relay.load(&locator)?;
      }
      Ok(())
    });
    let mut store = self.updates.lock().map_err(|_| "Update store lock poisoned")?;
    store.finish_open(&request.subscription_id, &ticket)?;
    warmed?;
    store.expire_leases(Instant::now());
    if !store.subscriptions.contains_key(&request.subscription_id) && store.subscriptions.len() >= MAX_SUBSCRIPTIONS {
      return Err("Too many session subscriptions".into());
    }
    // Snapshot and registration share this lock with publication. Later
    // changes necessarily diff against the same state returned to the reader.
    let projection = self.prepare_projection(
      &request.session_key,
      request.level == UpdateLevel::All,
      request.history_cursor.as_deref(),
    )?;
    let (subscription, cursor) = store.open(&request, &self.session_scope, resume.as_ref());
    let generation = source_generation(&projection.page);
    // Repository-only embeddings have no source generation to validate cached
    // detail and inspection identities after a replacement.
    let reset =
      subscription.revision == 0 || generation.is_none() || source_generation(&subscription.state) != generation;
    subscription.request = request;
    subscription.accessed = Instant::now();
    let update = subscription.update_with_events(
      projection.page,
      projection.semantic,
      projection.details,
      projection.events,
      reset,
      cursor.as_deref(),
    );
    store.enforce_budget();
    Ok(update)
  }

  pub(crate) fn invalidate_session_subscriptions(&self, key: Option<&str>) -> Vec<SessionReleaseRequest> {
    let Ok(mut store) = self.updates.lock() else {
      return Vec::new();
    };
    store.expire_leases(Instant::now());
    let affected = |sub: &Subscription| key.is_none_or(|key| key == sub.request.session_key);
    store.dormant.retain(|_, sub| !affected(sub));
    let mut guest_interests = Vec::new();
    store.subscriptions.retain(|id, sub| {
      if !affected(sub) {
        return true;
      }
      // Owners receive the relay reset. A grant sees only its exact interests.
      if sub.session_scope.is_some() {
        guest_interests.push(SessionReleaseRequest {
          subscription_id: id.clone(),
          session_key: sub.request.session_key.clone(),
        });
      }
      false
    });
    guest_interests
  }

  pub(crate) fn publish_session_updates(&self, key: Option<&str>) -> Vec<SessionUpdate> {
    let Ok(mut store) = self.updates.lock() else {
      return Vec::new();
    };
    store.expire_leases(Instant::now());
    let mut result = Vec::new();
    let all_sessions: std::collections::HashSet<_> = store
      .subscriptions
      .values()
      .filter(|subscription| subscription.session_scope.is_none() && subscription.request.level == UpdateLevel::All)
      .map(|subscription| subscription.request.session_key.clone())
      .collect();
    let mut projections = HashMap::new();
    for subscription in store.subscriptions.values_mut() {
      if key.is_some_and(|key| key != subscription.request.session_key) {
        continue;
      }
      // A grant can hide related sessions and host diagnostics. Reproject
      // its updates with the original grant, including while the owner worker
      // publishes. Unrestricted clients can reuse their richer projection.
      let key = &subscription.request.session_key;
      let projection = if subscription.session_scope.is_some() {
        let mut service = self.clone();
        service.session_scope = subscription.session_scope.clone();
        service
          .prepare_projection(key, subscription.request.level == UpdateLevel::All, None)
          .map(Arc::new)
      } else {
        projections
          .entry(key.clone())
          .or_insert_with(|| {
            self
              .prepare_projection(key, all_sessions.contains(key), None)
              .map(Arc::new)
          })
          .clone()
      };
      let projection = match projection {
        Ok(projection) => projection,
        Err(error) => {
          if let Some(update) = subscription.failure(error) {
            result.push(update);
          }
          continue;
        }
      };
      let cursor = subscription.revision.to_string();
      let reset = source_generation(&subscription.state) != source_generation(&projection.page);
      let update = subscription.update_with_events(
        projection.page.clone(),
        projection.semantic.clone(),
        if subscription.request.level == UpdateLevel::All {
          projection.details.clone()
        } else {
          Vec::new()
        },
        if subscription.request.level == UpdateLevel::All {
          projection.events.clone()
        } else {
          Vec::new()
        },
        reset,
        Some(&cursor),
      );
      if update.snapshot || update.revision != cursor {
        result.push(update);
      }
    }
    store.enforce_budget();
    result
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn subscription(level: UpdateLevel) -> Subscription {
    Subscription {
      session_scope: None,
      request: OpenSessionRequest {
        subscription_id: "view".into(),
        session_key: "session".into(),
        level,
        resume: None,
        scope: None,
        history_cursor: None,
      },
      generation: "initial".into(),
      revision: 0,
      state_hash: String::new(),
      items: BTreeMap::new(),
      order: Vec::new(),
      semantic_order: Vec::new(),
      event_order: Vec::new(),
      state: Value::Null,
      accessed: Instant::now(),
      bytes: 0,
    }
  }

  fn message(id: &str, role: &str, delivery: &str, text: &str) -> Value {
    json!({"event_key":id,"type":"message","role":role,"delivery":delivery,"summary":text})
  }

  fn page(events: Vec<Value>, questions: Vec<Value>) -> Value {
    json!({"events":events,"total_events":0,"outstanding_questions":questions,"previous_cursor":null,"next_cursor":null,"history_status":"complete","attention_revision":null,"is_running":false})
  }

  #[test]
  fn genuine_model_separators_and_context_are_retained_at_every_delivery_level() {
    let model = json!({"model":{"model_provider":"test","model_id":"new","display_name":null},"source":"recorded","revision":"new"});
    for level in [
      UpdateLevel::Final,
      UpdateLevel::Messages,
      UpdateLevel::Steps,
      UpdateLevel::All,
    ] {
      for scoped in [false, true] {
        let mut subscription = subscription(level);
        if scoped {
          subscription.request.scope = Some(UpdateScope {
            history: HistoryScope::Retained,
            ..Default::default()
          });
        }
        let mut page = page(
          vec![json!({"event_key":"change","type":"model_changed","summary":"old → new","is_bookkeeping":false})],
          vec![],
        );
        page["current_model"] = model.clone();
        let semantic = page["events"].as_array().unwrap().clone();
        let projected = subscription.update(page, semantic, Vec::new(), true, None);
        assert!(
          projected
            .items
            .iter()
            .any(|item| item["summary"]["type"] == "model_changed"
              && item["kind"] == "notification"
              && item["level"] == "final"),
          "{level:?}: {:?}",
          projected.items
        );
        assert_eq!(projected.state["current_model"], model);
      }
    }
  }

  #[test]
  fn renewal_acknowledges_present_and_missing_ids_without_reading_source_history() {
    let root = tempfile::tempdir().unwrap();
    let service = ViewerService::native(root.path().join("index.sqlite")).unwrap();
    let mut active = subscription(UpdateLevel::Steps);
    // This identity cannot load a body. Renewal still succeeds because it
    // inspects only the existing lease, preserving its accepted projection.
    active.request.session_key = "unresolvable-body-identity".into();
    active.revision = 42;
    active.state = json!({"is_running":true});
    active.accessed = Instant::now() - LEASE / 2;
    let previous_access = active.accessed;
    service
      .updates
      .lock()
      .unwrap()
      .subscriptions
      .insert("view".into(), active);

    let renewal = service
      .renew_session_subscriptions(&["view".into(), "missing".into(), "view".into(), "missing".into()])
      .unwrap();
    assert_eq!(
      serde_json::to_value(renewal).unwrap(),
      json!({"renewed_ids":["view"],"missing_ids":["missing"]})
    );
    let store = service.updates.lock().unwrap();
    let renewed = &store.subscriptions["view"];
    assert!(renewed.accessed > previous_access);
    assert_eq!(renewed.revision, 42);
    assert_eq!(renewed.generation, "initial");
    assert_eq!(renewed.state, json!({"is_running":true}));
    assert_eq!(store.subscriptions.len(), 1);
  }

  #[test]
  fn renewal_expires_leases_before_lookup_and_never_resurrects_them() {
    let root = tempfile::tempdir().unwrap();
    let service = ViewerService::native(root.path().join("index.sqlite")).unwrap();
    let mut expired = subscription(UpdateLevel::Steps);
    expired.accessed = Instant::now() - LEASE;
    let mut other_expired = subscription(UpdateLevel::Steps);
    other_expired.accessed = expired.accessed;
    service
      .updates
      .lock()
      .unwrap()
      .subscriptions
      .extend([("expired".into(), expired), ("unrequested".into(), other_expired)]);

    let renewal = service.renew_session_subscriptions(&["expired".into()]).unwrap();
    assert_eq!(renewal.renewed_ids, Vec::<String>::new());
    assert_eq!(renewal.missing_ids, ["expired"]);
    assert!(service.updates.lock().unwrap().subscriptions.is_empty());
    assert_eq!(
      service.renew_session_subscriptions(&["expired".into()]).unwrap(),
      renewal
    );
  }

  #[test]
  fn releasing_an_open_in_progress_prevents_late_registration() {
    let root = tempfile::tempdir().unwrap();
    let service = ViewerService::native(root.path().join("index.sqlite")).unwrap();
    let request = subscription(UpdateLevel::Steps).request;
    let first = service.updates.lock().unwrap().begin_open(&request, &None).unwrap();
    service
      .release_session(SessionReleaseRequest {
        subscription_id: request.subscription_id.clone(),
        session_key: request.session_key.clone(),
      })
      .unwrap();
    let mut store = service.updates.lock().unwrap();
    let successor = store.begin_open(&request, &None).unwrap();
    assert!(store.finish_open(&request.subscription_id, &first).is_err());
    assert!(store.finish_open(&request.subscription_id, &successor).is_ok());
    assert!(store.opening.is_empty());
    assert!(store.subscriptions.is_empty());
  }

  #[test]
  fn dormant_baselines_yield_to_live_leases_and_require_exact_revision() {
    let mut store = UpdateStore::default();
    let mut first = subscription(UpdateLevel::Steps);
    first.revision = 5;
    first.bytes = MEMORY_TARGET / 2;
    let mut request = first.request.clone();
    store.subscriptions.insert("view".into(), first);
    store.release("view");
    request.subscription_id = "reopen".into();
    let stale = SessionResume {
      subscription_id: "view".into(),
      generation: "initial".into(),
      revision: "4".into(),
      state_hash: String::new(),
    };
    let (opened, cursor) = store.open(&request, &None, Some(&stale));
    assert!(cursor.is_none());
    assert_ne!(opened.generation, "initial");
    opened.bytes = MEMORY_TARGET;
    store.enforce_budget();
    assert!(store.dormant.is_empty());
    assert!(store.subscriptions.contains_key("reopen"));
  }

  #[test]
  fn scoped_projection_failures_hide_reader_diagnostics() {
    let mut guest = subscription(UpdateLevel::Steps);
    guest.session_scope = Some(Arc::new(HashSet::from(["session".into()])));
    let update = guest.failure("private host diagnostic".into()).unwrap();
    let wire = serde_json::to_string(&update).unwrap();
    assert!(!wire.contains("private host diagnostic"));
    assert_eq!(update.state["error"], "Session updates are temporarily unavailable");
  }

  #[tokio::test]
  async fn source_resets_emit_one_owner_signal_and_only_active_grant_invalidations() {
    let root = tempfile::tempdir().unwrap();
    let service = ViewerService::native(root.path().join("index.sqlite")).unwrap();
    let (events, mut received) = tokio::sync::broadcast::channel(16);
    {
      let mut store = service.updates.lock().unwrap();
      for (id, key, guest, dormant) in [
        ("owner", "replaced", false, false),
        ("guest", "replaced", true, false),
        ("away", "replaced", true, true),
        ("other", "unchanged", false, false),
        ("other-away", "unchanged", false, true),
      ] {
        let mut sub = subscription(UpdateLevel::All);
        sub.request.session_key = key.into();
        sub.request.subscription_id = id.into();
        sub.revision = 3;
        sub.session_scope = guest.then(|| Arc::new(HashSet::from([key.to_owned()])));
        store.subscriptions.insert(id.into(), sub);
        if dormant {
          store.release(id);
        }
      }
    }
    let change = |session_key| crate::relay::RelayChange {
      session_key,
      reset: true,
    };
    crate::runtime::publish_relay_change(service.clone(), &events, change(Some("replaced".into()))).await;
    let guest = received.try_recv().unwrap();
    assert_eq!(guest.event, "session-subscription-invalidated");
    assert_eq!(
      guest.payload,
      json!({"subscription_id":"guest","session_key":"replaced"})
    );
    let owner = received.try_recv().unwrap();
    assert_eq!(owner.event, "relay-changed");
    assert_eq!(owner.payload, json!({"session_key":"replaced","reset":true}));
    assert!(received.try_recv().is_err());
    {
      let store = service.updates.lock().unwrap();
      assert_eq!(store.subscriptions.len(), 1);
      assert!(store.subscriptions.contains_key("other"));
      assert_eq!(store.dormant.len(), 1);
      assert!(store.dormant.contains_key("other-away"));
    }
    crate::runtime::publish_relay_change(service.clone(), &events, change(None)).await;
    assert_eq!(received.try_recv().unwrap().event, "relay-changed");
    assert!(received.try_recv().is_err());
    let store = service.updates.lock().unwrap();
    assert!(store.subscriptions.is_empty() && store.dormant.is_empty());
  }

  #[test]
  fn levels_keep_questions_and_errors_but_filter_intermediate_messages_and_tools() {
    let events = vec![
      message("u", "user", "unspecified", "hi"),
      message("step", "assistant", "commentary", "looking"),
      message("final", "assistant", "final", "done"),
      json!({"event_key":"tool","type":"tool_call"}),
      json!({"event_key":"q","type":"question_request"}),
      json!({"event_key":"error","type":"error"}),
    ];
    let final_update =
      subscription(UpdateLevel::Final).update(page(events.clone(), vec![]), events.clone(), vec![], true, None);
    assert_eq!(final_update.items.len(), 4);
    assert!(final_update.items.iter().all(|item| item["level"] == "final"));
    let messages =
      subscription(UpdateLevel::Messages).update(page(events.clone(), vec![]), events.clone(), vec![], true, None);
    assert_eq!(messages.items.len(), 5);
    assert!(
      messages
        .items
        .iter()
        .any(|item| item["item_id"] == "step" && item["level"] == "messages")
    );
    assert!(!messages.items.iter().any(|item| item["item_id"] == "tool"));
    let steps = subscription(UpdateLevel::Steps).update(page(events.clone(), vec![]), events, vec![], true, None);
    assert_eq!(steps.items.len(), 6);
  }

  #[test]
  fn all_tracks_every_source_record_independently_of_semantic_folding_and_detail_keys() {
    let events: Vec<_> = (0..24)
      .map(|index| json!({"event_key":format!("event.v1.{index}"),"event":{"type":"lifecycle","sequence":index}}))
      .collect();
    let mut state = subscription(UpdateLevel::All);
    let first = state.update_with_events(page(vec![], vec![]), vec![], vec![], events.clone(), true, None);
    assert_eq!(first.items.len(), 24);
    assert_eq!(first.event_order.as_ref().unwrap().len(), 24);
    assert_eq!(first.item_order, Some(vec![]));
    let mut changed = events.clone();
    changed[3]["event"]["sequence"] = json!(99);
    let next = state.update_with_events(
      page(vec![], vec![]),
      vec![],
      vec![],
      changed,
      false,
      Some(&first.revision),
    );
    assert_eq!(next.items.len(), 1);
    assert_eq!(next.items[0]["item_id"], "event:event.v1.3");
    assert!(next.event_order.is_none());
    let removed = state.update_with_events(
      page(vec![], vec![]),
      vec![],
      vec![],
      events[..3].to_vec(),
      false,
      Some(&next.revision),
    );
    assert_eq!(removed.removed_items.len(), 21);
    let edit = removed.order_edits.unwrap().event_order.unwrap();
    assert_eq!((edit.start, edit.delete_count), (3, 21));
    assert!(edit.insert.is_empty());
    let steps =
      subscription(UpdateLevel::Steps).update_with_events(page(vec![], vec![]), vec![], vec![], events, true, None);
    assert!(steps.items.is_empty());
    assert_eq!(steps.event_order, Some(vec![]));
  }

  #[test]
  fn appends_send_only_changed_items_and_no_unchanged_order() {
    let mut state = subscription(UpdateLevel::Steps);
    let initial = vec![
      message("u", "user", "unspecified", "hi"),
      message("a", "assistant", "commentary", "reading"),
    ];
    let first = state.update(page(initial.clone(), vec![]), initial.clone(), vec![], true, None);
    let mut next = initial;
    next[1]["summary"] = json!("read complete");
    let update = state.update(
      page(next.clone(), vec![]),
      next.clone(),
      vec![],
      false,
      Some(&first.revision),
    );
    assert!(!update.snapshot);
    assert_eq!(update.items.len(), 1);
    assert!(update.item_order.is_none());
    assert!(update.order_edits.is_none());
    assert_eq!(update.base_state_hash.as_ref(), Some(&first.state_hash));
    assert_ne!(update.state_hash, first.state_hash);
    let unchanged = state.update(page(next.clone(), vec![]), next, vec![], false, Some(&update.revision));
    assert_eq!(unchanged.revision, update.revision);
    assert_eq!(unchanged.state_hash, update.state_hash);
    assert_eq!(unchanged.base_state_hash.as_ref(), Some(&update.state_hash));
    assert!(unchanged.items.is_empty());
  }

  #[test]
  fn append_wire_size_is_independent_of_the_retained_order_length() {
    let mut subscription = subscription(UpdateLevel::Steps);
    let mut events: Vec<_> = (0..1000)
      .map(|i| message(&format!("existing-message-{i}"), "user", "unspecified", "earlier"))
      .collect();
    let first = subscription.update(page(events.clone(), vec![]), events.clone(), vec![], true, None);
    assert_eq!(first.state_hash.len(), 64);
    assert!(
      first
        .state_hash
        .bytes()
        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    );
    assert!(first.base_state_hash.is_none());
    events.push(message("new-message", "assistant", "final", "latest"));
    let next = subscription.update(
      page(events.clone(), vec![]),
      events,
      vec![],
      false,
      Some(&first.revision),
    );
    let wire = serde_json::to_string(&next).unwrap();
    assert!(wire.len() < 1500, "one appended message used {} bytes", wire.len());
    assert!(!wire.contains("existing-message-"));
    let edit = json!({"start":1000,"delete_count":0,"insert":["new-message"]});
    assert_eq!(
      json!(next.order_edits),
      json!({"item_order":edit,"semantic_order":edit})
    );
    assert_eq!(next.base_state_hash.as_ref(), Some(&first.state_hash));
  }

  #[test]
  fn order_splice_reconstructs_insertion_deletion_and_reordering() {
    let before = ["a", "b", "c", "d"].map(String::from);
    for (after, start, delete_count, insert) in [
      (vec!["x", "a", "b", "c", "d"], 0, 0, vec!["x"]),
      (vec!["a", "c", "d"], 1, 1, vec![]),
      (vec!["a", "c", "b", "d"], 1, 2, vec!["c", "b"]),
      (vec![], 0, 4, vec![]),
    ] {
      let after: Vec<_> = after.into_iter().map(String::from).collect();
      let edit = OrderEdit::between(&before, &after).unwrap();
      assert_eq!((edit.start, edit.delete_count), (start, delete_count));
      assert_eq!(edit.insert, insert);
      let mut rebuilt = before.to_vec();
      rebuilt.splice(edit.start..edit.start + edit.delete_count, edit.insert);
      assert_eq!(rebuilt, after);
    }
    assert!(OrderEdit::between(&before, &before).is_none());
  }

  #[test]
  fn revision_commitment_binds_predecessor_and_order_for_equal_counters() {
    let mut left = subscription(UpdateLevel::Steps);
    let mut right = subscription(UpdateLevel::Steps);
    let a = message("a", "user", "unspecified", "A");
    let b = message("b", "user", "unspecified", "B");
    let initial =
      |sub: &mut Subscription, items: Vec<Value>| sub.update(page(items.clone(), vec![]), items, vec![], true, None);
    let first = initial(&mut left, vec![a.clone(), b.clone()]);
    let reversed = initial(&mut right, vec![b.clone(), a.clone()]);
    assert_ne!(first.state_hash, reversed.state_hash);
    let edit = |sub: &mut Subscription, cursor: &str| {
      let items = vec![a.clone(), b.clone(), message("c", "assistant", "final", "C")];
      sub.update(page(items.clone(), vec![]), items, vec![], false, Some(cursor))
    };
    let left_final = edit(&mut left, &first.revision);
    let right_final = edit(&mut right, &reversed.revision);
    assert_eq!(left_final.revision, right_final.revision);
    assert_ne!(left_final.state_hash, right_final.state_hash);
    assert_eq!(left_final.base_state_hash, Some(first.state_hash));
    assert_eq!(right_final.base_state_hash, Some(reversed.state_hash));
  }

  #[test]
  fn resolving_questions_changes_control_state_without_resending_messages() {
    let mut state = subscription(UpdateLevel::Final);
    let events = vec![message("u", "user", "unspecified", "hi")];
    let first = state.update(
      page(
        events.clone(),
        vec![json!({"event_key":"q","requires_input":true,"unanswered_count":1})],
      ),
      events.clone(),
      vec![],
      true,
      None,
    );
    let resolved = state.update(
      page(events.clone(), vec![]),
      events,
      vec![],
      false,
      Some(&first.revision),
    );
    assert!(resolved.items.is_empty());
    assert_eq!(resolved.state["outstanding_questions"], json!([]));
    assert_ne!(resolved.revision, first.revision);
    assert_ne!(resolved.state_hash, first.state_hash);
    assert_eq!(resolved.base_state_hash, Some(first.state_hash));
    assert!(resolved.order_edits.is_none());
  }

  #[test]
  fn work_groups_do_not_hide_semantic_step_messages() {
    let mut state = subscription(UpdateLevel::Steps);
    let grouped = vec![json!({"event_key":"work","type":"trajectory","trajectory":{"status":"working"}})];
    let semantic = vec![
      message("comment", "assistant", "commentary", "checking"),
      json!({"event_key":"tool","type":"tool_call"}),
    ];
    let mut current = page(grouped, vec![]);
    current["is_running"] = json!(true);
    let update = state.update(current, semantic, vec![], true, None);
    assert_eq!(update.items.len(), 2);
    assert_eq!(update.groups.len(), 1);
    assert_eq!(update.item_order, Some(vec!["work".into()]));
    assert_eq!(update.semantic_order, Some(vec!["comment".into(), "tool".into()]));
    assert_eq!(update.state["is_running"], true);
  }

  #[test]
  fn session_running_state_does_not_follow_historical_or_scoped_work_groups() {
    for (is_running, display_status) in [(false, "working"), (true, "unknown")] {
      let mut state = subscription(UpdateLevel::Steps);
      state.request.scope = Some(UpdateScope::default());
      let grouped = vec![
        json!({"event_key":"work","type":"trajectory","child_keys":["tool"],"trajectory":{"status":display_status}}),
      ];
      let semantic = vec![json!({"event_key":"tool","type":"tool_call"})];
      let mut current = page(grouped, vec![]);
      current["is_running"] = json!(is_running);
      let update = state.update(current, semantic, vec![], true, None);
      assert_eq!(update.state["is_running"], is_running);
    }
  }

  #[test]
  fn gaps_and_replacements_produce_complete_snapshots() {
    let mut state = subscription(UpdateLevel::Steps);
    let events = vec![message("u", "user", "unspecified", "hi")];
    let first = state.update(page(events.clone(), vec![]), events.clone(), vec![], true, None);
    let gap = state.update(
      page(events.clone(), vec![]),
      events.clone(),
      vec![],
      false,
      Some("missing"),
    );
    assert!(gap.snapshot);
    assert_eq!(gap.items.len(), 1);
    let reset = state.update(page(vec![], vec![]), vec![], vec![], true, Some(&first.revision));
    assert!(reset.snapshot);
    assert_ne!(reset.generation, first.generation);
    assert_eq!(reset.item_order, Some(vec![]));
  }

  #[test]
  fn details_are_delivered_and_removed_at_item_scope() {
    let mut state = subscription(UpdateLevel::All);
    let first = state.update(
      page(vec![], vec![]),
      vec![],
      vec![("tool".into(), json!({"output":"first"}))],
      true,
      None,
    );
    let next = state.update(
      page(vec![], vec![]),
      vec![],
      vec![("tool".into(), json!({"output":"second"}))],
      false,
      Some(&first.revision),
    );
    assert_eq!(next.items[0]["detail"]["output"], "second");
    let removed = state.update(page(vec![], vec![]), vec![], vec![], false, Some(&next.revision));
    assert_eq!(removed.removed_items, vec!["detail:tool"]);
  }
  #[test]
  fn source_errors_preserve_display_and_recovery_retires_the_notification() {
    let mut state = subscription(UpdateLevel::Steps);
    let events = vec![message("u", "user", "unspecified", "hi")];
    let first = state.update(page(events.clone(), vec![]), events.clone(), vec![], true, None);
    let failed = state.failure("source unavailable".into()).unwrap();
    assert_eq!(failed.base_state_hash.as_ref(), Some(&first.state_hash));
    assert_ne!(failed.state_hash, first.state_hash);
    assert_eq!(failed.items[0]["notification"]["message"], "source unavailable");
    assert!(state.failure("source unavailable".into()).is_none());
    let recovered = state.update(
      page(events.clone(), vec![]),
      events,
      vec![],
      false,
      Some(&failed.revision),
    );
    assert!(recovered.items.is_empty());
    assert_eq!(recovered.removed_items, vec!["notification:source-error"]);
    assert_eq!(recovered.base_state_hash.as_ref(), Some(&failed.state_hash));
    assert_ne!(recovered.state_hash, failed.state_hash);
    assert!(recovered.state.get("error").is_none());
  }
}
