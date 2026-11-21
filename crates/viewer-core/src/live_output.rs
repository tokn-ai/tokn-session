//! Bounded session previews during a managed prompt's observation window.
//! Native activity can include background work; it never proves input admission
//! or supplies identities for persisted transcript records.
use serde::Serialize;
use std::collections::BTreeMap;
use tokn_interaction_core::{ExecutionState, RuntimeEvent, RuntimeEventKind, SessionTarget};

const MAX_SESSIONS: usize = 32;
const MAX_BYTES: usize = 64 * 1024;
const MAX_PARTS: usize = 64;

#[derive(Clone, Debug, Serialize)]
pub struct SessionLiveOutput {
  /// Host request identity for this observation window, not native causality.
  pub prompt_id: String,
  pub parts: Vec<SessionLiveOutputPart>,
  pub truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionLiveOutputPart {
  pub kind: SessionLiveOutputKind,
  pub text: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub status: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLiveOutputKind {
  Text,
  Reasoning,
  Tool,
}

type Scope = (String, String, String);

struct Entry {
  output: SessionLiveOutput,
  tools: BTreeMap<String, usize>,
  bytes: usize,
}

#[derive(Default)]
pub(crate) struct LiveOutputProjection {
  entries: BTreeMap<Scope, Entry>,
}

impl LiveOutputProjection {
  pub fn clear(&mut self) {
    self.entries.clear();
  }

  pub fn get(&self, target: &SessionTarget) -> Option<SessionLiveOutput> {
    let key = (
      target.runtime_id.clone(),
      target.instance_id.clone(),
      target.session_id.clone()?,
    );
    self
      .entries
      .get(&key)
      .filter(|entry| !entry.output.parts.is_empty())
      .map(|entry| entry.output.clone())
  }

  pub fn apply(&mut self, event: &RuntimeEvent) -> bool {
    if matches!(event.kind, RuntimeEventKind::Disconnected { .. }) {
      let before = self.entries.len();
      self
        .entries
        .retain(|(runtime, instance, _), _| runtime != &event.runtime_id || instance != &event.instance_id);
      return before != self.entries.len();
    }
    let Some(session) = &event.session_id else { return false };
    let key = (event.runtime_id.clone(), event.instance_id.clone(), session.clone());
    if let RuntimeEventKind::State(state) = &event.kind {
      if !matches!(state.status, ExecutionState::Running | ExecutionState::Stopping) {
        return self.entries.remove(&key).is_some();
      }
      if !self.entries.contains_key(&key) {
        if self.entries.len() >= MAX_SESSIONS {
          self.entries.pop_first();
        }
        self.entries.insert(
          key,
          Entry {
            output: SessionLiveOutput {
              prompt_id: String::new(),
              parts: Vec::new(),
              truncated: false,
            },
            tools: BTreeMap::new(),
            bytes: 0,
          },
        );
      }
      return false;
    }
    let RuntimeEventKind::Native(native) = &event.kind else {
      return false;
    };
    if native["protocol"] != "acp" {
      return false;
    }
    let Some(prompt_id) = native["prompt_id"]
      .as_str()
      .filter(|id| !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control))
    else {
      return false;
    };
    let Some(entry) = self.entries.get_mut(&key) else {
      return false;
    };
    if entry.output.prompt_id != prompt_id {
      entry.output = SessionLiveOutput {
        prompt_id: prompt_id.into(),
        parts: Vec::new(),
        truncated: false,
      };
      entry.tools.clear();
      entry.bytes = 0;
    }
    let update = &native["update"];
    match update["sessionUpdate"].as_str() {
      Some("agent_message_chunk" | "agent_thought_chunk") if update["content"]["type"] == "text" => {
        let Some(text) = update["content"]["text"].as_str() else {
          return false;
        };
        let kind = if update["sessionUpdate"] == "agent_thought_chunk" {
          SessionLiveOutputKind::Reasoning
        } else {
          SessionLiveOutputKind::Text
        };
        if entry.output.parts.last().is_none_or(|part| part.kind != kind) {
          if entry.output.parts.len() >= MAX_PARTS {
            entry.output.truncated = true;
            return true;
          }
          entry.output.parts.push(SessionLiveOutputPart {
            kind,
            text: String::new(),
            status: None,
          });
        }
        let text = bounded_text(text, MAX_BYTES.saturating_sub(entry.bytes));
        entry.output.truncated |= text.len() < update["content"]["text"].as_str().unwrap().len();
        entry.bytes += text.len();
        entry.output.parts.last_mut().unwrap().text.push_str(&text);
        true
      }
      Some("tool_call" | "tool_call_update") => {
        let Some(id) = update["toolCallId"]
          .as_str()
          .filter(|id| !id.is_empty() && id.len() <= 512)
        else {
          return false;
        };
        let index = if let Some(index) = entry.tools.get(id) {
          *index
        } else {
          if entry.output.parts.len() >= MAX_PARTS {
            entry.output.truncated = true;
            return true;
          }
          let index = entry.output.parts.len();
          entry.tools.insert(id.into(), index);
          entry.output.parts.push(SessionLiveOutputPart {
            kind: SessionLiveOutputKind::Tool,
            text: "Tool call".into(),
            status: None,
          });
          index
        };
        let part = &mut entry.output.parts[index];
        if let Some(title) = update["title"].as_str() {
          part.text = bounded_text(title, 256);
        }
        if let Some(status) = update["status"]
          .as_str()
          .filter(|status| matches!(*status, "pending" | "in_progress" | "completed" | "failed"))
        {
          part.status = Some(status.into());
        }
        true
      }
      _ => false,
    }
  }
}

fn bounded_text(value: &str, budget: usize) -> String {
  let mut result = String::new();
  for character in value
    .chars()
    .filter(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
  {
    if result.len() + character.len_utf8() > budget {
      break;
    }
    result.push(character);
  }
  result
}

#[cfg(test)]
mod tests;
