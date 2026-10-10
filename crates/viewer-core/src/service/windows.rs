//! Retained history windows and their stable, generation-scoped identities.
//! Source positions stay absolute when earlier turns are added to a window.
use super::*;
use crate::model::{HistoryWindowMode, SessionViewRequest, hex_decode, hex_encode};

#[derive(Default)]
pub(crate) struct UpdatePayloads {
  pub source_revision: Option<String>,
  pub details: Vec<EventDetail>,
  pub events: Vec<EventDetail>,
}

impl ViewerService {
  pub fn update_session_view(&self, request: SessionViewRequest) -> Result<(), String> {
    if request.view_id.is_empty() || request.view_id.len() > 128 {
      return Err("Invalid viewer identity".into());
    }
    if request.candidate_session_keys.len() > 128 {
      return Err("Too many background session candidates".into());
    }
    let selected = request
      .session_key
      .as_deref()
      .map(|key| {
        self.validate_session_key(key)?;
        decode_session_key(key)
      })
      .transpose()?;
    let mut candidates = Vec::new();
    for key in &request.candidate_session_keys {
      let locator = decode_session_key(key)?;
      // A sidebar page can race a delete/relocation. Discard stale candidates
      // instead of losing the lease on the currently selected conversation.
      if !candidates.contains(&locator) && self.validate_session_key(key).is_ok() {
        candidates.push(locator);
      }
    }
    self
      .relay
      .update_view(&request.view_id, request.revision, selected, candidates)
  }

  pub(super) fn window_identity(
    &self,
    locator: &SessionLocator,
    loaded: &Arc<LoadedSession>,
    scoped: bool,
  ) -> Result<WindowIdentity, String> {
    match self.relay.window_info(locator, loaded) {
      Some(info) => Ok(WindowIdentity {
        event_offset: info.event_offset,
        generation: scoped.then_some(info.generation),
      }),
      None if self.relay.covers(locator.provider) => Err("Session unloaded; reload its history".into()),
      None => Ok(WindowIdentity::default()),
    }
  }

  pub(super) fn load_retained_event_page(&self, request: EventPageRequest) -> Result<EventPage, String> {
    self.load_retained_pages(request, false, false).map(|(page, _, _)| page)
  }

  pub(crate) fn load_update_pages(
    &self,
    request: EventPageRequest,
    include_all: bool,
  ) -> Result<(EventPage, Vec<EventSummary>, UpdatePayloads), String> {
    self.load_retained_pages(request, true, include_all)
  }

  fn load_retained_pages(
    &self,
    request: EventPageRequest,
    include_semantic: bool,
    include_all: bool,
  ) -> Result<(EventPage, Vec<EventSummary>, UpdatePayloads), String> {
    if request.offset.is_some() || !matches!(request.direction, PageDirection::Backward) {
      return Err("History windows require backward pagination without an offset".into());
    }
    let locator = decode_session_key(&request.session_key)?;
    let attention_revision = self.attention_revision_for_locator(&locator);
    let mut loaded = if self.relay.covers(locator.provider) {
      if request.window_mode == Some(HistoryWindowMode::Earlier) {
        self.relay.load(&locator)?
      } else {
        self.relay.advance(&locator)?
      }
    } else {
      self.load_verified(&locator)?
    };
    let mut info = self.relay.window_info(&locator, &loaded);
    if request.window_mode == Some(HistoryWindowMode::Earlier) {
      let cursor = request.cursor.as_deref().ok_or("Earlier history requires a cursor")?;
      let before = decode_history_cursor(cursor, info.as_ref().map(|info| info.generation.as_str()))?;
      if let Some(current) = &info {
        if before < current.event_offset {
          return Err("History window changed; refresh before loading earlier turns".into());
        }
        if before == current.event_offset && current.has_earlier {
          loaded = self.relay.load_earlier(&locator, before)?;
          info = self.relay.window_info(&locator, &loaded);
        }
      }
    } else if request.cursor.is_some() {
      return Err("A retained history refresh does not accept a cursor".into());
    }
    // The fallback serves repository-only embeddings and tests. Native app
    // Automatic and Local modes both use the shared windowed reader above.
    let (start, has_earlier) = if let Some(info) = &info {
      (0, info.has_earlier)
    } else {
      let mut cache = self
        .loaded_session_cache
        .lock()
        .map_err(|_| "Session cache lock poisoned")?;
      let cached = cache.as_mut().filter(|cache| Arc::ptr_eq(&cache.loaded, &loaded));
      let previous = cached.as_ref().and_then(|cache| cache.retained_start);
      let start = if request.window_mode == Some(HistoryWindowMode::Earlier) {
        let before = decode_history_cursor(request.cursor.as_deref().unwrap(), None)?;
        previous.unwrap_or(before).min(turn_start(&loaded.events, before))
      } else {
        previous.unwrap_or_else(|| turn_start(&loaded.events, loaded.events.len()))
      };
      let mut questions = crate::questions::Questions::default();
      for (index, event) in loaded.events.iter().enumerate() {
        questions.observe(event, index);
      }
      let start = questions.first_index().map_or(start, |question| start.min(question));
      if let Some(cache) = cached {
        cache.retained_start = Some(start);
      }
      (start, start > 0)
    };
    let identity = self.window_identity(&locator, &loaded, true)?;
    let timeline = timeline_entries(&loaded.events);
    let entries: Vec<_> = timeline
      .iter()
      .filter(|entry| timeline_entry_start_source_event_index(entry).is_some_and(|index| index >= start))
      .collect();
    let targets = entries
      .iter()
      .any(|entry| timeline_entry_has_targeted_agent_activity(entry, &loaded.events))
      .then(|| self.delegation_targets_for_parent(&locator))
      .unwrap_or_default();
    let intermediate_usage = usage_filter::intermediate_usage(&loaded.events);
    let events = entries
      .iter()
      .map(|entry| {
        identity.summary(timeline_entry_event_summary(
          entry,
          &loaded.events,
          &targets,
          &intermediate_usage,
        ))
      })
      .collect::<Vec<_>>();
    // Intermediate assistant messages and individual tool operations remain
    // semantic objects even when the current UI folds them into a trajectory.
    let base = if include_semantic {
      base_timeline_entries(&loaded.events)
    } else {
      Vec::new()
    };
    let semantic = if include_semantic {
      base
        .iter()
        .filter(|entry| timeline_entry_start_source_event_index(entry).is_some_and(|index| index >= start))
        .map(|entry| {
          identity.summary(timeline_entry_event_summary(
            entry,
            &loaded.events,
            &targets,
            &intermediate_usage,
          ))
        })
        .collect()
    } else {
      Vec::new()
    };
    let mut payloads = UpdatePayloads {
      source_revision: info
        .as_ref()
        .map(|info| format!("{}:{}", info.generation, info.revision)),
      ..Default::default()
    };
    if include_all {
      // Source rows stay individual even when tools/compaction/work are folded.
      // Keep the same generation and absolute positions as display identities.
      for (index, event) in loaded.events.iter().enumerate().skip(start) {
        let mut detail = source_event_detail(encode_event_key(index), event)?;
        if !detail.is_hidden && detail.event.get("redacted") != Some(&Value::Bool(true)) {
          if let Some(native) = self.relay.native(&locator, index, &loaded) {
            detail.native = Some(bounded_detail_value(native, "native")?);
          }
        }
        payloads.events.push(identity.detail(detail, false));
      }
      for entry in base
        .into_iter()
        .filter(|entry| timeline_entry_start_source_event_index(entry).is_some_and(|index| index >= start))
      {
        let (detail, envelope) = if let TimelineEntry::Event { source_event_index } = &entry
          && matches!(loaded.events[*source_event_index], AgentEvent::Compaction(_))
        {
          self.load_event_detail_local(&locator, &loaded, encode_event_key(*source_event_index))?
        } else {
          self.timeline_entry_detail(&locator, &loaded, entry)?
        };
        payloads.details.push(identity.detail(detail, envelope));
      }
      for entry in entries {
        if let TimelineEntry::Trajectory { .. } = entry {
          let (detail, envelope) = self.timeline_entry_detail(&locator, &loaded, entry.clone())?;
          payloads.details.push(identity.detail(detail, envelope));
        }
      }
    }
    Ok((
      EventPage {
        total_events: events.len(),
        events,
        next_cursor: None,
        previous_cursor: has_earlier
          .then(|| encode_history_cursor(identity.generation.as_deref(), identity.event_offset + start)),
        history_status: loaded.history_status.into(),
        follow_error: self.page_follow_error(&locator),
        attention_revision,
        outstanding_questions: outstanding_questions(&loaded.events, &identity),
      },
      semantic,
      payloads,
    ))
  }
}

fn turn_start(events: &[AgentEvent], before: usize) -> usize {
  events[..before.min(events.len())]
    .iter()
    .enumerate()
    .rev()
    .filter(
      |(_, event)| matches!(event, AgentEvent::Message(message) if message.role == Role::User && !event.is_hidden()),
    )
    .nth(2)
    .map(|(index, _)| index)
    .unwrap_or(0)
}

fn encode_history_cursor(generation: Option<&str>, start: usize) -> String {
  format!(
    "history.v1.{}.{start:x}",
    hex_encode(generation.unwrap_or("").as_bytes())
  )
}

fn decode_history_cursor(cursor: &str, generation: Option<&str>) -> Result<usize, String> {
  let (encoded, start) = cursor
    .strip_prefix("history.v1.")
    .and_then(|value| value.split_once('.'))
    .ok_or("Invalid history cursor")?;
  if hex_decode(encoded)? != generation.unwrap_or("").as_bytes() {
    return Err("History changed; refresh before loading earlier turns".into());
  }
  usize::from_str_radix(start, 16).map_err(|_| "Invalid history position".into())
}

#[derive(Default)]
pub(super) struct WindowIdentity {
  event_offset: usize,
  generation: Option<String>,
}

impl WindowIdentity {
  fn scoped(&self, key: String) -> String {
    match &self.generation {
      Some(generation) => format!("window.v1.{}.{}", hex_encode(generation.as_bytes()), key),
      None => key,
    }
  }

  fn unscoped<'a>(&self, key: &'a str) -> Result<&'a str, String> {
    if let Some(rest) = key.strip_prefix("window.v1.") {
      let (generation, key) = rest.split_once('.').ok_or("Invalid window identity")?;
      if Some(hex_decode(generation)?.as_slice()) != self.generation.as_ref().map(|value| value.as_bytes()) {
        return Err("History changed; reload the session before opening this item".into());
      }
      Ok(key)
    } else if self.generation.is_some() {
      Err("Missing history generation".into())
    } else {
      Ok(key)
    }
  }

  /// Only locally generated identities enter this function.
  pub(super) fn key(&self, key: &str) -> String {
    self.scoped(translate_key(key, |index| index.checked_add(self.event_offset)).expect("valid source position"))
  }

  pub(super) fn local_key(&self, key: &str) -> Result<String, String> {
    translate_key(self.unscoped(key)?, |index| index.checked_sub(self.event_offset))
  }

  pub(super) fn summary(&self, mut summary: EventSummary) -> EventSummary {
    if self.generation.is_some() {
      summary.slot_key = Some(
        translate_key(&summary.event_key, |index| index.checked_add(self.event_offset)).expect("valid source position"),
      );
    }
    if let Some(keys) = &mut summary.child_keys {
      for key in keys {
        *key = self.key(key);
      }
    }
    summary.event_key = self.key(&summary.event_key);
    summary
  }

  pub(super) fn detail(&self, mut detail: EventDetail, native_envelope: bool) -> EventDetail {
    detail.event_key = self.key(&detail.event_key);
    if let Some(output) = &mut detail.tool_output {
      output.source_event_key = self.key(&output.source_event_key);
    }
    // Translate only viewer-owned envelopes; provider payloads can contain
    // identically named fields with unrelated meanings and remain untouched.
    if detail.event.get("type").and_then(Value::as_str) == Some("trajectory") {
      self.envelope_keys(&mut detail.event);
    }
    if let Some(indices) = detail
      .event
      .get_mut("source_event_indices")
      .and_then(Value::as_array_mut)
    {
      for value in indices {
        if let Some(index) = value.as_u64() {
          *value = json!(index.saturating_add(self.event_offset as u64));
        }
      }
    }
    if let Some(keys) = detail.event.get_mut("source_event_keys").and_then(Value::as_array_mut) {
      for key in keys {
        self.source_position(key);
      }
    }
    if let Some(id) = detail.event.get_mut("id")
      && id.get("kind").and_then(Value::as_str) == Some("uncorrelated")
    {
      self.source_position(id);
    }
    if native_envelope && let Some(native) = &mut detail.native {
      self.envelope_keys(native);
    }
    detail
  }

  fn source_position(&self, value: &mut Value) {
    if let Some(index) = value.get_mut("source_event_index")
      && let Some(position) = index.as_u64()
    {
      *index = json!(position.saturating_add(self.event_offset as u64));
    }
  }

  fn envelope_keys(&self, value: &mut Value) {
    if let Some(key) = value.get_mut("anchor_event_key")
      && let Some(text) = key.as_str()
    {
      *key = Value::String(self.key(text));
    }
    if let Some(records) = value.get_mut("source_records").and_then(Value::as_array_mut) {
      for record in records {
        if let Some(key) = record.get_mut("event_key")
          && let Some(text) = key.as_str()
        {
          *key = Value::String(self.key(text));
        }
      }
    }
  }
}

fn translate_key(key: &str, position: impl Fn(usize) -> Option<usize>) -> Result<String, String> {
  let translate = |value| position(value).ok_or_else(|| "Item is outside the retained history".to_string());
  if key.starts_with("event.v1.") {
    Ok(encode_event_key(translate(decode_event_key(key)?)?))
  } else if key.starts_with("trajectory.v1.") {
    Ok(encode_trajectory_key(translate(decode_trajectory_key(key)?)?))
  } else if key.starts_with("trajectory-events.v1.") {
    let (anchor, offset) = decode_trajectory_event_cursor(key)?;
    Ok(encode_trajectory_event_cursor(translate(anchor)?, offset))
  } else {
    Err("Invalid history item identity".into())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::relay::{RelayMode, RelaySettings};
  use std::{io::Write, time::Duration};
  use tokn_session_relay::{ProviderRoot, RelayConfig};

  async fn window(service: &ViewerService, key: &str, cursor: Option<String>) -> EventPage {
    let service = service.clone();
    let session_key = key.to_owned();
    tokio::task::spawn_blocking(move || {
      // Exercise the JSON boundary used by desktop and HTTP clients, including
      // direction: omitting it defaults to the legacy forward row API.
      let request = serde_json::from_value(json!({
        "session_key": session_key,
        "window_mode": if cursor.is_some() {
          "earlier"
        } else {
          "retained"
        },
        "cursor": cursor,
        "direction": "backward",
      }))
      .unwrap();
      service.load_event_page(request)
    })
    .await
    .unwrap()
    .unwrap()
  }

  #[tokio::test]
  async fn all_delivery_keeps_source_fragments_and_eager_details_without_resending_unchanged_records() {
    use crate::updates::{SessionUpdatesRequest, UpdateLevel};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("all.jsonl");
    std::fs::write(&path, include_str!("../../../pi/fixtures/basic_session.jsonl")).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("tcp://{}", listener.local_addr().unwrap());
    let mut config = RelayConfig::new(vec![ProviderRoot::new(Provider::Pi, root.path().into())]);
    config.include_native = true;
    config.poll_interval = Duration::from_millis(10);
    let server = tokio::spawn(crate::service_server::serve_listener(listener, config));
    let service = ViewerService::new(Arc::new(NativeRepository::default()));
    let mut changes = service.relay.changes.subscribe();
    service
      .relay
      .configure(RelaySettings {
        mode: RelayMode::External,
        endpoint,
        ..Default::default()
      })
      .unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
      while !service.relay.has_catalog() {
        changes.recv().await.unwrap();
      }
    })
    .await
    .unwrap();
    let key = encode_session_key(&SessionLocator {
      version: 1,
      provider: ViewerProvider::Pi,
      session_id: "pi-session".into(),
      source_path: path.clone(),
    })
    .unwrap();
    let request = SessionUpdatesRequest {
      subscription_id: "all-test".into(),
      session_key: key,
      level: UpdateLevel::All,
      cursor: None,
      detail_keys: vec![],
      scope: None,
      history_cursor: None,
      unsubscribe: false,
    };
    let load = |request| {
      let service = service.clone();
      async move {
        tokio::task::spawn_blocking(move || service.load_session_updates(request))
          .await
          .unwrap()
          .unwrap()
      }
    };
    let live_request = SessionUpdatesRequest {
      subscription_id: "live-only-test".into(),
      level: UpdateLevel::Steps,
      ..request.clone()
    };
    let live_service = service.clone();
    let subscribe_request = live_request.clone();
    let ack = tokio::task::spawn_blocking(move || {
      live_service.subscribe_session_owned(subscribe_request, Some("old-socket".into()))
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(ack["revision"], "0");
    assert!(ack.get("items").is_none(), "subscribing must not send history");
    let reclaim_service = service.clone();
    let reclaim_request = live_request.clone();
    let reclaimed = tokio::task::spawn_blocking(move || {
      reclaim_service.subscribe_session_owned(reclaim_request, Some("new-socket".into()))
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(reclaimed["generation"], ack["generation"]);
    service.release_session_subscriptions("old-socket");
    let backward_service = service.clone();
    let backward = tokio::task::spawn_blocking(move || backward_service.load_session_backward(live_request.clone()))
      .await
      .unwrap()
      .unwrap();
    assert!(backward.snapshot);
    assert_eq!(backward.generation, ack["generation"]);
    assert_eq!(backward.state["scope"]["history"], "latest_turn");
    assert!(
      backward
        .items
        .iter()
        .filter(|item| item["summary"]["type"] == "activity_group")
        .all(|group| group["summary"]["child_keys"]
          .as_array()
          .unwrap()
          .iter()
          .all(|child| !backward.items.iter().any(|item| item["item_id"] == *child))),
      "default backward delivery keeps inner groups collapsed"
    );
    let descriptor = backward
      .items
      .iter()
      .find(|item| item["summary"]["type"] == "activity_group")
      .unwrap();
    let group_key = descriptor["item_id"].as_str().unwrap().to_owned();
    let mut scope: crate::updates::UpdateScope = serde_json::from_value(backward.state["scope"].clone()).unwrap();
    scope.group_keys.push(group_key);
    let details_request = SessionUpdatesRequest {
      subscription_id: "live-only-test".into(),
      level: UpdateLevel::Steps,
      cursor: Some(backward.revision.clone()),
      scope: Some(scope),
      ..request.clone()
    };
    let group_service = service.clone();
    let group = tokio::task::spawn_blocking(move || {
      group_service.load_session_details(crate::delivery::SessionDetailsRequest::Group {
        request: details_request,
      })
    })
    .await
    .unwrap()
    .unwrap();
    let crate::delivery::SessionDetails::Group(group) = group else {
      panic!("Expected complete group rows");
    };
    let membership: std::collections::HashSet<_> = backward
      .items
      .iter()
      .chain(&group.items)
      .filter_map(|item| item["item_id"].as_str())
      .collect();
    assert!(
      descriptor["summary"]["child_keys"]
        .as_array()
        .unwrap()
        .iter()
        .all(|key| membership.contains(key.as_str().unwrap()))
    );
    let first = load(request.clone()).await;
    let records: Vec<_> = first.items.iter().filter(|item| item["kind"] == "event").collect();
    let source_types: Vec<_> = records
      .iter()
      .map(|item| item["event"]["event"]["type"].as_str().unwrap())
      .collect();
    assert!(source_types.contains(&"tool_call"));
    assert!(
      records
        .iter()
        .any(|item| item["event"]["event"]["record_kind"] == "invocation")
    );
    assert!(
      records
        .iter()
        .any(|item| item["event"]["event"]["record_kind"] == "result")
    );
    assert!(source_types.contains(&"session_started"));
    assert!(source_types.contains(&"provider_changed"));
    assert!(source_types.contains(&"unknown"));
    assert_eq!(first.event_order.as_ref().unwrap().len(), records.len());
    assert!(
      first.items.iter().any(
        |item| item["kind"] == "detail" && item["detail"]["tool_output"]["sections"][0]["text"] == "project readme"
      )
    );
    assert!(
      first
        .items
        .iter()
        .any(|item| item["kind"] == "detail" && item["detail"]["event"]["type"] == "trajectory")
    );
    let tool_key = first
      .items
      .iter()
      .find(|item| item["summary"]["type"] == "tool_call")
      .unwrap()["item_id"]
      .as_str()
      .unwrap()
      .to_owned();
    let tool_service = service.clone();
    let tool_request = LoadEventDetailRequest {
      session_key: request.session_key.clone(),
      event_key: tool_key,
    };
    let inspect_request = tool_request.clone();
    let tool = tokio::task::spawn_blocking(move || {
      tool_service.load_session_details(crate::delivery::SessionDetailsRequest::Tool { request: tool_request })
    })
    .await
    .unwrap()
    .unwrap();
    let crate::delivery::SessionDetails::Tool(tool) = tool else {
      panic!("Expected tool display payload");
    };
    assert!(tool.native.is_none());
    let inspect_service = service.clone();
    let inspect = tokio::task::spawn_blocking(move || inspect_service.inspect_session_event(inspect_request))
      .await
      .unwrap()
      .unwrap();
    assert!(
      inspect.native.is_some(),
      "inspection alone exposes opted-in native records"
    );
    let groups: Vec<_> = first
      .groups
      .iter()
      .filter(|group| group["summary"]["type"] == "trajectory")
      .collect();
    assert!(!groups.is_empty());
    for group in groups {
      let keys = group["summary"]["child_keys"].as_array().unwrap();
      assert!(!keys.is_empty());
      for key in keys {
        assert!(key.as_str().unwrap().starts_with("window.v1."));
        assert!(
          first
            .items
            .iter()
            .any(|item| item["item_id"] == *key && item["summary"].is_object())
        );
      }
    }
    let unchanged = load(SessionUpdatesRequest {
      cursor: Some(first.revision.clone()),
      ..request.clone()
    })
    .await;
    assert!(unchanged.items.is_empty());
    assert!(unchanged.event_order.is_none());
    assert_eq!(unchanged.revision, first.revision);
    let steps = load(SessionUpdatesRequest {
      subscription_id: "steps-test".into(),
      level: UpdateLevel::Steps,
      ..request.clone()
    })
    .await;
    assert!(
      steps
        .items
        .iter()
        .all(|item| item["kind"] != "event" && item["kind"] != "detail")
    );
    let appended = json!({"type":"message", "id":"appended", "parentId":"future-1", "message":{"role":"user", "content":"new prompt"}});
    std::fs::OpenOptions::new()
      .append(true)
      .open(&path)
      .unwrap()
      .write_all(format!("{appended}\n").as_bytes())
      .unwrap();
    let next = tokio::time::timeout(Duration::from_secs(4), async {
      loop {
        let update = load(SessionUpdatesRequest {
          cursor: Some(first.revision.clone()),
          ..request.clone()
        })
        .await;
        if update
          .items
          .iter()
          .any(|item| item["kind"] == "event" && item["event"]["event"]["text"] == "new prompt")
        {
          break update;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
      }
    })
    .await
    .unwrap();
    assert!(!next.snapshot);
    assert_eq!(next.items.iter().filter(|item| item["kind"] == "event").count(), 1);
    assert_eq!(next.event_order.unwrap().len(), records.len() + 1);
    service
      .relay
      .configure(RelaySettings {
        mode: RelayMode::Local,
        ..Default::default()
      })
      .unwrap();
    server.abort();
  }

  #[tokio::test]
  async fn shared_pages_redact_follow_diagnostics_in_retained_and_legacy_modes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("shared-session.jsonl");
    let header = json!({"type":"session", "id":"shared", "timestamp":"2026-01-01", "cwd":"/tmp"});
    let message = json!({"type":"message", "id":"one", "message":{"role":"user", "content":"hello"}});
    std::fs::write(&path, format!("{header}\n{message}\n")).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("tcp://{}", listener.local_addr().unwrap());
    let mut config = RelayConfig::new(vec![ProviderRoot::new(Provider::Pi, root.path().into())]);
    config.poll_interval = Duration::from_millis(20);
    let server = tokio::spawn(crate::service_server::serve_listener(listener, config));
    let service = ViewerService::new(Arc::new(NativeRepository::default()));
    let mut changes = service.relay.changes.subscribe();
    service
      .relay
      .configure(RelaySettings {
        mode: RelayMode::External,
        endpoint,
        ..Default::default()
      })
      .unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
      while !service.relay.has_catalog() {
        changes.recv().await.unwrap();
      }
    })
    .await
    .unwrap();
    let locator = SessionLocator {
      version: 1,
      provider: ViewerProvider::Pi,
      session_id: "shared".into(),
      source_path: path.clone(),
    };
    let key = encode_session_key(&locator).unwrap();
    let shared = service.scoped_to_sessions(&[key.clone()]).unwrap();
    let initial = window(&shared, &key, None).await;
    assert!(initial.follow_error.is_none());

    std::fs::OpenOptions::new()
      .append(true)
      .open(&path)
      .unwrap()
      .write_all(&[0xff, b'\n'])
      .unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
      loop {
        let change = changes.recv().await.unwrap();
        if change.session_key.as_deref() == Some(key.as_str()) && service.relay.follow_error(&locator).is_some() {
          break;
        }
      }
    })
    .await
    .unwrap();
    let owner = window(&service, &key, None).await;
    assert!(owner.follow_error.unwrap().contains(path.to_str().unwrap()));

    let retained = window(&shared, &key, None).await;
    assert_eq!(
      retained.follow_error.as_deref(),
      Some("Live updates are temporarily unavailable; retrying.")
    );
    assert_eq!(
      retained.events.len(),
      initial.events.len(),
      "last-good history remains visible"
    );
    let shared_legacy = shared.clone();
    let legacy = tokio::task::spawn_blocking(move || {
      shared_legacy.load_event_page(EventPageRequest {
        session_key: key,
        window_mode: None,
        cursor: None,
        offset: None,
        direction: PageDirection::Backward,
        limit: None,
      })
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(legacy.follow_error, retained.follow_error);
    assert_eq!(legacy.events.len(), initial.events.len());
    server.abort();
  }

  #[tokio::test]
  async fn retained_history_survives_prepend_append_and_reopen_with_correct_details() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("window.jsonl");
    let header = json!({"type":"session", "id":"window", "timestamp":"2026-01-01", "cwd":"/tmp"}).to_string() + "\n";
    let message = |id: usize| {
      json!({"type":"message", "id":format!("user-{id}"),
      "message":{"role":"user", "content":format!("prompt {id}")}})
      .to_string()
        + "\n"
    };
    std::fs::write(&path, format!("{header}{}", (0..8).map(message).collect::<String>())).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("tcp://{}", listener.local_addr().unwrap());
    let mut config = RelayConfig::new(vec![ProviderRoot::new(Provider::Pi, root.path().into())]);
    config.include_native = true;
    config.poll_interval = Duration::from_millis(10);
    let server = tokio::spawn(crate::service_server::serve_listener(listener, config));
    let service = ViewerService::new(Arc::new(NativeRepository::default()));
    let mut changes = service.relay.changes.subscribe();
    service
      .relay
      .configure(RelaySettings {
        mode: RelayMode::External,
        endpoint,
        include_native: true,
      })
      .unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
      while !service.relay.has_catalog() {
        changes.recv().await.unwrap();
      }
    })
    .await
    .unwrap();
    let key = encode_session_key(&SessionLocator {
      version: 1,
      provider: ViewerProvider::Pi,
      session_id: "window".into(),
      source_path: path.clone(),
    })
    .unwrap();
    service
      .update_session_view(SessionViewRequest {
        view_id: "window-test".into(),
        revision: 1,
        session_key: Some(key.clone()),
        candidate_session_keys: vec![],
      })
      .unwrap();
    let initial = window(&service, &key, None).await;
    assert_eq!(
      initial
        .events
        .iter()
        .map(|event| event.summary.as_str())
        .collect::<Vec<_>>(),
      ["prompt 5", "prompt 6", "prompt 7"]
    );
    let stable = initial.events.last().unwrap().event_key.clone();
    let scoped_request = crate::updates::SessionUpdatesRequest {
      subscription_id: "scoped-history-test".into(),
      session_key: key.clone(),
      level: crate::updates::UpdateLevel::Steps,
      cursor: None,
      detail_keys: vec![],
      scope: Some(crate::updates::UpdateScope::default()),
      history_cursor: None,
      unsubscribe: false,
    };
    let first_service = service.clone();
    let first_request = scoped_request.clone();
    let first = tokio::task::spawn_blocking(move || first_service.load_session_updates(first_request))
      .await
      .unwrap()
      .unwrap();
    assert_eq!(first.items.len(), 1, "opening projects only the newest turn");
    let history_service = service.clone();
    let history_cursor = initial.previous_cursor.clone();
    let history = tokio::task::spawn_blocking(move || {
      history_service.load_session_updates(crate::updates::SessionUpdatesRequest {
        cursor: Some(first.revision),
        history_cursor,
        scope: Some(crate::updates::UpdateScope {
          history: crate::updates::HistoryScope::Retained,
          ..Default::default()
        }),
        ..scoped_request
      })
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(history.item_order.as_ref().unwrap().len(), 6);
    let earlier = window(&service, &key, initial.previous_cursor).await;
    assert_eq!(earlier.events.len(), 6);
    assert_eq!(earlier.events.last().unwrap().event_key, stable);
    let detail = service
      .load_event_detail(LoadEventDetailRequest {
        session_key: key.clone(),
        event_key: stable.clone(),
      })
      .unwrap();
    assert_eq!(detail.event["text"], "prompt 7");
    assert_eq!(detail.event_key, stable);
    assert_eq!(detail.native.unwrap()["id"], "user-7");
    service
      .update_session_view(SessionViewRequest {
        view_id: "window-test".into(),
        revision: 2,
        session_key: None,
        candidate_session_keys: vec![],
      })
      .unwrap();
    let reopened = window(&service, &key, None).await;
    assert_eq!(
      reopened.events.len(),
      6,
      "switching away does not evict retained history"
    );
    std::fs::OpenOptions::new()
      .append(true)
      .open(&path)
      .unwrap()
      .write_all(message(8).as_bytes())
      .unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
      loop {
        let page = window(&service, &key, None).await;
        if page.events.len() == 7 {
          assert_eq!(page.events[0].summary, "prompt 2");
          assert_eq!(page.events[5].event_key, stable);
          break;
        }
        changes.recv().await.unwrap();
      }
    })
    .await
    .unwrap();
    let legacy_service = service.clone();
    let legacy_key = key.clone();
    let legacy = tokio::task::spawn_blocking(move || {
      legacy_service.load_event_page(EventPageRequest {
        session_key: legacy_key,
        window_mode: None,
        cursor: None,
        offset: None,
        direction: PageDirection::Backward,
        limit: Some(2),
      })
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(legacy.total_events, 10, "legacy row paging still sees the full history");
    assert_eq!(legacy.events.len(), 2);
    assert!(legacy.previous_cursor.is_some());
    std::fs::write(&path, format!("{header}{}", message(99))).unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
      loop {
        let page = window(&service, &key, None).await;
        if page.events.iter().any(|event| event.summary == "prompt 99") {
          break;
        }
        changes.recv().await.unwrap();
      }
    })
    .await
    .unwrap();
    assert!(
      service
        .load_event_detail(LoadEventDetailRequest {
          session_key: key,
          event_key: stable
        })
        .is_err(),
      "source replacement cannot reuse stale detail identities"
    );
    service
      .relay
      .configure(RelaySettings {
        mode: RelayMode::Local,
        ..RelaySettings::default()
      })
      .unwrap();
    server.abort();
  }

  #[test]
  fn prepending_history_preserves_event_and_trajectory_identity() {
    let first = WindowIdentity {
      event_offset: 100,
      generation: Some("same".into()),
    };
    let earlier = WindowIdentity {
      event_offset: 40,
      generation: Some("same".into()),
    };
    for (one, two) in [
      ("event.v1.5", "event.v1.41"),
      ("trajectory.v1.5", "trajectory.v1.41"),
      ("trajectory-events.v1.5.a", "trajectory-events.v1.41.a"),
    ] {
      let key = first.key(one);
      assert_eq!(key, earlier.key(two));
      assert_eq!(earlier.local_key(&key).unwrap(), two);
    }
  }

  #[test]
  fn stale_generation_cannot_open_reused_source_position_or_page_history() {
    let old = WindowIdentity {
      event_offset: 0,
      generation: Some("old".into()),
    };
    let fresh = WindowIdentity {
      event_offset: 0,
      generation: Some("fresh".into()),
    };
    assert!(fresh.local_key(&old.key("event.v1.2")).is_err());
    assert!(decode_history_cursor(&encode_history_cursor(Some("old"), 12), Some("fresh")).is_err());
    assert_eq!(
      decode_history_cursor(&encode_history_cursor(Some("fresh"), 12), Some("fresh")).unwrap(),
      12
    );
  }

  #[test]
  fn inspector_translates_all_operation_positions_but_never_provider_native_keys() {
    let identity = WindowIdentity {
      event_offset: 100,
      generation: Some("same".into()),
    };
    let native = json!({"source_event_count":1, "source_records":[{"event_key":"provider-key"}]});
    let detail = EventDetail {
      event_key: "event.v1.2".into(),
      event: json!({"type":"unknown"}),
      content_revision: None,
      native: Some(native.clone()),
      is_hidden: false,
      tool_output: None,
    };
    assert_eq!(
      identity.detail(detail, false).native,
      Some(native),
      "native payload is not a viewer envelope"
    );
    let detail = identity.detail(
      EventDetail {
        event_key: "event.v1.2".into(),
        event: json!({"id":{"kind":"uncorrelated", "source_event_index":2},
        "source_event_indices":[2,3], "source_event_keys":[{"source_event_index":2},{"source_event_index":3}]}),
        content_revision: None,
        native: Some(json!({"source_records":[{"event_key":"event.v1.2", "native":{"event_key":"provider-key"}}]})),
        is_hidden: false,
        tool_output: None,
      },
      true,
    );
    assert_eq!(detail.event["id"]["source_event_index"], 102);
    assert_eq!(detail.event["source_event_indices"], json!([102, 103]));
    assert_eq!(detail.event["source_event_keys"][1]["source_event_index"], 103);
    let native = detail.native.unwrap();
    assert_eq!(native["source_records"][0]["event_key"], identity.key("event.v1.2"));
    assert_eq!(native["source_records"][0]["native"]["event_key"], "provider-key");
  }
}
