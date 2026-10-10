//! Per-follow disk journal. Snapshots share immutable committed prefixes while
//! payloads stay on disk; the anonymous file disappears with the last snapshot.
use std::{
  collections::HashMap,
  fs::File,
  io::{Read, Seek, SeekFrom, Write},
  sync::{Arc, Mutex},
};

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use tokn_session_core::{AgentEvent, LifecycleScope, NormalizedRecord, Phase, Provider, Role, ToolRecordKind};
use tokn_session_relay::{RecordOperation, SessionContext};

use crate::RelayRecord;

pub(crate) const HISTORY_TURNS: usize = 3;
pub(crate) const INITIAL_HISTORY_TURNS: usize = 3;
const FALLBACK_EVENTS: usize = 300;

struct RecordIndex {
  offset: u64,
  length: usize,
  event_start: usize,
  events: usize,
  fingerprint: Option<[u8; 32]>,
}

type EncodedRecord = (Vec<u8>, Option<[u8; 32]>);

struct TurnIndex {
  start: usize,
  user_event: usize,
}

type ToolKey = (Provider, String, Option<String>, String);
type CompactionKey = (Provider, Option<String>, String);

struct ToolAnchor {
  first: usize,
  ambiguous: bool,
}

struct Journal {
  file: File,
  index: Vec<RecordIndex>,
  turns: Vec<TurnIndex>,
  pending_turn: Option<usize>,
  last_user_id: Option<String>,
  tools: HashMap<ToolKey, ToolAnchor>,
  compactions: HashMap<CompactionKey, usize>,
  // (later observation, original context); no event bodies are retained.
  dependencies: Vec<(usize, usize)>,
}

#[derive(Clone)]
pub(crate) struct History {
  journal: Arc<Mutex<Journal>>,
  length: usize,
  pub events: usize,
  pub bytes: usize,
  questions: crate::questions::Questions,
}

impl History {
  pub fn new() -> Result<Self, String> {
    Ok(Self {
      journal: Arc::new(Mutex::new(Journal {
        file: tempfile::tempfile().map_err(|e| format!("Cannot create session history journal: {e}"))?,
        index: Vec::new(),
        turns: Vec::new(),
        pending_turn: None,
        last_user_id: None,
        tools: HashMap::new(),
        compactions: HashMap::new(),
        dependencies: Vec::new(),
      })),
      length: 0,
      events: 0,
      bytes: 0,
      questions: Default::default(),
    })
  }

  pub fn len(&self) -> usize {
    self.length
  }

  #[cfg(test)]
  pub fn same_journal(&self, other: &Self) -> bool {
    Arc::ptr_eq(&self.journal, &other.journal)
  }

  pub fn fingerprint(record: &NormalizedRecord) -> Result<[u8; 32], String> {
    Ok(Sha256::digest(serde_json::to_vec(record).map_err(|e| e.to_string())?).into())
  }

  pub fn matches(&self, position: usize, record: &NormalizedRecord) -> Result<bool, String> {
    if position >= self.length {
      return Ok(false);
    }
    let fingerprint = Self::fingerprint(record)?;
    let stored = self.journal.lock().map_err(|e| e.to_string())?.index[position].fingerprint;
    let stored = if let Some(stored) = stored {
      stored
    } else {
      // Append-only readers never compare their history. If a caller does
      // request comparison, populate the digest lazily from committed bytes.
      let stored = Self::fingerprint(&self.read(position)?.0.record)?;
      self.journal.lock().map_err(|e| e.to_string())?.index[position].fingerprint = Some(stored);
      stored
    };
    Ok(stored == fingerprint)
  }

  #[cfg(test)]
  pub fn append(&mut self, records: &[RelayRecord]) -> Result<(), String> {
    self.append_cancellable(records, None)
  }

  pub fn append_cancellable(
    &mut self,
    records: &[RelayRecord],
    cancel: Option<&CancellationToken>,
  ) -> Result<(), String> {
    self.append_with_limit(records, crate::service_protocol::MAX_SNAPSHOT_BYTES, cancel)
  }

  fn append_with_limit(
    &mut self,
    records: &[RelayRecord],
    max_bytes: usize,
    cancel: Option<&CancellationToken>,
  ) -> Result<(), String> {
    self.append_encoded(records, max_bytes, cancel, encode_record)
  }

  fn append_encoded(
    &mut self,
    records: &[RelayRecord],
    max_bytes: usize,
    cancel: Option<&CancellationToken>,
    mut encode: impl FnMut(&RelayRecord) -> Result<EncodedRecord, String>,
  ) -> Result<(), String> {
    let mut journal = self.journal.lock().map_err(|e| e.to_string())?;
    // Only the owner appends, and only at its last committed prefix.
    let mut additions = Vec::with_capacity(records.len());
    let mut offset = self.bytes as u64;
    let mut events = self.events;
    journal.file.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    for record in records {
      check_cancelled(cancel)?;
      // The same encoding is charged to the limit and persisted. No caller
      // needs to serialize a record merely to determine its journal size.
      let (bytes, fingerprint) = encode(record)?;
      if (offset as usize).saturating_add(bytes.len()) > max_bytes {
        return Err("Relay session exceeds the history journal size limit".into());
      }
      journal.file.write_all(&bytes).map_err(|e| e.to_string())?;
      additions.push(RecordIndex {
        offset,
        length: bytes.len(),
        event_start: events,
        events: record.record.events.len(),
        fingerprint,
      });
      offset += bytes.len() as u64;
      events += record.record.events.len();
    }
    check_cancelled(cancel)?;
    let mut event_position = self.events;
    for record in records {
      for event in &record.record.events {
        self.questions.observe(event, event_position);
        match event {
          AgentEvent::Lifecycle(event)
            if matches!(event.scope, LifecycleScope::Turn) && event.phase == Phase::Started =>
          {
            journal.pending_turn = Some(event_position);
          }
          AgentEvent::Lifecycle(event)
            if matches!(event.scope, LifecycleScope::Turn) && event.phase == Phase::Finished =>
          {
            journal.pending_turn = None;
          }
          AgentEvent::Message(message) if message.role == Role::User && !event.is_hidden() => {
            if message.message_id.is_none() || journal.last_user_id != message.message_id {
              let start = journal.pending_turn.take().unwrap_or(event_position);
              journal.turns.push(TurnIndex {
                start,
                user_event: event_position,
              });
              journal.last_user_id.clone_from(&message.message_id);
            }
          }
          _ => {}
        }
        journal.observe_dependency(event, event_position);
        event_position += 1;
      }
    }
    journal.index.extend(additions);
    self.length += records.len();
    self.events = events;
    self.bytes = offset as usize;
    Ok(())
  }

  /// Open at the latest three turns. Earlier requests extend by three user turns.
  /// Reconnect offsets are snapped backwards to a complete turn/record.
  pub fn window_start(&self, retain_from: Option<usize>, before_event: Option<usize>) -> usize {
    let journal = self.journal.lock().unwrap_or_else(|e| e.into_inner());
    let turns = &journal.turns[..journal.turns.partition_point(|turn| turn.user_event < self.events)];
    let start = if let Some(before) = before_event {
      if turns.is_empty() {
        before.min(self.events).saturating_sub(FALLBACK_EVENTS)
      } else {
        let count = turns.partition_point(|turn| turn.start < before);
        if count <= HISTORY_TURNS {
          0
        } else {
          turns[count - HISTORY_TURNS].start
        }
      }
    } else if let Some(start) = retain_from.filter(|start| *start < self.events || *start == 0) {
      if turns.is_empty() || start == 0 {
        start
      } else {
        turns[..turns.partition_point(|turn| turn.start <= start)]
          .last()
          .map_or(0, |turn| turn.start)
      }
    } else {
      self.start_for_span(&journal, INITIAL_HISTORY_TURNS, FALLBACK_EVENTS)
    };
    self.complete_context(&journal, start)
  }

  pub fn retained_turns(&self, start: usize) -> usize {
    let journal = self.journal.lock().unwrap_or_else(|e| e.into_inner());
    let turns = &journal.turns[..journal.turns.partition_point(|turn| turn.user_event < self.events)];
    turns.len() - turns.partition_point(|turn| turn.start < start)
  }

  /// A replacement retains the number of loaded turns, rather than applying
  /// an unrelated old absolute offset or shrinking an expanded window to one.
  pub fn replacement_start(&self, turns: usize, events: usize) -> usize {
    let journal = self.journal.lock().unwrap_or_else(|e| e.into_inner());
    let start = self.start_for_span(&journal, turns.max(INITIAL_HISTORY_TURNS), events.max(FALLBACK_EVENTS));
    self.complete_context(&journal, start)
  }

  fn start_for_span(&self, journal: &Journal, count: usize, fallback: usize) -> usize {
    let turns = &journal.turns[..journal.turns.partition_point(|turn| turn.user_event < self.events)];
    if turns.is_empty() {
      self.events.saturating_sub(fallback)
    } else if turns.len() <= count {
      0
    } else {
      turns[turns.len() - count].start
    }
  }

  /// Late tool results may depend on an invocation before the loaded range.
  /// Include that context atomically, preserving the assembler's identities.
  pub fn context_start(&self, start: usize) -> usize {
    let journal = self.journal.lock().unwrap_or_else(|e| e.into_inner());
    self.complete_context(&journal, start)
  }

  fn complete_context(&self, journal: &Journal, mut start: usize) -> usize {
    if let Some(question) = self.questions.first_index() {
      start = start.min(question);
    }
    loop {
      let before = start;
      let position = journal.index[..self.length].partition_point(|record| record.event_start + record.events <= start);
      if let Some(record) = journal.index[..self.length].get(position) {
        start = start.min(record.event_start);
      }
      // Edges are appended in source order. Scanning backwards also closes
      // transitive dependencies as the retained start moves backwards.
      for &(observation, source) in journal.dependencies.iter().rev() {
        if observation >= self.events {
          continue;
        }
        if observation < start {
          break;
        }
        start = start.min(source);
      }
      if start == before {
        return start;
      }
    }
  }

  pub fn record_at_event(&self, event: usize) -> usize {
    let journal = self.journal.lock().unwrap_or_else(|e| e.into_inner());
    journal.index[..self.length].partition_point(|record| record.event_start + record.events <= event)
  }

  pub fn read(&self, position: usize) -> Result<(RelayRecord, usize), String> {
    if position >= self.length {
      return Err("History record outside committed snapshot".into());
    }
    let mut journal = self.journal.lock().map_err(|e| e.to_string())?;
    let index = &journal.index[position];
    let (offset, length, event_start) = (index.offset, index.length, index.event_start);
    let mut bytes = vec![0; length];
    journal.file.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    journal.file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
    Ok((serde_json::from_slice(&bytes).map_err(|e| e.to_string())?, event_start))
  }

  #[cfg(test)]
  pub fn iter(&self) -> impl Iterator<Item = RelayRecord> + '_ {
    (0..self.length).map(|position| self.read(position).unwrap().0)
  }
}

pub(crate) fn check_cancelled(cancel: Option<&CancellationToken>) -> Result<(), String> {
  if cancel.is_some_and(CancellationToken::is_cancelled) {
    Err("Session snapshot read cancelled".into())
  } else {
    Ok(())
  }
}

/// Keep the private journal's flattened Relay encoding while serializing its
/// large normalized payload only once. The small envelope contains no bodies.
#[derive(serde::Serialize)]
struct RecordEnvelope<'a> {
  path: &'a std::path::Path,
  topic: &'a str,
  session: &'a SessionContext,
  operation: RecordOperation,
}

fn encode_record(record: &RelayRecord) -> Result<EncodedRecord, String> {
  if matches!(record.session.provider, Provider::Codex | Provider::Pi) {
    // These readers detect replacement using source revisions/cursors, so
    // computing a digest for every historical record performs unused work.
    return Ok((serde_json::to_vec(record).map_err(|e| e.to_string())?, None));
  }
  let normalized = serde_json::to_vec(&record.record).map_err(|e| e.to_string())?;
  let fingerprint = Sha256::digest(&normalized).into();
  let mut bytes = serde_json::to_vec(&RecordEnvelope {
    path: &record.path,
    topic: &record.topic,
    session: &record.session,
    operation: record.operation,
  })
  .map_err(|e| e.to_string())?;
  // Both encodings are nonempty JSON objects produced from known structs.
  // Merge their members without parsing or encoding the payload again.
  bytes.pop();
  bytes.push(b',');
  bytes.extend_from_slice(&normalized[1..]);
  Ok((bytes, Some(fingerprint)))
}

impl Journal {
  fn observe_dependency(&mut self, event: &AgentEvent, position: usize) {
    match event {
      AgentEvent::ToolCall(tool) => {
        let Some((session, call)) = tool.session_id.as_deref().zip(tool.tool_call_id.as_deref()) else {
          return;
        };
        if session.trim().is_empty() || call.trim().is_empty() {
          return;
        }
        let key = (
          tool.provider,
          session.trim().to_owned(),
          tool.turn_id.clone(),
          call.trim().to_owned(),
        );
        let terminal = matches!(tool.record_kind, ToolRecordKind::Result)
          || tool.is_error == Some(true)
          || (matches!(tool.record_kind, ToolRecordKind::Snapshot) && tool.phase == Phase::Finished);
        if let Some(anchor) = self.tools.get_mut(&key) {
          self.dependencies.push((position, anchor.first));
          if matches!(tool.record_kind, ToolRecordKind::Invocation) {
            if !terminal {
              anchor.ambiguous = true;
            }
          } else if terminal && !anchor.ambiguous {
            self.tools.remove(&key);
          }
        } else if !terminal {
          self.tools.insert(
            key,
            ToolAnchor {
              first: position,
              ambiguous: false,
            },
          );
        }
      }
      AgentEvent::Compaction(compaction) => {
        let Some(id) = compaction.compaction_id.as_ref().filter(|id| !id.is_empty()) else {
          return;
        };
        let key = (compaction.provider, compaction.session_id.clone(), id.clone());
        if let Some(first) = self.compactions.get(&key) {
          self.dependencies.push((position, *first));
        } else {
          self.compactions.insert(key, position);
        }
      }
      _ => {}
    }
  }
}

#[cfg(test)]
mod tests {
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
    let events: Vec<_> = include_str!("../tests/fixtures/codex/async_question_replies.jsonl")
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
        include_str!("../tests/fixtures/codex/async_question_replies.jsonl")
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
        let (encoded, fingerprint) = encode_record(&record).unwrap();
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
    let mut history = History::new().unwrap();
    history.append(&[initial_record]).unwrap();
    let initial = history.clone();
    let limit = history.bytes + serde_json::to_vec(&additions[0]).unwrap().len();
    assert!(history.append_with_limit(&additions, limit, None).is_err());
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
          .append_encoded(records, crate::service_protocol::MAX_SNAPSHOT_BYTES, None, |record| {
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
}
