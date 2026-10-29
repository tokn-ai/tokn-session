use super::*;
use serde_json::json;

fn record(id: usize, events: serde_json::Value) -> RelayRecord {
  serde_json::from_value(json!({
    "path": "/tmp/history.jsonl", "topic": "codex.test", "operation": "upsert", "record_id": format!("row:{id}"),
    "session": {"provider": "codex", "session_id": "test"}, "events": events,
    "native": {"row": id}
  }))
  .unwrap()
}

fn turn(id: usize) -> RelayRecord {
  record(
    id,
    json!([
      {"type":"lifecycle", "provider":"codex", "turn_id":format!("turn-{id}"), "scope":"turn", "phase":"started", "native":{}},
      {"type":"message", "provider":"codex", "message_id":format!("user-{id}"), "role":"user", "delivery":"unspecified", "phase":"finished", "text":format!("prompt {id}")},
      {"type":"message", "provider":"codex", "message_id":format!("assistant-{id}"), "role":"assistant", "delivery":"final", "phase":"finished", "text":"x".repeat(64 * 1024)},
      {"type":"usage", "provider":"codex", "kind":"model_call", "input_tokens":10, "output_tokens":20, "native":{}}
    ]),
  )
}

#[test]
fn journal_retains_offsets_instead_of_history_payloads_and_keeps_committed_prefixes() {
  let mut history = History::new().unwrap();
  history.append(&(0..10).map(turn).collect::<Vec<_>>()).unwrap();
  let first = history.clone();
  assert_eq!(history.window_start(None, None), 7 * 4);
  assert_eq!(history.window_start(None, Some(7 * 4)), 4 * 4);
  assert_eq!(history.window_start(None, Some(4 * 4)), 4);
  assert_eq!(history.window_start(None, Some(4)), 0);
  assert!(history.bytes > 640 * 1024);
  let journal = history.journal.lock().unwrap();
  let retained = journal.index.capacity() * std::mem::size_of::<RecordIndex>()
    + journal.turns.capacity() * std::mem::size_of::<TurnIndex>();
  assert!(retained < 4096, "journal retains only indexes, not old message bodies");
  drop(journal);
  history.append(&[turn(10)]).unwrap();
  assert_eq!(first.len(), 10);
  assert_eq!(first.events, 40);
  assert!(first.read(10).is_err(), "old snapshot must not see uncommitted append");
  assert_eq!(first.window_start(None, None), 28);
  assert_eq!(
    history.window_start(Some(16), None),
    16,
    "loaded earlier history stays loaded"
  );
  let (record, offset) = history.read(history.record_at_event(28)).unwrap();
  assert_eq!(offset, 28);
  assert_eq!(record.record.native.unwrap()["row"], 7);
  assert!(matches!(record.record.events.last(), Some(AgentEvent::Usage(_))));
  let replacement = History::new().unwrap();
  assert!(!replacement.same_journal(&first));
  assert_eq!(first.read(0).unwrap().0.record.record_id, "row:0");
}

#[test]
fn appended_prompt_cannot_change_an_older_snapshots_turn_boundaries() {
  let mut history = History::new().unwrap();
  history.append(&(0..4).map(turn).collect::<Vec<_>>()).unwrap();
  history
    .append(&[record(
      4,
      json!([
        {"type":"lifecycle", "provider":"codex", "turn_id":"pending", "scope":"turn", "phase":"started", "native":{}}
      ]),
    )])
    .unwrap();
  let older = history.clone();
  history.append(&[record(5, json!([
    {"type":"message", "provider":"codex", "message_id":"pending-user", "role":"user", "delivery":"unspecified", "phase":"finished", "text":"next"}
  ]))]).unwrap();
  assert_eq!(older.window_start(None, None), 4);
  assert_eq!(history.window_start(None, None), 8);
}

#[test]
fn outstanding_questions_retain_navigation_context_without_mutating_old_snapshots() {
  let mut normalizer = tokn_session_codex::normalize::CodexNormalizer::new();
  let events: Vec<_> = include_str!("../../tests/fixtures/codex/async_question_replies.jsonl")
    .lines()
    .take(5)
    .flat_map(|line| normalizer.normalize(serde_json::from_str(line).unwrap()))
    .collect();
  let mut history = History::new().unwrap();
  history
    .append(&[record(0, serde_json::to_value(events).unwrap())])
    .unwrap();
  for index in 1..8 {
    history.append(&[record(index, json!([{"type":"message","provider":"codex","message_id":format!("user-{index}"),"role":"user","delivery":"unspecified","phase":"finished","text":"ordinary follow-up"}]))]).unwrap();
  }
  assert_eq!(history.window_start(None, None), 0);
  let before = history.clone();
  let answer = normalizer.normalize(
    serde_json::from_str(
      include_str!("../../tests/fixtures/codex/async_question_replies.jsonl")
        .lines()
        .nth(5)
        .unwrap(),
    )
    .unwrap(),
  );
  history
    .append(&[record(9, serde_json::to_value(answer).unwrap())])
    .unwrap();
  assert!(history.window_start(None, None) > 0);
  assert_eq!(before.window_start(None, None), 0);
}

#[test]
fn tool_results_and_compaction_updates_retain_their_earlier_context() {
  let tool = |kind: &str| {
    json!({"type":"tool_call", "provider":"codex", "session_id":"test",
    "tool_call_id":"call", "record_kind":kind, "tool_name":"exec", "tool_kind":"shell", "phase":"finished",
    "input": if kind == "invocation" { json!({"command":"pwd"}) } else { json!(null) },
    "output": if kind == "result" { json!({"text":"/tmp"}) } else { json!(null) }})
  };
  let mut history = History::new().unwrap();
  history
    .append(&[turn(0), record(1, json!([tool("invocation")]))])
    .unwrap();
  history.append(&(1..7).map(turn).collect::<Vec<_>>()).unwrap();
  let initial_start = history.window_start(None, None);
  assert!(initial_start > 4);
  let before_result = history.clone();
  history.append(&[record(8, json!([tool("result")]))]).unwrap();
  assert_eq!(
    history.context_start(initial_start),
    4,
    "late result must bring invocation into window"
  );
  assert_eq!(
    before_result.context_start(initial_start),
    initial_start,
    "future dependencies cannot mutate old snapshots"
  );
  let events: Vec<_> = history.iter().flat_map(|record| record.record.events).skip(4).collect();
  let tools = tokn_session_core::assemble_tool_operations(&events);
  assert_eq!(tools.len(), 1);
  assert!(tools[0].is_finished());
  assert_eq!(tools[0].input.as_ref().unwrap()["command"], "pwd");

  let mut compact = History::new().unwrap();
  compact
    .append(&[record(
      0,
      json!([
        {"type":"compaction", "provider":"codex", "session_id":"test", "compaction_id":"compact", "state":"started"}
      ]),
    )])
    .unwrap();
  compact.append(&(0..7).map(turn).collect::<Vec<_>>()).unwrap();
  compact.append(&[record(8, json!([
    {"type":"compaction", "provider":"codex", "session_id":"test", "compaction_id":"compact", "state":"completed", "summary":"kept decisions"}
  ]))]).unwrap();
  assert_eq!(
    compact.window_start(None, None),
    0,
    "completed compaction keeps its correlated start"
  );
}

#[test]
fn replacement_keeps_expanded_turn_span_and_fallback_uses_complete_records() {
  let mut history = History::new().unwrap();
  history.append(&(0..12).map(turn).collect::<Vec<_>>()).unwrap();
  assert_eq!(history.replacement_start(7, 28), 5 * 4);
  assert_eq!(
    history.window_start(Some(5 * 4 + 2), None),
    5 * 4,
    "reconnect snaps backwards to full turn"
  );
  let mut fallback = History::new().unwrap();
  for index in 0..160 {
    fallback
      .append(&[record(
        index,
        json!([
          {"type":"unknown", "provider":"codex", "native_type":"future"},
          {"type":"unknown", "provider":"codex", "native_type":"future"},
          {"type":"unknown", "provider":"codex", "native_type":"future"}
        ]),
      )])
      .unwrap();
  }
  assert_eq!(fallback.window_start(None, None), 180);
  assert_eq!(fallback.window_start(Some(181), None), 180);
}

#[test]
fn hidden_users_do_not_count_and_no_user_history_is_paged() {
  let mut history = History::new().unwrap();
  let rows: Vec<_> = (0..650).map(|id| record(id, json!([
    {"type":"message", "provider":"pi", "role":"user", "delivery":"unspecified", "phase":"finished", "text":"hidden", "provenance":{"source":null,"display":false}}
  ]))).collect();
  history.append(&rows).unwrap();
  assert_eq!(history.window_start(None, None), 350);
  assert_eq!(history.window_start(None, Some(350)), 50);
  assert_eq!(history.window_start(None, Some(50)), 0);
}

#[test]
fn journal_encoding_and_fingerprints_preserve_wire_bytes_and_ignore_context() {
  for provider in [
    Provider::Codex,
    Provider::Pi,
    Provider::OpenCode,
    Provider::ZCode,
    Provider::WorkBuddy,
    Provider::Dsh,
  ] {
    for native in [None, Some(json!({"quoted": "\"汉字\n", "nested": [null, true, 42]}))] {
      let mut record = turn(1);
      record.session.provider = provider;
      record.record.native = native;
      record.path = "/tmp/汉字\".jsonl".into();
      record.topic = "quoted\"topic".into();
      let (encoded, fingerprint) = encode_record(&record, budget::MAX_RECORD_BYTES).unwrap();
      assert_eq!(encoded, serde_json::to_vec(&record).unwrap());
      assert_eq!(
        fingerprint.is_none(),
        matches!(provider, Provider::Codex | Provider::Pi)
      );
      if let Some(fingerprint) = fingerprint {
        assert_eq!(fingerprint, History::fingerprint(&record.record).unwrap());
      }
      let mut history = History::new().unwrap();
      history.append(std::slice::from_ref(&record)).unwrap();
      record.session.title = Some("metadata changed".into());
      record.path = "/tmp/another.jsonl".into();
      assert!(history.matches(0, &record.record).unwrap());
      assert!(!history.matches(1, &record.record).unwrap());
      record.record.record_id.push_str("-edited");
      assert!(!history.matches(0, &record.record).unwrap());
    }
  }
}

#[test]
fn rejected_batch_preserves_committed_prefix_and_can_be_retried() {
  let initial_record = turn(0);
  let additions: Vec<_> = (1..5).map(turn).collect();
  let one = serde_json::to_vec(&initial_record).unwrap().len();
  let budget = Budget::new(budget::Limits {
    disk_bytes: one * 8,
    index_bytes: 1_000_000,
    records: 100,
  });
  let mut history = History::with_budget(budget.clone()).unwrap();
  history.append(&[initial_record]).unwrap();
  let initial = history.clone();
  let occupied = budget
    .reserve(Usage {
      disk_bytes: one * 6,
      ..Usage::default()
    })
    .unwrap();
  assert!(history.append(&additions).is_err());
  assert_eq!(
    history.journal.lock().unwrap().file.metadata().unwrap().len(),
    history.bytes as u64
  );
  drop(occupied);
  assert_eq!(
    (history.len(), history.events, history.bytes),
    (initial.len(), initial.events, initial.bytes)
  );
  assert_eq!(history.window_start(None, None), 0);
  assert!(history.read(1).is_err());
  history.append(&additions).unwrap();
  assert_eq!(history.len(), 5);
  assert_eq!(history.events, 20);
  assert_eq!(history.window_start(None, None), 8);
  assert_eq!(history.read(4).unwrap().0.record.record_id, "row:4");
  assert!(initial.read(1).is_err());
  assert_eq!(initial.read(0).unwrap().0.record.record_id, "row:0");
  assert_eq!(
    history.bytes,
    history
      .iter()
      .map(|record| serde_json::to_vec(&record).unwrap().len())
      .sum::<usize>()
  );
}

#[test]
fn cancelled_append_keeps_the_published_history_unchanged() {
  let mut history = History::new().unwrap();
  history.append(&[turn(0)]).unwrap();
  let initial = history.clone();
  let cancel = CancellationToken::new();
  cancel.cancel();
  assert!(
    history
      .append_cancellable(&[turn(1)], Some(&cancel))
      .unwrap_err()
      .contains("cancelled")
  );
  assert_eq!(
    (history.len(), history.events, history.bytes),
    (initial.len(), initial.events, initial.bytes)
  );
  assert!(history.read(1).is_err());
  history.append(&[turn(1)]).unwrap();
  assert_eq!(history.read(1).unwrap().0.record.record_id, "row:1");
}

fn test_budget(disk_bytes: usize) -> Budget {
  Budget::new(budget::Limits {
    disk_bytes,
    index_bytes: 1_000_000,
    records: 100,
  })
}

#[test]
fn journal_storage_is_independent_of_a_record_or_snapshot_buffer_budget() {
  let rows = (0..6).map(|id| record(id, json!([]))).collect::<Vec<_>>();
  let record_bytes = encode_record(&rows[0], budget::MAX_RECORD_BYTES).unwrap().0.len();
  let legacy_snapshot_budget = record_bytes * 2;
  let pool = test_budget(record_bytes * 8);
  let mut history = History::with_budget(pool.clone()).unwrap();
  history
    .append_encoded(&rows, record_bytes, None, |record| encode_record(record, record_bytes))
    .unwrap();
  assert!(history.bytes > legacy_snapshot_budget);
  assert_eq!(history.len(), rows.len());
  assert_eq!(pool.usage().disk_bytes, history.bytes);
  assert_eq!(pool.usage().records, rows.len());
  assert_eq!(history.read(5).unwrap().0.record.record_id, "row:5");
}

#[test]
fn shared_journal_disk_budget_rolls_back_copies_and_follows_last_snapshot_owner() {
  let rows = (0..2).map(|id| record(id, json!([]))).collect::<Vec<_>>();
  let record_bytes = encode_record(&rows[0], budget::MAX_RECORD_BYTES).unwrap().0.len();
  let pool = test_budget(record_bytes * 3);
  let mut original = History::with_budget(pool.clone()).unwrap();
  original.append(&rows).unwrap();
  let before = pool.usage();
  let snapshot = original.clone();
  assert_eq!(pool.usage().disk_bytes, before.disk_bytes);

  // Rebuilding/copying into a different physical journal consumes storage,
  // even when the serialized records are identical to another snapshot.
  let mut replacement = History::with_budget(pool.clone()).unwrap();
  let copied = original.iter().collect::<Vec<_>>();
  assert!(replacement.append(&copied).unwrap_err().contains("scratch disk"));
  assert_eq!(replacement.len(), 0);
  assert_eq!(replacement.journal.lock().unwrap().file.metadata().unwrap().len(), 0);
  assert_eq!(pool.usage().disk_bytes, before.disk_bytes);
  assert_eq!(pool.usage().index_bytes, before.index_bytes);
  assert_eq!(pool.usage().records, before.records);
  assert_eq!(snapshot.read(1).unwrap().0.record.record_id, "row:1");
  drop(original);
  assert_eq!(pool.usage().disk_bytes, before.disk_bytes);
  drop(snapshot);
  assert_eq!(pool.usage().disk_bytes, 0);
  assert_eq!(pool.usage().index_bytes, 0);
  assert_eq!(pool.usage().records, 0);
  replacement.append(&copied).unwrap();
  assert_eq!(replacement.len(), 2);
  drop(replacement);
  assert_eq!(pool.usage().disk_bytes, 0);
}

#[test]
fn index_limits_reject_admission_before_writing_or_reserving_payloads() {
  let row = record(
    0,
    json!([{
      "type":"question_request", "provider":"codex", "request_id":"r".repeat(1000),
      "phase":"finished", "questions":[{
        "id":"q".repeat(1000), "question":"Choose?", "allows_free_text":true, "is_secret":false
      }], "native":{}
    }]),
  );
  for limits in [
    budget::Limits {
      disk_bytes: 1_000_000,
      index_bytes: 1_000_000,
      records: 0,
    },
    budget::Limits {
      disk_bytes: 1_000_000,
      index_bytes: index_cost(std::slice::from_ref(&row)).unwrap() - 1,
      records: 100,
    },
  ] {
    let pool = Budget::new(limits);
    let mut history = History::with_budget(pool.clone()).unwrap();
    assert!(
      history
        .append(std::slice::from_ref(&row))
        .unwrap_err()
        .contains("index")
    );
    assert_eq!(history.len(), 0);
    assert_eq!(history.journal.lock().unwrap().file.metadata().unwrap().len(), 0);
    assert_eq!(pool.usage().disk_bytes, 0);
    assert_eq!(pool.usage().index_bytes, 0);
    assert_eq!(pool.usage().records, 0);
  }

  let mut identity_only = record(0, json!([]));
  let small_index = index_cost(std::slice::from_ref(&identity_only)).unwrap();
  identity_only.record.record_id = "r".repeat(4096);
  let pool = Budget::new(budget::Limits {
    disk_bytes: 1_000_000,
    index_bytes: small_index + 1024,
    records: 100,
  });
  let mut history = History::with_budget(pool.clone()).unwrap();
  assert!(history.append(&[identity_only]).unwrap_err().contains("index memory"));
  assert_eq!(pool.usage().disk_bytes, 0);
}

#[test]
fn record_encoding_and_late_cancellation_leave_no_charged_suffix() {
  let pool = test_budget(10_000);
  let mut history = History::with_budget(pool.clone()).unwrap();
  history.append(&[record(0, json!([]))]).unwrap();
  let prior = history.clone();
  let before = pool.usage();
  let rows = [record(1, json!([])), record(2, json!([]))];
  let cancel = CancellationToken::new();
  let result = history.append_encoded(&rows, budget::MAX_RECORD_BYTES, Some(&cancel), |record| {
    let encoded = encode_record(record, budget::MAX_RECORD_BYTES)?;
    cancel.cancel();
    Ok(encoded)
  });
  assert!(result.unwrap_err().contains("cancelled"));
  assert_eq!(history.len(), 1);
  assert_eq!(pool.usage().disk_bytes, before.disk_bytes);
  assert_eq!(pool.usage().index_bytes, before.index_bytes);
  assert_eq!(
    history.journal.lock().unwrap().file.metadata().unwrap().len(),
    prior.bytes as u64
  );

  let mut large = record(3, json!([]));
  large.record.native = Some(json!({"text":"x".repeat(2000)}));
  for provider in [Provider::Codex, Provider::OpenCode] {
    large.session.provider = provider;
    assert!(encode_record(&large, 512).unwrap_err().contains("encoding size"));
  }
  assert!(
    history
      .append_encoded(&rows, budget::MAX_RECORD_BYTES, None, |record| {
        if record.record.record_id == "row:2" {
          Err("fixture encoding failure".into())
        } else {
          encode_record(record, budget::MAX_RECORD_BYTES)
        }
      })
      .is_err()
  );
  assert_eq!(pool.usage().disk_bytes, before.disk_bytes);
  assert_eq!(
    history.journal.lock().unwrap().file.metadata().unwrap().len(),
    prior.bytes as u64
  );
  assert_eq!(prior.read(0).unwrap().0.record.record_id, "row:0");
  history.append(&rows).unwrap();
  assert_eq!(history.len(), 3);
}

#[test]
fn old_snapshot_cannot_overwrite_a_newer_committed_suffix() {
  let pool = test_budget(10_000);
  let mut history = History::with_budget(pool.clone()).unwrap();
  history.append(&[record(0, json!([]))]).unwrap();
  let mut old = history.clone();
  history.append(&[record(1, json!([]))]).unwrap();
  let before = pool.usage();
  assert!(
    old
      .append(&[record(2, json!([]))])
      .unwrap_err()
      .contains("older history")
  );
  assert_eq!(pool.usage().disk_bytes, before.disk_bytes);
  assert_eq!(history.read(1).unwrap().0.record.record_id, "row:1");
  assert_eq!(old.len(), 1);
}

#[test]
#[ignore = "manual journal CPU benchmark"]
fn benchmark_journal_initial_load_and_appends() {
  use std::{hint::black_box, time::Instant};
  let records: Vec<_> = (0..160).map(turn).collect();
  let append = |history: &mut History, records: &[RelayRecord], legacy: bool| {
    if legacy {
      // The previous source reader first encoded every record for charging,
      // then the journal encoded it again plus a normalized-only digest.
      let charged: usize = records
        .iter()
        .map(|record| serde_json::to_vec(record).unwrap().len())
        .sum();
      black_box(charged);
      history
        .append_encoded(records, budget::MAX_RECORD_BYTES, None, |record| {
          let bytes = serde_json::to_vec(record).map_err(|e| e.to_string())?;
          let fingerprint = History::fingerprint(&record.record)?;
          Ok((bytes, Some(fingerprint)))
        })
        .unwrap();
    } else {
      history.append(records).unwrap();
    }
  };
  for iteration in 0..3 {
    for legacy in [true, false] {
      let start = Instant::now();
      let mut history = History::new().unwrap();
      append(&mut history, &records, legacy);
      black_box(history.bytes);
      let initial = start.elapsed();
      let start = Instant::now();
      for record in &records[..10] {
        append(&mut history, std::slice::from_ref(record), legacy);
      }
      black_box(history.bytes);
      eprintln!(
        "journal 10 MiB run={iteration} legacy={legacy} initial={initial:?}; ten 64 KiB appends={:?}",
        start.elapsed()
      );
    }
  }
}
