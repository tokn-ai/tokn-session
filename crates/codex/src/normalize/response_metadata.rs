//! Request labels belong to one proven turn, never the session model baseline.
//! Codex does not persist a turn's service tier: its persistent settings can
//! differ from a turn override. Neither those settings nor usage imply Fast.
use super::CodexLine;
use tokn_codex_protocol::{ResponseItem, RolloutItem};
use tokn_session_core::{ModelOrigin, ResponseMetadata};

#[cfg(test)]
mod tests;

#[derive(Default)]
pub(super) struct ResponseContext {
  owner: Option<String>,
  active_turn: Option<String>,
  requested: Option<RequestedTurn>,
}

struct RequestedTurn {
  id: String,
  metadata: ResponseMetadata,
}

fn identity(value: Option<&str>) -> Option<&str> {
  value.filter(|value| {
    !value.is_empty() && value.len() <= 512 && value.trim() == *value && !value.chars().any(char::is_control)
  })
}

impl ResponseContext {
  pub(super) fn owns_session(&self, session: Option<&str>) -> bool {
    self.owner.is_some() && self.owner.as_deref() == session
  }

  pub(super) fn observe(&mut self, line: &CodexLine) -> Option<ResponseMetadata> {
    match line.item() {
      RolloutItem::SessionMeta(meta) => {
        self.owner = identity(meta.id.as_deref()).map(str::to_owned);
        self.active_turn = None;
        self.requested = None;
      }
      RolloutItem::TurnContext(context) => {
        self.requested = identity(context.turn_id.as_deref())
          .filter(|id| self.active_turn.as_deref().is_none_or(|active| active == *id))
          .and_then(|id| {
            Some(RequestedTurn {
              id: id.to_owned(),
              metadata: ResponseMetadata::new(
                ResponseMetadata::recorded_model(None, context.model.as_deref(), ModelOrigin::Request),
                None,
              )?,
            })
          });
      }
      RolloutItem::ResponseItem(ResponseItem::Message(message)) => {
        let turn = message
          .internal_chat_message_metadata_passthrough
          .as_ref()
          .and_then(|metadata| identity(metadata["turn_id"].as_str()));
        if message.role.as_deref() == Some("user") {
          if self
            .requested
            .as_ref()
            .is_some_and(|requested| Some(requested.id.as_str()) != turn)
          {
            self.requested = None;
          }
        } else if message.role.as_deref() == Some("assistant") {
          // Physical adjacency alone does not identify an assistant's turn.
          return self.for_turn(turn);
        }
      }
      RolloutItem::EventMessage(event) => {
        let payload = &event.native;
        let turn = identity(payload["turn_id"].as_str().or_else(|| payload["turnId"].as_str()));
        match event.event_type.as_deref() {
          Some("task_started" | "turn_started") => {
            self.active_turn = turn.map(str::to_owned);
            self.requested = None;
          }
          Some("task_complete" | "turn_complete" | "turn_aborted") => {
            if turn.is_none()
              || self.active_turn.as_deref() == turn
              || self
                .requested
                .as_ref()
                .is_some_and(|requested| Some(requested.id.as_str()) == turn)
            {
              self.active_turn = None;
              self.requested = None;
            }
          }
          Some("item_started" | "item_completed") if payload["item"]["type"] == "AgentMessage" => {
            if identity(payload["thread_id"].as_str()) != self.owner.as_deref() {
              return None;
            }
            return self.for_turn(turn);
          }
          Some("thread_rolled_back" | "session_configured") => {
            self.active_turn = None;
            self.requested = None;
          }
          _ => {}
        }
      }
      RolloutItem::Compacted(_) => {
        self.active_turn = None;
        self.requested = None;
      }
      RolloutItem::Unknown(item) if matches!(item.native_type.as_deref(), Some("turn_context" | "compacted")) => {
        self.requested = None;
      }
      RolloutItem::Unknown(item) if item.native_type.as_deref() == Some("session_meta") => {
        self.owner = None;
        self.active_turn = None;
        self.requested = None;
      }
      _ => {}
    }
    None
  }

  fn for_turn(&self, turn: Option<&str>) -> Option<ResponseMetadata> {
    self.owner.as_ref()?;
    let requested = self.requested.as_ref()?;
    (Some(requested.id.as_str()) == turn).then(|| requested.metadata.clone())
  }
}
