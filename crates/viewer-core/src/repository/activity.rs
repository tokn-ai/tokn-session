//! Compact body projection: final replies and the most recent turn state.
//! JSONL readers retain decoding cursors, never completed transcript records.
use std::{collections::HashMap, time::Instant};

use tokn_session_core::{AgentEvent, LifecycleScope, MessageDelivery, Phase, Provider, Role, ToolRecordKind};
use tokn_session_index::SessionPresentation;

use crate::{
  model::{SessionLocator, ViewerProvider},
  service_source::{FileVersion, versions},
};

#[derive(Clone, Default)]
pub(crate) struct Activity {
  pub final_count: u64,
  pub running: bool,
  active_turn: Option<String>,
  last_final_id: Option<String>,
  pub presentation: SessionPresentation,
  questions: crate::questions::Questions,
  pub question_attention: crate::model::QuestionAttention,
}

impl Activity {
  pub fn from_loaded(loaded: tokn_session_core::LoadedSession, session_id: &str) -> Result<Self, String> {
    if loaded.reference.id != session_id {
      return Err("Session activity owner changed".into());
    }
    let mut activity = Self {
      presentation: SessionPresentation {
        title: loaded.reference.title.map(|v| v.chars().take(160).collect()),
        preview: loaded.reference.preview.map(|v| v.chars().take(240).collect()),
      },
      ..Self::default()
    };
    for event in &loaded.events {
      activity.observe(event);
    }
    Ok(activity)
  }

  pub fn observe(&mut self, event: &AgentEvent) {
    if event.is_hidden() {
      return;
    }
    self.questions.observe(event, 0);
    self.question_attention = self.questions.summary();
    match event {
      AgentEvent::Lifecycle(l) if matches!(l.scope, LifecycleScope::Turn) => {
        if l.phase == Phase::Started {
          self.active_turn = Some(l.turn_id.clone());
          self.running = true;
        } else if l.phase == Phase::Finished && self.active_turn.as_ref().is_none_or(|id| id == &l.turn_id) {
          self.running = false;
          self.active_turn = None;
        }
      }
      AgentEvent::Message(m)
        if m.role == Role::Assistant && m.delivery == MessageDelivery::Final && m.phase == Phase::Finished =>
      {
        if m.message_id.is_none() || m.message_id != self.last_final_id {
          self.final_count = self.final_count.saturating_add(1);
        }
        self.last_final_id = m.message_id.clone();
        self.running = false;
        self.active_turn = None;
      }
      // Providers without turn lifecycle records can still expose active work.
      AgentEvent::ToolCall(t) if matches!(t.record_kind, ToolRecordKind::Invocation | ToolRecordKind::Progress) => {
        self.running = true
      }
      AgentEvent::Reasoning(_) => self.running = true,
      AgentEvent::Error(_) => {
        self.running = false;
        self.active_turn = None;
      }
      AgentEvent::Message(m)
        if m.role == Role::Assistant && m.delivery == MessageDelivery::Final && m.phase != Phase::Finished =>
      {
        self.running = true
      }
      AgentEvent::Message(m) if m.role == Role::Assistant && m.delivery == MessageDelivery::Commentary => {
        self.running = true
      }
      _ => {}
    }
  }

  pub fn marker(&self) -> String {
    format!(
      "session-activity.v4.{}.{}.{}.{}",
      self.final_count,
      u8::from(self.running),
      self.question_attention.required_count,
      self.question_attention.available_count
    )
  }

  pub fn from_marker(marker: Option<&str>) -> Option<Self> {
    let marker = marker?;
    if let Some(body) = marker
      .strip_prefix("session-activity.v4.")
      .or_else(|| marker.strip_prefix("session-activity.v3."))
    {
      let parts: Vec<_> = body.split('.').collect();
      if parts.len() != 4 {
        return None;
      }
      return Some(Self {
        final_count: parts[0].parse().ok()?,
        running: match parts[1] {
          "0" => false,
          "1" => true,
          _ => return None,
        },
        question_attention: crate::model::QuestionAttention {
          required_count: parts[2].parse().ok()?,
          available_count: parts[3].parse().ok()?,
        },
        ..Default::default()
      });
    }
    let (count, running) = marker.strip_prefix("final-replies.v2.")?.split_once('.')?;
    Some(Self {
      final_count: count.parse().ok()?,
      running: match running {
        "0" => false,
        "1" => true,
        _ => return None,
      },
      ..Default::default()
    })
  }
}

enum Reader {
  Codex(Box<tokn_session_codex::CodexHistoryReader>),
  Pi(Box<tokn_session_relay::JsonlReader>),
}

struct Entry {
  reader: Reader,
  activity: Activity,
  version: Vec<Option<FileVersion>>,
  used: Instant,
}

#[derive(Default)]
pub(super) struct ActivityReaders {
  entries: HashMap<SessionLocator, Entry>,
}

impl ActivityReaders {
  pub fn contains(&self, locator: &SessionLocator) -> bool {
    self.entries.contains_key(locator)
  }

  pub fn read(&mut self, locator: &SessionLocator) -> Result<Activity, String> {
    // Removing the entry makes every error invalidate the cursor atomically.
    let version = versions(&locator.source_path, false);
    if version
      .first()
      .and_then(Option::as_ref)
      .is_some_and(|v| v.length > crate::service_protocol::MAX_SNAPSHOT_BYTES as u64)
    {
      return Err("Session activity exceeds the source size limit".into());
    }
    let mut entry = self.entries.remove(locator);
    if locator.provider == ViewerProvider::Pi
      && entry.as_ref().is_some_and(|entry| {
        version != entry.version
          && version[0]
            .as_ref()
            .zip(entry.version[0].as_ref())
            .is_none_or(|(after, before)| {
              #[cfg(unix)]
              if after.identity != before.identity {
                return true;
              }
              after.length <= before.length
            })
      })
    {
      entry = None;
    }
    let mut entry = match entry {
      Some(entry) => entry,
      None => Entry {
        reader: match locator.provider {
          ViewerProvider::Codex => Reader::Codex(Box::new(tokn_session_codex::CodexHistoryReader::new(
            locator.source_path.clone(),
            false,
            crate::service_protocol::MAX_SNAPSHOT_BYTES,
          ))),
          ViewerProvider::Pi => Reader::Pi(Box::new(tokn_session_relay::JsonlReader::for_snapshot(
            locator.source_path.clone(),
            Provider::Pi,
            false,
            locator.source_path.parent().unwrap_or(std::path::Path::new(".")),
          )?)),
          _ => return Err("Incremental activity requires a JSONL source".into()),
        },
        activity: Activity::default(),
        version: Vec::new(),
        used: Instant::now(),
      },
    };
    match &mut entry.reader {
      Reader::Codex(reader) => {
        if let Some(update) = reader.poll(&tokn_session_codex::CodexSessionSource::new(None))? {
          if update.reference.id != locator.session_id {
            return Err("Session activity owner changed".into());
          }
          if update.reset {
            entry.activity = Activity::default();
          }
          entry.activity.presentation = SessionPresentation {
            title: update.reference.title.map(|v| v.chars().take(160).collect()),
            preview: update.reference.preview.map(|v| v.chars().take(240).collect()),
          };
          for record in update.records {
            for event in record.events {
              entry.activity.observe(&event);
            }
          }
        }
      }
      Reader::Pi(reader) => {
        let (update, reset) = reader.follow_snapshot()?;
        if !update.warnings.is_empty() {
          return Err(update.warnings.join("; "));
        }
        if reset {
          entry.activity = Activity::default();
        }
        for record in update.records {
          if record.session.session_id != locator.session_id {
            return Err("Session activity owner changed".into());
          }
          if let Some(title) = record.session.title {
            entry.activity.presentation.title = Some(title.chars().take(160).collect());
          }
          for event in record.record.events {
            if entry.activity.presentation.preview.is_none() {
              if let AgentEvent::Message(m) = &event {
                if m.role == Role::User && !event.is_hidden() {
                  entry.activity.presentation.preview = Some(m.text.chars().take(240).collect());
                }
              }
            }
            entry.activity.observe(&event);
          }
        }
      }
    }
    entry.version = version;
    entry.used = Instant::now();
    let activity = entry.activity.clone();
    // Only hot sources need a cursor. Cold entries can be reconstructed from
    // provider history without keeping thousands of normalizers resident.
    if self.entries.len() >= 32 {
      if let Some(oldest) = self
        .entries
        .iter()
        .min_by_key(|(_, e)| e.used)
        .map(|(key, _)| key.clone())
      {
        self.entries.remove(&oldest);
      }
    }
    self.entries.insert(locator.clone(), entry);
    Ok(activity)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;
  use std::io::Write;
  use tokn_session_core::{LifecycleEvent, MessageEvent};

  fn message(role: Role, delivery: MessageDelivery, phase: Phase) -> AgentEvent {
    AgentEvent::Message(MessageEvent {
      provenance: None,
      provider: Provider::Codex,
      session_id: None,
      message_id: None,
      parent_id: None,
      role,
      delivery,
      phase,
      text: "reply".into(),
      timestamp: None,
    })
  }

  fn lifecycle(turn: &str, scope: LifecycleScope, phase: Phase) -> AgentEvent {
    AgentEvent::Lifecycle(LifecycleEvent {
      provider: Provider::Codex,
      session_id: None,
      turn_id: turn.into(),
      step_id: None,
      scope,
      phase,
      outcome: None,
      native: json!({}),
      timestamp: None,
    })
  }

  #[test]
  fn only_completed_final_replies_count_and_running_preserves_the_count() {
    let mut activity = Activity::default();
    activity.observe(&message(Role::User, MessageDelivery::Unspecified, Phase::Finished));
    activity.observe(&message(Role::Assistant, MessageDelivery::Final, Phase::Delta));
    assert_eq!(activity.final_count, 0);
    activity.observe(&lifecycle("current", LifecycleScope::Turn, Phase::Started));
    activity.observe(&lifecycle("old", LifecycleScope::Turn, Phase::Finished));
    activity.observe(&lifecycle("current", LifecycleScope::Step, Phase::Finished));
    assert!(activity.running);
    activity.observe(&message(Role::Assistant, MessageDelivery::Final, Phase::Finished));
    assert_eq!(activity.final_count, 1);
    assert!(!activity.running);
    activity.observe(&message(Role::Assistant, MessageDelivery::Commentary, Phase::Finished));
    assert!(activity.running);
    assert_eq!(activity.final_count, 1);
    activity.observe(&lifecycle("current", LifecycleScope::Turn, Phase::Finished));
    assert!(!activity.running);
    let restored = Activity::from_marker(Some(&activity.marker())).unwrap();
    assert_eq!(restored.final_count, 1);
    assert!(!restored.running);
  }

  #[test]
  fn multiple_parts_of_one_final_message_count_once() {
    let mut activity = Activity::default();
    let mut event = message(Role::Assistant, MessageDelivery::Final, Phase::Finished);
    if let AgentEvent::Message(message) = &mut event {
      message.message_id = Some("one".into());
    }
    activity.observe(&event);
    activity.observe(&event);
    assert_eq!(activity.final_count, 1);
    if let AgentEvent::Message(message) = &mut event {
      message.message_id = Some("two".into());
    }
    activity.observe(&event);
    assert_eq!(activity.final_count, 2);
  }

  #[test]
  fn activity_markers_preserve_legacy_counts_while_writing_the_reconciled_version() {
    for version in [3, 4] {
      let activity = Activity::from_marker(Some(&format!("session-activity.v{version}.12.1.2.3"))).unwrap();
      assert_eq!(activity.final_count, 12);
      assert!(activity.running);
      assert_eq!(activity.question_attention.required_count, 2);
      assert_eq!(activity.question_attention.available_count, 3);
      assert_eq!(activity.marker(), "session-activity.v4.12.1.2.3");
    }
  }

  #[test]
  fn codex_activity_reads_only_appended_complete_rows_and_rebuilds_after_corruption() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("session.jsonl");
    let header = json!({"type":"session_meta","payload":{"id":"session","cwd":"/tmp"}}).to_string() + "\n";
    std::fs::write(&path, &header).unwrap();
    let locator = SessionLocator {
      version: 1,
      provider: ViewerProvider::Codex,
      session_id: "session".into(),
      source_path: path.clone(),
    };
    let mut readers = ActivityReaders::default();
    assert_eq!(readers.read(&locator).unwrap().final_count, 0);
    let final_row = json!({"type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"done"}]}}).to_string();
    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    write!(file, "{final_row}").unwrap();
    assert_eq!(readers.read(&locator).unwrap().final_count, 0);
    writeln!(file).unwrap();
    assert_eq!(readers.read(&locator).unwrap().final_count, 1);
    let stats = match &readers.entries[&locator].reader {
      Reader::Codex(reader) => reader.stats(),
      _ => unreachable!(),
    };
    assert_eq!(readers.read(&locator).unwrap().final_count, 1);
    let after = match &readers.entries[&locator].reader {
      Reader::Codex(reader) => reader.stats(),
      _ => unreachable!(),
    };
    assert_eq!(stats.rows_parsed, after.rows_parsed);
    writeln!(file, "{{broken").unwrap();
    assert!(readers.read(&locator).is_err());
    assert!(!readers.contains(&locator));
    std::fs::write(&path, format!("{header}{final_row}\n{final_row}\n")).unwrap();
    assert_eq!(readers.read(&locator).unwrap().final_count, 2);
    std::fs::write(&path, &header).unwrap();
    assert_eq!(readers.read(&locator).unwrap().final_count, 0);
  }

  #[test]
  fn pi_activity_preserves_counts_on_append_and_resets_same_size_rewrites() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("session.jsonl");
    let header = json!({"type":"session","id":"session","version":3,"cwd":"/tmp","timestamp":"2026-09-25T00:00:00Z"})
      .to_string()
      + "\n";
    let reply =
      json!({"type":"message","id":"m1","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}})
        .to_string()
        + "\n";
    std::fs::write(&path, format!("{header}{reply}")).unwrap();
    let locator = SessionLocator {
      version: 1,
      provider: ViewerProvider::Pi,
      session_id: "session".into(),
      source_path: path.clone(),
    };
    let mut readers = ActivityReaders::default();
    assert_eq!(readers.read(&locator).unwrap().final_count, 1);
    assert_eq!(readers.read(&locator).unwrap().final_count, 1);
    std::fs::write(&path, format!("{header}{}", reply.replace("done", "edit"))).unwrap();
    assert_eq!(readers.read(&locator).unwrap().final_count, 1);
    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    write!(file, "{}", reply.replace("m1", "m2")).unwrap();
    assert_eq!(readers.read(&locator).unwrap().final_count, 2);
  }
}
