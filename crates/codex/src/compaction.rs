use tokn_codex_protocol::{RolloutItem, RolloutLine};
use tokn_session_core::{AgentEvent, CompactionEvent, CompactionState, Provider, UnknownEvent};

#[derive(Default)]
pub(crate) struct Compactions {
  sequence: u64,
  pending: Option<PendingCheckpoint>,
}

struct PendingCheckpoint {
  id: String,
  session_id: Option<String>,
  turn_id: Option<String>,
  snapshot: SnapshotStage,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SnapshotStage {
  Checkpoint,
  WorldState,
  TurnContext,
  Settings,
}

impl PendingCheckpoint {
  /// Codex persists this ordered snapshot batch as part of installing the
  /// checkpoint, before accounting and the completion notice. Other context
  /// updates are not evidence that they belong to the same compaction.
  fn snapshot_record(&mut self, line: &RolloutLine) -> bool {
    let payload = &line.native()["payload"];
    match (self.snapshot, line.item()) {
      (SnapshotStage::Checkpoint, RolloutItem::WorldState(item))
        if item.full == Some(true) && item.state.is_object() =>
      {
        self.snapshot = SnapshotStage::WorldState;
      }
      (SnapshotStage::WorldState, RolloutItem::TurnContext(item)) => {
        let Some(turn_id) = item.turn_id.as_deref().filter(|id| !id.trim().is_empty()) else {
          return false;
        };
        if self.turn_id.as_deref().is_some_and(|expected| expected != turn_id) {
          return false;
        }
        self.turn_id = Some(turn_id.to_owned());
        self.snapshot = SnapshotStage::TurnContext;
      }
      (SnapshotStage::TurnContext, RolloutItem::EventMessage(item))
        if item.event_type.as_deref() == Some("thread_settings_applied")
          // Older settings snapshots omit thread_id; a provided identity must
          // still match the checkpoint's owner, and completion checks it again.
          && payload.get("thread_id").is_none_or(|value| {
            value.is_null() || nonempty(value).is_some_and(|id| Some(id) == self.session_id.as_deref())
          })
          && payload["thread_settings"].is_object() =>
      {
        self.snapshot = SnapshotStage::Settings;
      }
      _ => return false,
    }
    true
  }
}

impl Compactions {
  pub fn clear(&mut self) {
    self.pending = None;
  }

  pub fn normalize(
    &mut self,
    line: &RolloutLine,
    session_id: Option<String>,
    canonical: bool,
  ) -> Option<Vec<AgentEvent>> {
    let raw = line.native();
    let payload = &raw["payload"];
    let mut event = CompactionEvent::new(Provider::Codex, session_id, CompactionState::Completed);
    event.timestamp = line.timestamp().map(str::to_owned);
    if let Some(ordinal) = line.ordinal() {
      event.source_refs.push(ordinal.to_string());
    }
    match line.item() {
      RolloutItem::Compacted(item) if item.message.is_some() => {
        self.sequence += 1;
        let id = item
          .window_id
          .as_ref()
          .filter(|id| !id.trim().is_empty())
          .cloned()
          .unwrap_or_else(|| format!("checkpoint:{}", self.sequence));
        let resume = payload.get("resume_metadata");
        let turn = payload.pointer("/resume_metadata/last_started_turn_id");
        self.pending = (!resume.is_some_and(|value| !value.is_null() && !value.is_object())
          && !turn.is_some_and(|value| !value.is_null() && nonempty(value).is_none()))
        .then(|| PendingCheckpoint {
          id: id.clone(),
          session_id: event.session_id.clone(),
          turn_id: turn.and_then(nonempty).map(str::to_owned),
          snapshot: SnapshotStage::Checkpoint,
        });
        event.compaction_id = Some(id);
        event.summary = item.message.clone().filter(|text| !text.trim().is_empty());
        event.summary_opaque = item.replacement_history.as_ref().is_some_and(|items| {
          items.iter().any(|item| match item {
            tokn_codex_protocol::ResponseItem::Compaction(item)
            | tokn_codex_protocol::ResponseItem::ContextCompaction(item) => item.encrypted_content.is_some(),
            _ => false,
          })
        });
        event.context.window_id = item.window_id.clone();
        event.context.previous_window_id = item.previous_window_id.clone();
        event.context.window_number = item.window_number;
      }
      RolloutItem::EventMessage(item) if item.event_type.as_deref() == Some("context_compacted") => {
        event.compaction_id = self
          .pending
          .take()
          .filter(|pending| pending.session_id == event.session_id)
          .map(|pending| pending.id);
      }
      RolloutItem::EventMessage(item)
        if canonical
          && item.event_type.as_deref() == Some("item_completed")
          && payload["item"]["type"] == "ContextCompaction" =>
      {
        let pending = self.pending.take();
        let thread_id = nonempty(&payload["thread_id"])?;
        let turn_id = nonempty(&payload["turn_id"])?;
        let id = nonempty(&payload["item"]["id"])?;
        event.source_refs.push(id.to_owned());
        event.compaction_id = Some(
          pending
            .filter(|pending| {
              pending.session_id.as_deref() == Some(thread_id)
                && pending.turn_id.as_deref().is_none_or(|expected| expected == turn_id)
            })
            .map_or_else(|| id.to_owned(), |pending| pending.id),
        );
        event.turn_id = Some(turn_id.to_owned());
        event.timestamp = event
          .timestamp
          .or_else(|| payload["completed_at_ms"].as_u64().map(|time| time.to_string()));
      }
      RolloutItem::ResponseItem(item)
        if matches!(
          item,
          tokn_codex_protocol::ResponseItem::Compaction(_) | tokn_codex_protocol::ResponseItem::ContextCompaction(_)
        ) =>
      {
        self.pending = None;
        if matches!(item, tokn_codex_protocol::ResponseItem::Compaction(control) if control.encrypted_content.is_none())
        {
          return Some(vec![AgentEvent::Unknown(UnknownEvent {
            provider: Provider::Codex,
            session_id: event.session_id,
            native_type: Some("response_item.compaction".into()),
            native: Some(raw.clone()),
            timestamp: event.timestamp,
          })]);
        }
        event.compaction_id = payload["id"].as_str().map(str::to_owned);
        event.summary_opaque = payload["encrypted_content"].is_string();
      }
      RolloutItem::EventMessage(item) if item.event_type.as_deref() == Some("token_count") => return None,
      RolloutItem::TokenUsageRecord(item) => {
        if item.usage.is_none()
          || self.pending.as_ref().is_some_and(|pending| {
            [&item.thread_id, &item.session_id].into_iter().any(|id| {
              id.as_deref()
                .is_some_and(|id| Some(id) != pending.session_id.as_deref())
            }) || item
              .turn_id
              .as_deref()
              .is_some_and(|id| pending.turn_id.as_deref().is_some_and(|expected| expected != id))
          })
        {
          self.clear();
        }
        return None;
      }
      _ => {
        if !self
          .pending
          .as_mut()
          .is_some_and(|pending| pending.snapshot_record(line))
        {
          self.clear();
        }
        return None;
      }
    }
    Some(vec![AgentEvent::Compaction(event)])
  }
}

fn nonempty(value: &serde_json::Value) -> Option<&str> {
  value.as_str().filter(|id| !id.trim().is_empty())
}
