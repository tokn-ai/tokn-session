//! Semantic delivery levels. Source history stays authoritative; this bounded
//! store holds only subscribed display objects and replaces snapshots on gaps.
use crate::{
  ViewerService,
  model::{EventPageRequest, HistoryWindowMode, LoadEventDetailRequest, PageDirection},
  relay::RelayChange,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
  collections::{BTreeMap, HashMap},
  sync::Arc,
  time::{Duration, Instant},
};

mod scope;
pub use scope::{HistoryScope, UpdateScope};

const MAX_SUBSCRIPTIONS: usize = 24;
const LEASE: Duration = Duration::from_secs(90);
const MEMORY_TARGET: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq, Ord, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum UpdateLevel {
  Final,
  #[default]
  Steps,
  Details,
  All,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SessionUpdatesRequest {
  pub subscription_id: String,
  pub session_key: String,
  #[serde(default)]
  pub level: UpdateLevel,
  pub cursor: Option<String>,
  /// One-shot retained-history expansion, separate from the revision cursor.
  #[serde(default)]
  pub history_cursor: Option<String>,
  #[serde(default)]
  pub detail_keys: Vec<String>,
  #[serde(default)]
  pub scope: Option<UpdateScope>,
  #[serde(default)]
  pub unsubscribe: bool,
}

#[derive(Debug, Serialize)]
pub struct SessionUpdate {
  pub subscription_id: String,
  pub session_key: String,
  pub level: UpdateLevel,
  pub generation: String,
  pub base_revision: Option<String>,
  pub revision: String,
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
  pub state: Value,
}

struct Subscription {
  owner: Option<String>,
  request: SessionUpdatesRequest,
  generation: String,
  revision: u64,
  items: BTreeMap<String, Value>,
  order: Vec<String>,
  semantic_order: Vec<String>,
  event_order: Vec<String>,
  state: Value,
  accessed: Instant,
}

#[derive(Default)]
pub(crate) struct UpdateStore {
  subscriptions: HashMap<String, Subscription>,
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
        UpdateLevel::Steps
      },
    ),
    "question_reply" => ("user_message", UpdateLevel::Final),
    "question_request" | "error" => ("notification", UpdateLevel::Final),
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

impl UpdateStore {
  fn enforce_budget(&mut self) {
    while self
      .subscriptions
      .values()
      .map(|subscription| subscription.items.values().map(weight).sum::<usize>() + weight(&subscription.state))
      .sum::<usize>()
      > MEMORY_TARGET
    {
      let Some(oldest) = self
        .subscriptions
        .iter()
        .min_by_key(|(_, subscription)| subscription.accessed)
        .map(|(key, _)| key.clone())
      else {
        break;
      };
      self.subscriptions.remove(&oldest);
    }
  }
}

impl Subscription {
  fn failure(&mut self, error: String) -> Option<SessionUpdate> {
    let id = "notification:source-error".to_string();
    let item = json!({"item_id":id,"kind":"notification","level":"final","notification":{"type":"source_error","message":error}});
    if self.items.get(&id) == Some(&item) {
      return None;
    }
    let base_revision = self.revision.to_string();
    self.revision += 1;
    self.state["error"] = json!(error);
    self.items.insert(id, item.clone());
    Some(SessionUpdate {
      subscription_id: self.request.subscription_id.clone(),
      session_key: self.request.session_key.clone(),
      level: self.request.level,
      generation: self.generation.clone(),
      base_revision: Some(base_revision),
      revision: self.revision.to_string(),
      snapshot: false,
      items: vec![item],
      groups: Vec::new(),
      semantic_order: None,
      event_order: None,
      removed_items: Vec::new(),
      item_order: None,
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
      scope::project(
        page,
        semantic,
        events,
        scope,
        self.request.level >= UpdateLevel::Details,
      )
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
    state["is_running"] = json!(
      summaries
        .iter()
        .any(|summary| summary["trajectory"]["status"] == "working")
    );
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
    let changed = reset
      || self.items != items
      || self.order != order
      || self.event_order != event_order
      || self.semantic_order != semantic_order
      || self.state != state;
    if reset && self.revision != 0 {
      self.generation = uuid::Uuid::new_v4().to_string();
    }
    if changed {
      self.revision += 1;
    }
    let snapshot = reset || cursor != Some(old_revision.as_str());
    let additions: Vec<Value> = items
      .iter()
      .filter(|(key, value)| snapshot || self.items.get(*key) != Some(*value))
      .map(|(_, value)| value.clone())
      .collect();
    let removed = if snapshot {
      Vec::new()
    } else {
      self
        .items
        .keys()
        .filter(|key| !items.contains_key(*key))
        .cloned()
        .collect()
    };
    let item_order = (snapshot || self.order != order).then(|| order.clone());
    let semantic_order_update = (snapshot || self.semantic_order != semantic_order).then(|| semantic_order.clone());
    let event_order_update = (snapshot || self.event_order != event_order).then(|| event_order.clone());
    self.event_order = event_order;
    self.semantic_order = semantic_order;
    self.items = items;
    self.order = order;
    self.state = state;
    SessionUpdate {
      subscription_id: self.request.subscription_id.clone(),
      session_key: self.request.session_key.clone(),
      level: self.request.level,
      generation: self.generation.clone(),
      base_revision: (!snapshot).then_some(old_revision),
      revision: self.revision.to_string(),
      snapshot,
      groups: additions
        .iter()
        .filter(|item| item["kind"] == "work_summary")
        .cloned()
        .collect(),
      items: additions
        .into_iter()
        .filter(|item| item["kind"] != "work_summary")
        .collect(),
      semantic_order: semantic_order_update,
      event_order: event_order_update,
      removed_items: removed,
      item_order,
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

  fn prepare_details(&self, request: &SessionUpdatesRequest) -> Result<Vec<(String, Value)>, String> {
    let mut details = Vec::new();
    if request.level == UpdateLevel::Details {
      for key in &request.detail_keys {
        let detail = self.load_event_detail(LoadEventDetailRequest {
          session_key: request.session_key.clone(),
          event_key: key.clone(),
        })?;
        details.push((key.clone(), serde_json::to_value(detail).map_err(|e| e.to_string())?));
      }
    }
    Ok(details)
  }

  pub fn subscribe_session(&self, request: SessionUpdatesRequest) -> Result<Value, String> {
    self.subscribe_session_owned(request, None)
  }

  pub fn subscribe_session_owned(
    &self,
    request: SessionUpdatesRequest,
    owner: Option<String>,
  ) -> Result<Value, String> {
    self.validate_session_key(&request.session_key)?;
    if request.subscription_id.is_empty()
      || request.subscription_id.len() > 128
      || request.detail_keys.len() > 16
      || request.scope.as_ref().is_some_and(|scope| scope.group_keys.len() > 128)
    {
      return Err("Invalid session subscription".into());
    }
    let mut store = self.updates.lock().map_err(|_| "Update store lock poisoned")?;
    if request.unsubscribe {
      if store
        .subscriptions
        .get(&request.subscription_id)
        .is_some_and(|subscription| owner.is_none() || subscription.owner == owner)
      {
        store.subscriptions.remove(&request.subscription_id);
      }
      return Ok(json!({"subscription_id":request.subscription_id,"unsubscribed":true}));
    }
    store.subscriptions.retain(|_, value| value.accessed.elapsed() < LEASE);
    if !store.subscriptions.contains_key(&request.subscription_id) && store.subscriptions.len() >= MAX_SUBSCRIPTIONS {
      return Err("Too many session subscriptions".into());
    }
    let id = request.subscription_id.clone();
    let subscription = store.subscriptions.entry(id.clone()).or_insert_with(|| Subscription {
      owner: None,
      request: request.clone(),
      generation: uuid::Uuid::new_v4().to_string(),
      revision: 0,
      items: BTreeMap::new(),
      order: Vec::new(),
      semantic_order: Vec::new(),
      event_order: Vec::new(),
      state: Value::Null,
      accessed: Instant::now(),
    });
    if subscription.request.session_key != request.session_key {
      return Err("Subscription identity changed; use a new subscription id".into());
    }
    if subscription.request.level != request.level {
      subscription.revision = 0;
      subscription.generation = uuid::Uuid::new_v4().to_string();
      subscription.items.clear();
      subscription.order.clear();
      subscription.semantic_order.clear();
      subscription.event_order.clear();
      subscription.state = Value::Null;
    }
    if owner.is_some() {
      subscription.owner = owner;
    }
    subscription.request = request;
    subscription.accessed = Instant::now();
    Ok(json!({"subscription_id":id,"generation":subscription.generation,"revision":subscription.revision.to_string()}))
  }

  pub fn renew_session_subscriptions(&self, ids: &[String]) -> Result<(), String> {
    let mut store = self.updates.lock().map_err(|_| "Update store lock poisoned")?;
    for id in ids {
      if let Some(subscription) = store.subscriptions.get_mut(id) {
        subscription.accessed = Instant::now();
      }
    }
    Ok(())
  }

  pub fn release_session_subscriptions(&self, owner: &str) {
    if let Ok(mut store) = self.updates.lock() {
      store
        .subscriptions
        .retain(|_, subscription| subscription.owner.as_deref() != Some(owner));
    }
  }

  pub fn load_session_backward(&self, mut request: SessionUpdatesRequest) -> Result<SessionUpdate, String> {
    // Backward reads always return a complete selected projection. Revision
    // cursors belong to live diffs, not pagination or cache coverage.
    request.cursor = None;
    if request.scope.is_none() {
      request.scope = Some(UpdateScope {
        history: if request.history_cursor.is_some() {
          HistoryScope::Retained
        } else {
          HistoryScope::LatestTurn
        },
        ..Default::default()
      });
    }
    self.load_session_updates(request)
  }

  pub fn load_session_updates(&self, request: SessionUpdatesRequest) -> Result<SessionUpdate, String> {
    self.validate_session_key(&request.session_key)?;
    if request.subscription_id.is_empty()
      || request.subscription_id.len() > 128
      || request.detail_keys.len() > 16
      || request.scope.as_ref().is_some_and(|scope| scope.group_keys.len() > 128)
    {
      return Err("Invalid session subscription".into());
    }
    if request.unsubscribe || (request.level == UpdateLevel::Details && request.detail_keys.is_empty()) {
      let mut store = self.updates.lock().map_err(|_| "Update store lock poisoned")?;
      if store
        .subscriptions
        .get(&request.subscription_id)
        .is_some_and(|subscription| subscription.request.session_key == request.session_key)
      {
        store.subscriptions.remove(&request.subscription_id);
      }
      return Ok(SessionUpdate {
        subscription_id: request.subscription_id,
        session_key: request.session_key,
        level: request.level,
        generation: uuid::Uuid::new_v4().to_string(),
        base_revision: None,
        revision: "0".into(),
        snapshot: true,
        items: Vec::new(),
        groups: Vec::new(),
        semantic_order: Some(Vec::new()),
        event_order: Some(Vec::new()),
        removed_items: Vec::new(),
        item_order: Some(Vec::new()),
        state: json!({"total_events":0,"previous_cursor":null,"next_cursor":null,"history_status":"complete","attention_revision":null,"outstanding_questions":[]}),
      });
    }
    // Serialize registration and publication: no source update can fall into
    // the gap between producing the initial snapshot and registering interest.
    let mut store = self.updates.lock().map_err(|_| "Update store lock poisoned")?;
    store.subscriptions.retain(|_, value| value.accessed.elapsed() < LEASE);
    if !store.subscriptions.contains_key(&request.subscription_id) && store.subscriptions.len() >= MAX_SUBSCRIPTIONS {
      let oldest = store
        .subscriptions
        .iter()
        .min_by_key(|(_, value)| value.accessed)
        .map(|(key, _)| key.clone())
        .unwrap();
      store.subscriptions.remove(&oldest);
    }
    let projection = self.prepare_projection(
      &request.session_key,
      request.level == UpdateLevel::All,
      request.history_cursor.as_deref(),
    )?;
    let details = if request.level == UpdateLevel::All {
      projection.details
    } else {
      self.prepare_details(&request)?
    };
    let subscription = store
      .subscriptions
      .entry(request.subscription_id.clone())
      .or_insert_with(|| Subscription {
        owner: None,
        request: request.clone(),
        generation: uuid::Uuid::new_v4().to_string(),
        revision: 0,
        items: BTreeMap::new(),
        order: Vec::new(),
        semantic_order: Vec::new(),
        event_order: Vec::new(),
        state: Value::Null,
        accessed: Instant::now(),
      });
    let reset = subscription.revision == 0
      || subscription.request.session_key != request.session_key
      || subscription.request.level != request.level;
    subscription.request = request.clone();
    subscription.accessed = Instant::now();
    let update = subscription.update_with_events(
      projection.page,
      projection.semantic,
      details,
      projection.events,
      reset,
      request.cursor.as_deref(),
    );
    store.enforce_budget();
    Ok(update)
  }

  pub(crate) fn publish_session_updates(&self, change: &RelayChange) -> Vec<SessionUpdate> {
    let Ok(mut store) = self.updates.lock() else {
      return Vec::new();
    };
    store.subscriptions.retain(|_, value| value.accessed.elapsed() < LEASE);
    let mut result = Vec::new();
    let all_sessions: std::collections::HashSet<_> = store
      .subscriptions
      .values()
      .filter(|subscription| subscription.request.level == UpdateLevel::All)
      .map(|subscription| subscription.request.session_key.clone())
      .collect();
    let mut projections = HashMap::new();
    for subscription in store.subscriptions.values_mut() {
      if change
        .session_key
        .as_ref()
        .is_some_and(|key| key != &subscription.request.session_key)
      {
        continue;
      }
      // A newly registered live interest has no baseline until its backward
      // snapshot is accepted. That snapshot reads the latest source under this
      // same lock; queued publication then diffs against it without a gap.
      if subscription.revision == 0 {
        continue;
      }
      // Project each changed session once, regardless of how many levels or
      // clients subscribe to it. Full payloads are built only when an all-level subscriber needs them.
      let projection = projections
        .entry(subscription.request.session_key.clone())
        .or_insert_with(|| {
          self
            .prepare_projection(
              &subscription.request.session_key,
              all_sessions.contains(&subscription.request.session_key),
              None,
            )
            .map(Arc::new)
        });
      let (projection, details) = match projection.clone().and_then(|projection| {
        let details = if subscription.request.level == UpdateLevel::All {
          Ok(projection.details.clone())
        } else {
          self.prepare_details(&subscription.request)
        }?;
        Ok((projection, details))
      }) {
        Ok(value) => value,
        Err(error) => {
          if let Some(update) = subscription.failure(error) {
            result.push(update);
          }
          continue;
        }
      };
      let cursor = subscription.revision.to_string();
      let update = subscription.update_with_events(
        projection.page.clone(),
        projection.semantic.clone(),
        details,
        if subscription.request.level == UpdateLevel::All {
          projection.events.clone()
        } else {
          Vec::new()
        },
        change.reset,
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
      owner: None,
      request: SessionUpdatesRequest {
        subscription_id: "view".into(),
        session_key: "session".into(),
        level,
        cursor: None,
        detail_keys: Vec::new(),
        scope: None,
        history_cursor: None,
        unsubscribe: false,
      },
      generation: "initial".into(),
      revision: 0,
      items: BTreeMap::new(),
      order: Vec::new(),
      semantic_order: Vec::new(),
      event_order: Vec::new(),
      state: Value::Null,
      accessed: Instant::now(),
    }
  }

  fn message(id: &str, role: &str, delivery: &str, text: &str) -> Value {
    json!({"event_key":id,"type":"message","role":role,"delivery":delivery,"summary":text})
  }

  fn page(events: Vec<Value>, questions: Vec<Value>) -> Value {
    json!({"events":events,"total_events":0,"outstanding_questions":questions,"previous_cursor":null,"next_cursor":null,"history_status":"complete","attention_revision":null})
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
    assert_eq!(removed.event_order.unwrap().len(), 3);
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
    let unchanged = state.update(page(next.clone(), vec![]), next, vec![], false, Some(&update.revision));
    assert_eq!(unchanged.revision, update.revision);
    assert!(unchanged.items.is_empty());
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
  }

  #[test]
  fn work_groups_do_not_hide_semantic_step_messages() {
    let mut state = subscription(UpdateLevel::Steps);
    let grouped = vec![json!({"event_key":"work","type":"trajectory","trajectory":{"status":"working"}})];
    let semantic = vec![
      message("comment", "assistant", "commentary", "checking"),
      json!({"event_key":"tool","type":"tool_call"}),
    ];
    let update = state.update(page(grouped, vec![]), semantic, vec![], true, None);
    assert_eq!(update.items.len(), 2);
    assert_eq!(update.groups.len(), 1);
    assert_eq!(update.item_order, Some(vec!["work".into()]));
    assert_eq!(update.semantic_order, Some(vec!["comment".into(), "tool".into()]));
    assert_eq!(update.state["is_running"], true);
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
    let mut state = subscription(UpdateLevel::Details);
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
    state.update(page(events.clone(), vec![]), events.clone(), vec![], true, None);
    let failed = state.failure("source unavailable".into()).unwrap();
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
    assert!(recovered.state.get("error").is_none());
  }
}
