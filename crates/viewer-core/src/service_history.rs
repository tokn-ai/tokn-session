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

mod budget;
use budget::{Budget, Charge, Usage};

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
  budget: Budget,
  charge: Charge,
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
  questions: Arc<crate::questions::Questions>,
}

impl History {
  pub fn new() -> Result<Self, String> {
    Self::with_budget(Budget::shared())
  }

  fn with_budget(budget: Budget) -> Result<Self, String> {
    let charge = budget.reserve(Usage::default())?;
    Ok(Self {
      journal: Arc::new(Mutex::new(Journal {
        file: tempfile::tempfile().map_err(|e| format!("Cannot create session history journal: {e}"))?,
        budget,
        charge,
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
    Ok(Sha256::digest(encode_bounded(record, budget::MAX_RECORD_BYTES)?).into())
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
    self.append_encoded(records, budget::MAX_RECORD_BYTES, cancel, |record| {
      encode_record(record, budget::MAX_RECORD_BYTES)
    })
  }

  fn append_encoded(
    &mut self,
    records: &[RelayRecord],
    max_record_bytes: usize,
    cancel: Option<&CancellationToken>,
    mut encode: impl FnMut(&RelayRecord) -> Result<EncodedRecord, String>,
  ) -> Result<(), String> {
    let mut journal = self.journal.lock().map_err(|e| e.to_string())?;
    // A snapshot may share an older immutable prefix. It cannot overwrite a
    // newer committed suffix belonging to the same physical journal.
    if self.length != journal.index.len() {
      return Err("Cannot append to an older history snapshot".into());
    }
    check_cancelled(cancel)?;
    let mut charge = journal.budget.reserve(Usage {
      records: records.len(),
      index_bytes: index_cost(records)?,
      disk_bytes: 0,
    })?;
    let mut additions = Vec::with_capacity(records.len());
    let mut offset = self.bytes as u64;
    let mut events = self.events;
    journal.file.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    let write_result = (|| -> Result<(), String> {
      for record in records {
        check_cancelled(cancel)?;
        // Bound encoding before allocation and charge the actual bytes before
        // writing them. No transcript-sized buffer or session-size gate exists.
        let (bytes, fingerprint) = encode(record)?;
        if bytes.len() > max_record_bytes {
          return Err("History record exceeds the encoding size limit".into());
        }
        charge.merge(journal.budget.reserve(Usage {
          disk_bytes: bytes.len(),
          ..Usage::default()
        })?);
        journal.file.write_all(&bytes).map_err(|e| e.to_string())?;
        additions.push(RecordIndex {
          offset,
          length: bytes.len(),
          event_start: events,
          events: record.record.events.len(),
          fingerprint,
        });
        offset += bytes.len() as u64;
        events = events
          .checked_add(record.record.events.len())
          .ok_or("History event index overflow")?;
      }
      check_cancelled(cancel)
    })();
    if let Err(error) = write_result {
      if let Err(rollback) = journal.file.set_len(self.bytes as u64) {
        // If truncation fails, keep credits until the physical journal drops;
        // releasing them would allow unaccounted scratch storage to accumulate.
        journal.charge.merge(charge);
        return Err(format!("{error}; failed to roll back history journal: {rollback}"));
      }
      return Err(error);
    }
    let mut event_position = self.events;
    for record in records {
      for event in &record.record.events {
        Arc::make_mut(&mut self.questions).observe(event, event_position);
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
    journal.charge.merge(charge);
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

/// Conservative admission for compact indexes, including identities retained
/// by turn, dependency, and question correlation. Payload text stays on disk.
/// Charging every observation also covers allocator growth and retired entries;
/// this estimates retained memory rather than claiming exact allocator usage.
fn index_cost(records: &[RelayRecord]) -> Result<usize, String> {
  let mut bytes = records
    .len()
    .checked_mul(2 * std::mem::size_of::<RecordIndex>())
    .ok_or("History index memory budget exhausted")?;
  let mut charge = |length: usize| -> Result<(), String> {
    bytes = bytes
      .checked_add(length)
      .ok_or("History index memory budget exhausted")?;
    Ok(())
  };
  for record in records {
    // The owning source reader also retains record identities for window
    // reconciliation. Include that companion index in the shared admission.
    charge(
      record
        .record
        .record_id
        .len()
        .checked_mul(4)
        .ok_or("History index memory budget exhausted")?,
    )?;
    for event in &record.record.events {
      charge(512)?;
      let mut identity = |text: Option<&str>| -> Result<(), String> {
        charge(
          text
            .map_or(0, str::len)
            .checked_mul(4)
            .ok_or("History index memory budget exhausted")?,
        )
      };
      match event {
        AgentEvent::Lifecycle(event) => identity(Some(&event.turn_id))?,
        AgentEvent::Message(message) if message.role == Role::User => identity(message.message_id.as_deref())?,
        AgentEvent::ToolCall(tool) => {
          identity(tool.session_id.as_deref())?;
          identity(tool.turn_id.as_deref())?;
          identity(tool.tool_call_id.as_deref())?;
        }
        AgentEvent::Compaction(compaction) => {
          identity(compaction.session_id.as_deref())?;
          identity(compaction.compaction_id.as_deref())?;
        }
        AgentEvent::QuestionRequest(request) => {
          identity(request.request_id.as_deref())?;
          identity(request.turn_id.as_deref())?;
          for question in &request.questions {
            identity(question.id.as_deref())?;
          }
          charge(
            request
              .questions
              .len()
              .checked_mul(256)
              .ok_or("History index memory budget exhausted")?,
          )?;
        }
        _ => {}
      }
    }
  }
  Ok(bytes)
}

fn encode_bounded(value: &impl serde::Serialize, limit: usize) -> Result<Vec<u8>, String> {
  struct BoundedBytes {
    bytes: Vec<u8>,
    limit: usize,
  }
  impl Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
      if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
        return Err(std::io::Error::other("History record exceeds the encoding size limit"));
      }
      self.bytes.extend_from_slice(bytes);
      Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
      Ok(())
    }
  }
  let mut writer = BoundedBytes {
    bytes: Vec::new(),
    limit,
  };
  serde_json::to_writer(&mut writer, value).map_err(|error| error.to_string())?;
  Ok(writer.bytes)
}

fn encode_record(record: &RelayRecord, max_bytes: usize) -> Result<EncodedRecord, String> {
  if matches!(record.session.provider, Provider::Codex | Provider::Pi) {
    // These readers detect replacement using source revisions/cursors, so
    // computing a digest for every historical record performs unused work.
    return Ok((encode_bounded(record, max_bytes)?, None));
  }
  let normalized = encode_bounded(&record.record, max_bytes)?;
  let fingerprint = Sha256::digest(&normalized).into();
  let mut bytes = encode_bounded(
    &RecordEnvelope {
      path: &record.path,
      topic: &record.topic,
      session: &record.session,
      operation: record.operation,
    },
    max_bytes,
  )?;
  if bytes.len().saturating_add(normalized.len()).saturating_sub(1) > max_bytes {
    return Err("History record exceeds the encoding size limit".into());
  }
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
mod tests;
