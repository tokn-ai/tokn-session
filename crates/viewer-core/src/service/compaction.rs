use super::*;
use crate::model::{CompactionCardSummary, CompactionTokenSummary};
use tokn_session_core::{CompactionOperation, compaction_operations};

pub(super) fn for_source(events: &[AgentEvent], index: usize) -> Option<CompactionOperation> {
  if !matches!(events.get(index), Some(AgentEvent::Compaction(_))) {
    return None;
  }
  compaction_operations(events)
    .into_iter()
    .find(|operation| operation.source_event_indices.first() == Some(&index))
}

pub(super) fn card(event: &AgentEvent) -> Option<CompactionCardSummary> {
  let AgentEvent::Compaction(event) = event else {
    return None;
  };
  Some(CompactionCardSummary {
    state: serialized_label(event.state).unwrap(),
    trigger: event.trigger.as_ref().map(|s| truncate(s.clone(), 120)),
    reason: event.reason.as_ref().map(|s| truncate(s.clone(), 500)),
    has_summary: event.summary.as_ref().is_some_and(|s| !s.is_empty()),
    summary_opaque: event.summary_opaque,
    measurements: event
      .measurements
      .iter()
      .map(|item| CompactionTokenSummary {
        scope: serialized_label(item.scope).unwrap(),
        tokens: item.tokens.to_string(),
        estimated: item.estimated,
      })
      .collect(),
  })
}

#[cfg(test)]
mod tests {
  use super::super::tests::{key_for, loaded_session, service_with_session};
  use super::*;
  use crate::relay::{RelayMode, RelaySettings};
  use std::{io::Write, time::Duration};
  use tokn_session_core::{CompactionEvent, CompactionState, CompactionTokenScope, LifecycleEvent, LifecycleScope};
  use tokn_session_relay::{ProviderRoot, RelayConfig};

  #[test]
  fn one_stable_card_aggregates_details_without_finishing_the_active_turn() {
    let mut compact = CompactionEvent::new(Provider::Codex, Some("fixture".into()), CompactionState::Started);
    compact.compaction_id = Some("op".into());
    let mut events = vec![
      AgentEvent::Lifecycle(LifecycleEvent {
        provider: Provider::Codex,
        session_id: Some("fixture".into()),
        turn_id: "turn".into(),
        step_id: None,
        scope: LifecycleScope::Turn,
        phase: Phase::Started,
        outcome: None,
        timestamp: None,
        native: json!({}),
      }),
      AgentEvent::Compaction(compact.clone()),
    ];
    let page = |events: Vec<AgentEvent>| {
      service_with_session(loaded_session(events))
        .load_event_page(EventPageRequest {
          window_mode: None,
          session_key: key_for("fixture"),
          cursor: None,
          offset: None,
          direction: PageDirection::Forward,
          limit: None,
        })
        .unwrap()
    };
    let initial = page(events.clone());
    assert_eq!(initial.events.len(), 2);
    assert_eq!(initial.events[1].title, "Compacting…");
    let key = initial.events[1].event_key.clone();
    compact.state = CompactionState::Completed;
    compact.summary = Some("## Summary\nretained decisions".into());
    compact.tokens(CompactionTokenScope::ContextBefore, u64::MAX, None);
    events.push(AgentEvent::Compaction(compact));
    let finished = page(events.clone());
    assert_eq!(finished.events.len(), 2);
    assert_eq!(finished.events[1].event_key, key);
    assert_eq!(finished.events[1].title, "Context compacted");
    assert_eq!(finished.events[0].trajectory.as_ref().unwrap().status, "working");
    assert_eq!(
      finished.events[1].compaction.as_ref().unwrap().measurements[0].tokens,
      u64::MAX.to_string()
    );
    let service = service_with_session(loaded_session(events));
    let detail = service
      .load_event_detail(LoadEventDetailRequest {
        session_key: key_for("fixture"),
        event_key: key,
      })
      .unwrap();
    assert_eq!(detail.event["summary"], "## Summary\nretained decisions");
    assert_eq!(detail.event["state"], "completed");
    assert!(detail.native.is_none());
    assert!(
      service
        .load_event_detail(LoadEventDetailRequest {
          session_key: key_for("fixture"),
          event_key: encode_event_key(2)
        })
        .is_err()
    );
  }

  async fn retained_projection(
    service: &ViewerService,
    session_key: &str,
  ) -> (EventPage, Vec<EventSummary>, super::super::windows::UpdatePayloads) {
    let service = service.clone();
    let session_key = session_key.to_owned();
    tokio::task::spawn_blocking(move || {
      service.load_update_pages(
        EventPageRequest {
          window_mode: Some(crate::model::HistoryWindowMode::Retained),
          session_key,
          cursor: None,
          offset: None,
          direction: PageDirection::Backward,
          limit: None,
        },
        true,
      )
    })
    .await
    .unwrap()
    .unwrap()
  }

  #[tokio::test]
  async fn codex_snapshot_compaction_keeps_one_card_and_both_sources_across_follow() {
    let fixture = include_str!("../../../codex/fixtures/compaction_snapshot.jsonl");
    let records: Vec<_> = fixture.lines().collect();
    let (completion, checkpoint) = records.split_last().unwrap();
    for include_native in [false, true] {
      let root = tempfile::tempdir().unwrap();
      let source_path = root.path().join("compaction.jsonl");
      std::fs::write(&source_path, checkpoint.join("\n") + "\n").unwrap();
      let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
      let endpoint = format!("tcp://{}", listener.local_addr().unwrap());
      let mut config = RelayConfig::new(vec![ProviderRoot::new(Provider::Codex, root.path().into())]);
      config.include_native = include_native;
      config.poll_interval = Duration::from_millis(20);
      let server = tokio::spawn(crate::service_server::serve_listener(listener, config));
      let service = ViewerService::new(Arc::new(NativeRepository::default()));
      let mut changes = service.relay.changes.subscribe();
      service
        .relay
        .configure(RelaySettings {
          mode: RelayMode::External,
          endpoint,
          include_native,
        })
        .unwrap();
      tokio::time::timeout(Duration::from_secs(4), async {
        while !service.relay.has_catalog() {
          changes.recv().await.unwrap();
        }
      })
      .await
      .unwrap();
      let session_key = encode_session_key(&SessionLocator {
        version: 1,
        provider: ViewerProvider::Codex,
        session_id: "fixture".into(),
        source_path: source_path.clone(),
      })
      .unwrap();
      let (initial, _, initial_payloads) = retained_projection(&service, &session_key).await;
      let initial_card = initial
        .events
        .iter()
        .find(|event| event.event_type == "compaction")
        .unwrap();
      let stable_key = initial_card.event_key.clone();
      assert!(initial_payloads.is_running);
      assert!(initial_card.compaction.as_ref().unwrap().summary_opaque);
      assert_eq!(
        initial_payloads
          .events
          .iter()
          .filter(|event| event.event["type"] == "compaction")
          .count(),
        1
      );

      std::fs::OpenOptions::new()
        .append(true)
        .open(&source_path)
        .unwrap()
        .write_all(format!("{completion}\n").as_bytes())
        .unwrap();
      let (page, semantic, payloads) = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
          let projection = retained_projection(&service, &session_key).await;
          if projection
            .2
            .events
            .iter()
            .filter(|event| event.event["type"] == "compaction")
            .count()
            == 2
          {
            break projection;
          }
          changes.recv().await.unwrap();
        }
      })
      .await
      .unwrap();
      assert!(
        payloads.is_running,
        "compaction completion does not finish the active turn"
      );
      let cards: Vec<_> = page
        .events
        .iter()
        .filter(|event| event.event_type == "compaction")
        .collect();
      assert_eq!(cards.len(), 1);
      assert_eq!(cards[0].event_key, stable_key);
      assert_eq!(cards[0].title, "Context compacted");
      assert!(cards[0].compaction.as_ref().unwrap().summary_opaque);
      assert!(!cards[0].compaction.as_ref().unwrap().has_summary);
      let semantic_cards: Vec<_> = semantic
        .iter()
        .filter(|event| event.event_type == "compaction")
        .collect();
      assert_eq!(semantic_cards.len(), 1);
      assert_eq!(semantic_cards[0].event_key, stable_key);
      let detail = payloads
        .details
        .iter()
        .find(|detail| detail.event_key == stable_key)
        .unwrap();
      assert_eq!(detail.event["state"], "completed");
      assert_eq!(detail.event["summary_opaque"], true);
      assert_eq!(detail.event["turn_id"], "turn-1");
      if include_native {
        let native = detail.native.as_ref().unwrap();
        assert_eq!(native["source_event_count"], 2);
        let records = native["source_records"].as_array().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["native"]["type"], "compacted");
        assert_eq!(records[1]["native"]["payload"]["type"], "item_completed");
        assert_eq!(records[0]["event_key"], stable_key);
      } else {
        assert!(detail.native.is_none());
      }
      assert_eq!(
        payloads
          .events
          .iter()
          .filter(|event| event.event["type"] == "compaction")
          .count(),
        2,
        "raw normalized observations remain individually inspectable"
      );
      for event in payloads
        .events
        .iter()
        .filter(|event| event.event["type"] == "compaction")
      {
        assert_eq!(event.native.is_some(), include_native);
      }
      let legacy_service = service.clone();
      let legacy = tokio::task::spawn_blocking(move || {
        legacy_service.load_event_page(EventPageRequest {
          window_mode: None,
          session_key,
          cursor: None,
          offset: None,
          direction: PageDirection::Forward,
          limit: None,
        })
      })
      .await
      .unwrap()
      .unwrap();
      assert_eq!(
        legacy
          .events
          .iter()
          .filter(|event| event.event_type == "compaction")
          .count(),
        1
      );
      service.relay.shutdown().await;
      server.abort();
    }
  }
}
