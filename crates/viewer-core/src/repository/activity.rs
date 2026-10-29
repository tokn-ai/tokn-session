//! Compact body projection: attention, latest assistant line, and turn state.
//! JSONL readers retain decoding cursors, never completed transcript records.
use std::{collections::HashMap, time::Instant};

use sha2::{Digest, Sha256};
use tokn_session_core::{
  AgentEvent, LifecycleScope, MessageDelivery, MessageEvent, Phase, Provider, Role, ToolRecordKind,
};
use tokn_session_index::SessionPresentation;

use crate::{
  model::{SessionLocator, ViewerProvider},
  service_source::{FileVersion, versions},
};

/// Only the first nonempty display line survives streaming. Whitespace and
/// line boundaries can cross chunks; no complete message text is retained.
#[derive(Clone, Default)]
struct AssistantLine {
  text: String,
  chars: usize,
  pending_space: bool,
  complete: bool,
}

impl AssistantLine {
  fn append(&mut self, fragment: &str) {
    if self.complete {
      return;
    }
    for character in fragment.chars() {
      if character == '\n' {
        if !self.text.is_empty() {
          self.complete = true;
          break;
        }
        self.pending_space = false;
      } else if character.is_whitespace() {
        self.pending_space = !self.text.is_empty();
      } else {
        if self.pending_space && self.chars < 240 {
          self.text.push(' ');
          self.chars += 1;
        }
        self.pending_space = false;
        if self.chars < 240 {
          self.text.push(character);
          self.chars += 1;
        }
        if self.chars == 240 {
          self.complete = true;
          break;
        }
      }
    }
  }

  fn preview(&self) -> Option<&str> {
    (!self.text.is_empty()).then_some(self.text.as_str())
  }
}

#[derive(Clone)]
struct AssistantStream {
  provider: Provider,
  // Exact identity matching stays bounded even if provider IDs are huge.
  message_id: Option<[u8; 32]>,
  delivery: MessageDelivery,
  line: AssistantLine,
}

#[derive(Clone)]
pub(crate) struct Activity {
  pub final_count: u64,
  pub running: bool,
  active_turn: Option<String>,
  last_final_id: Option<String>,
  assistant_stream: Option<AssistantStream>,
  pub presentation: SessionPresentation,
  questions: crate::questions::Questions,
  pub question_attention: crate::model::QuestionAttention,
}

impl Default for Activity {
  fn default() -> Self {
    Self {
      final_count: 0,
      running: false,
      active_turn: None,
      last_final_id: None,
      assistant_stream: None,
      presentation: SessionPresentation {
        latest_message: Some("null".into()),
        ..Default::default()
      },
      questions: Default::default(),
      question_attention: Default::default(),
    }
  }
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
        latest_message: Some("null".into()),
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
    let response_cancelled = event.is_response_cancelled();
    if response_cancelled {
      self.assistant_stream = None;
    }
    match event {
      AgentEvent::Lifecycle(lifecycle) if lifecycle.phase == Phase::Started => self.assistant_stream = None,
      AgentEvent::Message(message) if message.role == Role::User => self.assistant_stream = None,
      AgentEvent::ToolCall(tool)
        if tool.record_kind == ToolRecordKind::Invocation
          && self
            .assistant_stream
            .as_ref()
            .is_some_and(|stream| stream.message_id.is_none()) =>
      {
        self.assistant_stream = None
      }
      AgentEvent::Error(_) | AgentEvent::SessionStarted(_) => self.assistant_stream = None,
      _ => {}
    }
    if let AgentEvent::Message(message) = event
      && message.role == Role::Assistant
    {
      self.observe_assistant(message);
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
          self.assistant_stream = None;
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
      _ if response_cancelled => {
        self.running = false;
        self.active_turn = None;
      }
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

  fn observe_assistant(&mut self, message: &MessageEvent) {
    let fragments = matches!(message.provider, Provider::Codex | Provider::Dsh);
    let identity: Option<[u8; 32]> = message
      .message_id
      .as_ref()
      .map(|id| Sha256::digest(id.as_bytes()).into());
    let mut stream = if fragments && message.phase == Phase::Delta {
      self
        .assistant_stream
        .take()
        .filter(|stream| stream.provider == message.provider && stream.message_id == identity)
    } else {
      None
    }
    .unwrap_or_else(|| AssistantStream {
      provider: message.provider,
      message_id: identity,
      delivery: message.delivery,
      line: AssistantLine::default(),
    });
    if message.delivery != MessageDelivery::Unspecified {
      stream.delivery = message.delivery;
    }
    stream.line.append(&message.text);
    // Codex/DSH deltas append fragments. OpenCode/ZCode deltas and all other
    // providers contain snapshots, so they replace the compact line. A finished
    // canonical snapshot replaces any partial accumulated stream as well.
    self.presentation.latest_message = Some(
      serde_json::json!({
        "message_id": message.message_id.as_ref().map(|id| id.chars().take(256).collect::<String>()),
        "delivery": stream.delivery,
        "phase": message.phase,
        "preview": stream.line.preview(),
      })
      .to_string(),
    );
    self.assistant_stream = (fragments && message.phase != Phase::Finished).then_some(stream);
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
  Codex(Box<tokn_session_codex::CodexHistoryProjectionReader>),
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
          ViewerProvider::Codex => Reader::Codex(Box::new(tokn_session_codex::CodexHistoryProjectionReader::new(
            locator.source_path.clone(),
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
        // The entry is detached until the whole update validates. A late parse,
        // lineage, or owner error discards both the staged activity and cursor.
        if let Some(update) = reader.poll(&tokn_session_codex::CodexSessionSource::new(None), |reset, record| {
          if reset {
            entry.activity = Activity::default();
          }
          for event in record.events {
            entry.activity.observe(&event);
          }
        })? {
          if update.reference.id != locator.session_id {
            return Err("Session activity owner changed".into());
          }
          entry.activity.presentation.title = update.reference.title.map(|v| v.chars().take(160).collect());
          entry.activity.presentation.preview = update.reference.preview.map(|v| v.chars().take(240).collect());
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
      images: Vec::new(),
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

  fn pending_codex_response() -> (tokn_session_codex::normalize::CodexNormalizer, Activity) {
    let mut normalizer = tokn_session_codex::normalize::CodexNormalizer::new();
    let mut activity = Activity::default();
    for payload in [
      json!({"type":"task_started", "turn_id":"turn"}),
      json!({"type":"request_user_input", "call_id":"request", "turn_id":"turn", "questions":[
        {"id":"question", "header":"Choice", "question":"Which option?"}
      ]}),
    ] {
      for event in normalizer.normalize(serde_json::from_value(json!({"type":"event_msg", "payload":payload})).unwrap())
      {
        activity.observe(&event);
      }
    }
    assert!(activity.running);
    assert_eq!(activity.question_attention.required_count, 1);
    (normalizer, activity)
  }

  #[test]
  fn normalized_codex_abort_without_turn_identity_stops_activity_and_retires_questions() {
    for reason in ["interrupted", "replaced", "review_ended", "budget_limited"] {
      for explicit_null in [false, true] {
        let (mut normalizer, mut activity) = pending_codex_response();
        let mut payload = json!({"type":"turn_aborted", "reason":reason});
        if explicit_null {
          payload["turn_id"] = serde_json::Value::Null;
        }
        let events =
          normalizer.normalize(serde_json::from_value(json!({"type":"event_msg", "payload":payload})).unwrap());
        assert!(matches!(&events[..], [AgentEvent::Metadata(event)]
          if matches!(event.kind, tokn_session_core::MetadataKind::Diagnostic)));
        assert!(events[0].is_response_cancelled());
        for event in events {
          activity.observe(&event);
        }
        assert!(!activity.running);
        assert!(activity.active_turn.is_none());
        assert_eq!(activity.question_attention, Default::default());
        assert_eq!(activity.final_count, 0);
      }
    }
  }

  #[test]
  fn normalized_codex_control_errors_leave_the_active_response_and_questions_pending() {
    for code in [
      json!("thread_rollback_failed"),
      json!({"active_turn_not_steerable":{"turn_kind":"review"}}),
      json!({"active_turn_not_steerable":{"turn_kind":"compact"}}),
    ] {
      let (mut normalizer, mut activity) = pending_codex_response();
      let events = normalizer.normalize(
        serde_json::from_value(json!({
          "type":"event_msg", "payload":{"type":"error", "message":"Request rejected", "codex_error_info":code}
        }))
        .unwrap(),
      );
      assert!(matches!(&events[..], [AgentEvent::Metadata(event)]
        if matches!(event.kind, tokn_session_core::MetadataKind::Diagnostic)));
      for event in events {
        assert!(!event.is_response_cancelled());
        activity.observe(&event);
      }
      assert!(activity.running);
      assert_eq!(activity.active_turn.as_deref(), Some("turn"));
      assert_eq!(activity.question_attention.required_count, 1);
      assert_eq!(activity.final_count, 0);
    }
  }

  #[test]
  fn normalized_pi_cancellation_stops_partial_reasoning_and_tool_activity() {
    let mut normalizer = tokn_session_pi::normalize::PiNormalizer::new();
    normalizer.normalize(serde_json::from_value(json!({"type":"session", "id":"session"})).unwrap());
    let events = normalizer.normalize(
      serde_json::from_value(json!({
        "type":"message", "id":"assistant", "parentId":"user", "message":{
          "role":"assistant", "content":[{"type":"thinking", "thinking":"Partial reasoning"},
            {"type":"toolCall", "id":"call", "name":"read", "arguments":{"path":"file"}}],
          "stopReason":"aborted", "errorMessage":"Request aborted."
        }
      }))
      .unwrap(),
    );
    let mut activity = Activity::default();
    let mut saw_work = false;
    for event in events {
      activity.observe(&event);
      if matches!(event, AgentEvent::Reasoning(_) | AgentEvent::ToolCall(_)) {
        assert!(activity.running);
        saw_work = true;
      }
    }
    assert!(saw_work);
    assert!(!activity.running);
    assert!(activity.active_turn.is_none());
    assert!(activity.assistant_stream.is_none());
    assert_eq!(activity.final_count, 0);
  }

  #[test]
  fn normalized_opencode_and_zcode_reasoning_only_errors_and_cancellations_end_activity() {
    for zcode in [false, true] {
      for cancelled in [false, true] {
        for recovered in [false, true] {
          let directory = tempfile::tempdir().unwrap();
          let path = directory.path().join("db.sqlite");
          let connection = rusqlite::Connection::open(&path).unwrap();
          connection.execute_batch(
            "CREATE TABLE session(id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER);
             CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);
             INSERT INTO session VALUES('session', NULL, '/synthetic/project', 'Errors', 100, 101);
             INSERT INTO part VALUES('reasoning', 'assistant', 'session', 101, '{\"type\":\"reasoning\",\"text\":\"Partial reasoning\"}');",
          ).unwrap();
          let mut data = json!({"role":"assistant", "parentID":"user", "error":{
            "name":if cancelled {"MessageAbortedError"} else {"APIError"}, "data":{"message":"terminal outcome"}
          }});
          if recovered {
            data["providerID"] = json!(42);
          }
          connection
            .execute(
              "INSERT INTO message VALUES('assistant', 'session', 101, ?1)",
              [data.to_string()],
            )
            .unwrap();
          drop(connection);
          let source = if zcode {
            tokn_session_opencode::OpenCodeSessionSource::for_zcode(Some(path))
          } else {
            tokn_session_opencode::OpenCodeSessionSource::new(Some(path))
          };
          let loaded = source.load_session_exact("session").unwrap();
          let mut activity = Activity::default();
          let mut saw_reasoning = false;
          for event in &loaded.events {
            activity.observe(event);
            if matches!(event, AgentEvent::Reasoning(_)) {
              assert!(activity.running);
              saw_reasoning = true;
            }
          }
          assert!(saw_reasoning);
          assert!(
            !activity.running,
            "zcode={zcode}, cancelled={cancelled}, recovered={recovered}"
          );
          assert!(activity.active_turn.is_none());
          assert!(activity.assistant_stream.is_none());
          assert_eq!(activity.final_count, 0);
        }
      }
    }
  }

  #[test]
  fn warnings_retries_and_unsuccessful_tools_do_not_cancel_response_activity() {
    let mut activity = Activity::default();
    activity.observe(&lifecycle("existing-turn", LifecycleScope::Turn, Phase::Started));
    activity.observe(&text_message(Provider::Codex, None, Phase::Delta, "Partial reply"));
    for native_type in ["warning", "retry", "message.error"] {
      let diagnostic: AgentEvent = serde_json::from_value(json!({
        "type":"metadata", "provider":"opencode", "session_id":"session", "kind":"diagnostic",
        "native_type":native_type, "summary":"response cancelled", "native":{
          "data":{"role":"assistant", "error":{"name":"APIError", "data":{"message":"retrying"}}}
        }, "timestamp":null
      }))
      .unwrap();
      activity.observe(&diagnostic);
      assert!(activity.running);
      assert_eq!(activity.active_turn.as_deref(), Some("existing-turn"));
      assert!(activity.assistant_stream.is_some());
    }
    let tool = tokn_session_opencode::OpenCodeLiveNormalizer::normalize_line(
      &json!({
        "type":"tool_use", "sessionID":"session", "part":{"type":"tool", "tool":"bash", "callID":"call",
          "state":{"status":"error", "error":"Tool execution aborted", "metadata":{"interrupted":true}}}
      })
      .to_string(),
    )
    .unwrap();
    for event in tool {
      let tokn_session_core::LiveSessionEvent::Event(event) = event else {
        panic!("expected normalized tool")
      };
      activity.observe(&event);
      assert!(activity.running);
    }
    let cancellation = tokn_session_opencode::OpenCodeLiveNormalizer::normalize_line(
      &json!({
        "type":"error", "sessionID":"session", "error":{"name":"MessageAbortedError", "data":{"message":"Stopped"}}
      })
      .to_string(),
    )
    .unwrap();
    for event in cancellation {
      let tokn_session_core::LiveSessionEvent::Event(event) = event else {
        panic!("expected cancellation diagnostic")
      };
      activity.observe(&event);
    }
    assert!(!activity.running);
    assert!(activity.active_turn.is_none());
    assert!(activity.assistant_stream.is_none());
  }

  #[test]
  fn latest_assistant_projection_includes_commentary_but_excludes_hidden_and_other_roles() {
    let mut activity = Activity::default();
    assert_eq!(activity.presentation.latest_message.as_deref(), Some("null"));
    let mut event = message(Role::Assistant, MessageDelivery::Final, Phase::Finished);
    if let AgentEvent::Message(m) = &mut event {
      m.message_id = Some("final".into());
      m.text = "First reply".into();
    }
    activity.observe(&event);
    if let AgentEvent::Message(m) = &mut event {
      m.message_id = Some("commentary".into());
      m.delivery = MessageDelivery::Commentary;
      m.text = "\n  Checking  schema. \nSecond line".into();
    }
    activity.observe(&event);
    let expected =
      json!({"message_id":"commentary", "delivery":"commentary", "phase":"finished", "preview":"Checking schema."});
    assert_eq!(
      serde_json::from_str::<serde_json::Value>(activity.presentation.latest_message.as_deref().unwrap()).unwrap(),
      expected
    );
    for role in [Role::User, Role::System, Role::Tool] {
      activity.observe(&message(role, MessageDelivery::Unspecified, Phase::Finished));
    }
    if let AgentEvent::Message(m) = &mut event {
      m.text = "Hidden private text".into();
      m.provenance = Some(tokn_session_core::MessageProvenance {
        source: json!({}),
        display: Some(false),
        native: None,
        surface_op: None,
        source_event_seqs: None,
      });
    }
    activity.observe(&event);
    assert_eq!(
      serde_json::from_str::<serde_json::Value>(activity.presentation.latest_message.as_deref().unwrap()).unwrap(),
      expected
    );
  }

  #[test]
  fn latest_assistant_projection_bounds_text_and_preserves_empty_as_authoritative() {
    let mut activity = Activity::default();
    let mut event = message(Role::Assistant, MessageDelivery::Commentary, Phase::Delta);
    if let AgentEvent::Message(m) = &mut event {
      m.text = "😀".repeat(500);
      m.message_id = Some("x".repeat(2000));
    }
    activity.observe(&event);
    let compact: serde_json::Value =
      serde_json::from_str(activity.presentation.latest_message.as_deref().unwrap()).unwrap();
    assert_eq!(compact["preview"].as_str().unwrap().chars().count(), 240);
    assert_eq!(compact["message_id"].as_str().unwrap().chars().count(), 256);
    assert_eq!(compact.as_object().unwrap().len(), 4);
    if let AgentEvent::Message(m) = &mut event {
      m.text = " \n ".into();
      m.phase = Phase::Finished;
    }
    activity.observe(&event);
    let compact: serde_json::Value =
      serde_json::from_str(activity.presentation.latest_message.as_deref().unwrap()).unwrap();
    assert!(compact["preview"].is_null());
    assert_eq!(compact["phase"], "finished");
  }

  fn text_message(provider: Provider, id: Option<&str>, phase: Phase, text: &str) -> AgentEvent {
    let mut event = message(Role::Assistant, MessageDelivery::Unspecified, phase);
    if let AgentEvent::Message(message) = &mut event {
      message.provider = provider;
      message.message_id = id.map(str::to_owned);
      message.text = text.to_owned();
    }
    event
  }

  fn latest(activity: &Activity) -> serde_json::Value {
    serde_json::from_str(activity.presentation.latest_message.as_deref().unwrap()).unwrap()
  }

  #[test]
  fn codex_fragment_deltas_extend_the_first_line_and_finished_snapshots_replace_it() {
    let mut activity = Activity::default();
    activity.observe(&text_message(Provider::Codex, Some("one"), Phase::Started, "\n First"));
    activity.observe(&text_message(Provider::Codex, Some("one"), Phase::Delta, "  line"));
    activity.observe(&text_message(
      Provider::Codex,
      Some("one"),
      Phase::Delta,
      ".\nSecond line",
    ));
    activity.observe(&text_message(
      Provider::Codex,
      Some("one"),
      Phase::Delta,
      " should stay out",
    ));
    assert_eq!(latest(&activity)["preview"], "First line.");
    assert_eq!(latest(&activity)["phase"], "delta");
    activity.observe(&text_message(Provider::Codex, Some("two"), Phase::Delta, "New message"));
    assert_eq!(latest(&activity)["preview"], "New message");
    activity.observe(&text_message(
      Provider::Codex,
      Some("two"),
      Phase::Finished,
      "Canonical final reply\nFull details",
    ));
    assert_eq!(latest(&activity)["preview"], "Canonical final reply");
    assert!(activity.assistant_stream.is_none());
  }

  #[test]
  fn dsh_idless_deltas_accumulate_until_a_new_step_or_user_turn() {
    let mut activity = Activity::default();
    activity.observe(&lifecycle("turn", LifecycleScope::Turn, Phase::Started));
    activity.observe(&lifecycle("turn", LifecycleScope::Step, Phase::Started));
    for text in ["Still", " working", "..."] {
      activity.observe(&text_message(Provider::Dsh, None, Phase::Delta, text));
    }
    assert_eq!(latest(&activity)["preview"], "Still working...");
    activity.observe(&lifecycle("turn", LifecycleScope::Step, Phase::Started));
    activity.observe(&text_message(Provider::Dsh, None, Phase::Delta, "Next step"));
    assert_eq!(latest(&activity)["preview"], "Next step");
    activity.observe(&message(Role::User, MessageDelivery::Unspecified, Phase::Finished));
    activity.observe(&text_message(Provider::Dsh, None, Phase::Delta, "New turn"));
    assert_eq!(latest(&activity)["preview"], "New turn");
    activity.observe(&text_message(
      Provider::Dsh,
      Some("done"),
      Phase::Finished,
      "Finished snapshot",
    ));
    assert_eq!(latest(&activity)["preview"], "Finished snapshot");
    activity.observe(&text_message(Provider::Dsh, None, Phase::Delta, "Independent stream"));
    assert_eq!(latest(&activity)["preview"], "Independent stream");
  }

  #[test]
  fn mutable_snapshot_providers_replace_delta_and_updated_text_instead_of_appending() {
    for (provider, phase) in [
      (Provider::OpenCode, Phase::Delta),
      (Provider::ZCode, Phase::Delta),
      (Provider::WorkBuddy, Phase::Updated),
    ] {
      let mut activity = Activity::default();
      activity.observe(&text_message(provider, Some("same"), phase, "First"));
      activity.observe(&text_message(provider, Some("same"), phase, "First complete"));
      assert_eq!(latest(&activity)["preview"], "First complete");
      assert!(activity.assistant_stream.is_none());
    }
  }

  #[test]
  fn compact_stream_never_retains_more_than_240_unicode_characters() {
    for provider in [Provider::Codex, Provider::Dsh] {
      let mut activity = Activity::default();
      activity.observe(&text_message(provider, None, Phase::Delta, "😀"));
      activity.observe(&text_message(provider, None, Phase::Delta, &"😀".repeat(1000)));
      assert_eq!(latest(&activity)["preview"].as_str().unwrap().chars().count(), 240);
      let stream = activity.assistant_stream.as_ref().unwrap();
      assert_eq!(stream.line.text.chars().count(), 240);
      activity.observe(&text_message(provider, None, Phase::Delta, "never retained"));
      assert_eq!(
        activity.assistant_stream.as_ref().unwrap().line.text.chars().count(),
        240
      );
    }
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
    let last_good = readers.read(&locator).unwrap();
    // A valid record before a later failure must not publish a partial count or
    // preview, and retrying the repaired source must not apply it twice.
    writeln!(file, "{final_row}\n{{broken").unwrap();
    assert!(readers.read(&locator).is_err());
    assert!(!readers.contains(&locator));
    assert_eq!(last_good.final_count, 1);
    std::fs::write(&path, format!("{header}{final_row}\n{final_row}\n")).unwrap();
    assert_eq!(readers.read(&locator).unwrap().final_count, 2);
    std::fs::write(&path, &header).unwrap();
    assert_eq!(readers.read(&locator).unwrap().final_count, 0);
  }

  #[test]
  fn codex_activity_keeps_full_counts_and_questions_beyond_a_record_budget() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("session.jsonl");
    let mut file = std::fs::File::create(&path).unwrap();
    writeln!(
      file,
      "{}",
      json!({"type":"session_meta","payload":{"id":"session","cwd":"/tmp"}})
    )
    .unwrap();
    for index in 0..20 {
      writeln!(
        file,
        "{}",
        json!({"type":"response_item","payload":{
          "type":"message","id":format!("final-{index}"),"role":"assistant","phase":"final_answer",
          "content":[{"type":"output_text","text":format!("Reply {index}")}]
        }})
      )
      .unwrap();
    }
    writeln!(
      file,
      "{}",
      json!({"type":"event_msg","payload":{
        "type":"task_started","turn_id":"current"
      }})
    )
    .unwrap();
    writeln!(
      file,
      "{}",
      json!({"type":"event_msg","payload":{
        "type":"request_user_input","call_id":"request","turn_id":"current",
        "questions":[{"id":"question","header":"Choice","question":"Which option?"}]
      }})
    )
    .unwrap();
    let record_budget = 512;
    assert!(file.metadata().unwrap().len() > record_budget as u64);
    let locator = SessionLocator {
      version: 1,
      provider: ViewerProvider::Codex,
      session_id: "session".into(),
      source_path: path.clone(),
    };
    let mut readers = ActivityReaders::default();
    readers.entries.insert(
      locator.clone(),
      Entry {
        reader: Reader::Codex(Box::new(tokn_session_codex::CodexHistoryProjectionReader::new(
          path,
          record_budget,
        ))),
        activity: Activity::default(),
        version: Vec::new(),
        used: Instant::now(),
      },
    );
    let activity = readers.read(&locator).unwrap();
    assert_eq!(activity.final_count, 20);
    assert!(activity.running);
    assert_eq!(activity.question_attention.required_count, 1);
    assert_eq!(latest(&activity)["preview"], "Reply 19");

    writeln!(
      file,
      "{}",
      json!({"type":"response_item","payload":{
        "type":"message","id":"last","role":"assistant","phase":"final_answer",
        "content":[{"type":"output_text","text":"Completed"}]
      }})
    )
    .unwrap();
    let activity = readers.read(&locator).unwrap();
    assert_eq!(activity.final_count, 21);
    assert!(!activity.running);
    assert_eq!(activity.question_attention.required_count, 0);
    assert_eq!(latest(&activity)["preview"], "Completed");
  }

  #[test]
  fn codex_activity_discards_streamed_records_when_the_owner_changes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("session.jsonl");
    let header = json!({"type":"session_meta","payload":{"id":"original","cwd":"/tmp"}}).to_string();
    let reply = json!({"type":"response_item","payload":{
      "type":"message","role":"assistant","phase":"final_answer",
      "content":[{"type":"output_text","text":"done"}]
    }})
    .to_string();
    std::fs::write(&path, format!("{header}\n{reply}\n")).unwrap();
    let mut locator = SessionLocator {
      version: 1,
      provider: ViewerProvider::Codex,
      session_id: "original".into(),
      source_path: path.clone(),
    };
    let mut readers = ActivityReaders::default();
    assert_eq!(readers.read(&locator).unwrap().final_count, 1);
    std::fs::write(
      &path,
      format!("{}\n{reply}\n{reply}\n", header.replace("original", "replacement")),
    )
    .unwrap();
    assert_eq!(
      readers.read(&locator).err().as_deref(),
      Some("Session activity owner changed")
    );
    assert!(!readers.contains(&locator));
    locator.session_id = "replacement".into();
    assert_eq!(readers.read(&locator).unwrap().final_count, 2);
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
