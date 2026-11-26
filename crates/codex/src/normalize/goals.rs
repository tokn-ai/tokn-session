//! Codex records a user goal before the work turn and may persist its final
//! status only in the response of the native update_goal tool.
#[cfg(test)]
mod tests;

use super::{parse_json_string_or_value, string_field_any};
use crate::event::CodexLine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use tokn_codex_protocol::{MessageItem, ResponseItem, RolloutItem};
use tokn_session_core::{
  AgentEvent, GoalUpdated, MessageDelivery, MessageEvent, MessageProvenance, Phase, Provider, Role,
};

const MAX_TURN_CALL_IDS: usize = 4096;

#[derive(PartialEq)]
struct GoalIdentity {
  objective: [u8; 32],
  created_at: i64,
}

impl GoalIdentity {
  fn read(goal: &Value, session: &str) -> Option<Self> {
    let objective = goal["objective"].as_str().filter(|s| !s.trim().is_empty())?;
    (goal["threadId"].as_str() == Some(session)).then_some(Self {
      objective: Sha256::digest(objective.as_bytes()).into(),
      created_at: goal["createdAt"].as_i64()?,
    })
  }
}

#[derive(Clone, Copy)]
enum GoalCall {
  Direct,
  CodeMode,
}

#[derive(Default)]
pub(super) struct Goals {
  // A lazy window may start after the active goal was established.
  saw_goal: bool,
  active: Option<GoalIdentity>,
  turn: Option<String>,
  // Keep consumed/ambiguous IDs as tombstones until the turn closes. A
  // duplicate cannot claim a delayed response from an unrelated invocation.
  calls: HashMap<[u8; 32], Option<GoalCall>>,
  exhausted: bool,
  pub(super) missing_origin: bool,
}

impl Goals {
  pub(super) fn observe(&mut self, line: &CodexLine, session: Option<&str>) -> Option<AgentEvent> {
    let session = session?;
    let payload = &line.native()["payload"];
    if let RolloutItem::EventMessage(event) = line.item() {
      match event.event_type.as_deref()? {
        "thread_goal_updated" => {
          if payload["threadId"].as_str().or(payload["thread_id"].as_str()) != Some(session) {
            return None;
          }
          self.saw_goal = true;
          let active = if payload["goal"]["status"] == "active" {
            GoalIdentity::read(&payload["goal"], session)
          } else {
            None
          };
          if self.active != active {
            self.active = active;
            // A replaced, paused or cleared goal invalidates in-flight mutations.
            for call in self.calls.values_mut() {
              *call = None;
            }
          }
        }
        "task_started" | "turn_started" => {
          let turn = string_field_any(payload, &["turn_id", "turnId"]);
          if self.turn != turn {
            self.turn = turn;
            self.calls.clear();
            self.exhausted = false;
          }
        }
        "turn_aborted" | "task_complete" | "turn_complete"
          if self.turn.as_deref() == payload["turn_id"].as_str().or(payload["turnId"].as_str()) =>
        {
          self.turn = None;
          self.calls.clear();
        }
        _ => {}
      }
      return None;
    }
    let RolloutItem::ResponseItem(item) = line.item() else {
      return None;
    };
    if let ResponseItem::Message(message) = item {
      return user_goal(message, session, line.timestamp());
    }
    let turn = self.turn.as_deref()?;
    if self.exhausted || payload["internal_chat_message_metadata_passthrough"]["turn_id"].as_str() != Some(turn) {
      return None;
    }
    let id = payload["call_id"].as_str().filter(|s| !s.is_empty())?;
    let id: [u8; 32] = Sha256::digest(id.as_bytes()).into();
    match item {
      ResponseItem::CustomToolCall(_) | ResponseItem::FunctionCall(_) => {
        if self.calls.len() >= MAX_TURN_CALL_IDS {
          self.calls.clear();
          self.exhausted = true;
          return None;
        }
        match self.calls.entry(id) {
          Entry::Occupied(mut entry) => {
            entry.insert(None);
          }
          Entry::Vacant(entry) => {
            entry.insert(goal_call(item));
          }
        }
        None
      }
      ResponseItem::CustomToolCallOutput(_) | ResponseItem::FunctionCallOutput(_) => {
        let call = self.calls.get_mut(&id)?.take()?;
        let receipt = goal_receipt(call, item)?;
        let goal = receipt.get("goal")?;
        if goal["status"] != "complete" {
          return None;
        }
        let identity = GoalIdentity::read(goal, session)?;
        if !self.saw_goal {
          self.missing_origin = true;
          return None;
        }
        if self.active.as_ref() != Some(&identity) {
          return None;
        }
        self.active = None;
        Some(AgentEvent::GoalUpdated(GoalUpdated {
          provider: Provider::Codex,
          session_id: Some(session.into()),
          turn_id: Some(turn.into()),
          goal: Some(goal.clone()),
          timestamp: line.timestamp().map(str::to_owned),
        }))
      }
      _ => None,
    }
  }
}

fn goal_call(item: &ResponseItem) -> Option<GoalCall> {
  match item {
    ResponseItem::CustomToolCall(call)
      if call.name.as_deref() == Some("exec")
        && matches!(call.namespace.as_deref(), None | Some("functions"))
        && matches!(call.status.as_deref(), None | Some("completed"))
        && super::code_mode::completes_goal(&call.input) =>
    {
      Some(GoalCall::CodeMode)
    }
    ResponseItem::FunctionCall(call)
      if matches!(call.name.as_deref(), Some("update_goal" | "functions.update_goal"))
        && matches!(call.namespace.as_deref(), None | Some("functions"))
        && parse_json_string_or_value(call.arguments.clone()) == json!({"status":"complete"}) =>
    {
      Some(GoalCall::Direct)
    }
    _ => None,
  }
}

fn goal_receipt(call: GoalCall, item: &ResponseItem) -> Option<Value> {
  match (call, item) {
    (GoalCall::CodeMode, ResponseItem::CustomToolCallOutput(out))
      if matches!(out.name.as_deref(), None | Some("exec")) =>
    {
      let [header, result] = out.output.as_array()?.as_slice() else {
        return None;
      };
      if header["type"] != "input_text" || result["type"] != "input_text" {
        return None;
      }
      let header = header["text"].as_str()?;
      if !header.starts_with("Script completed\n") || !header.trim_end().ends_with("\nOutput:") {
        return None;
      }
      serde_json::from_str(result["text"].as_str()?).ok()
    }
    (GoalCall::Direct, ResponseItem::FunctionCallOutput(out)) => match &out.output {
      Value::Object(_) => Some(out.output.clone()),
      Value::String(text) => serde_json::from_str(text).ok(),
      Value::Array(parts) => {
        let [part] = parts.as_slice() else { return None };
        if part["type"] != "input_text" {
          return None;
        }
        serde_json::from_str(part["text"].as_str()?).ok()
      }
      _ => None,
    },
    _ => None,
  }
}

fn user_goal(item: &MessageItem, session: &str, timestamp: Option<&str>) -> Option<AgentEvent> {
  if item.role.as_deref() != Some("user") {
    return None;
  }
  let metadata = item.internal_chat_message_metadata_passthrough.as_ref()?;
  let [kind] = metadata["content_item_kinds"].as_array()?.as_slice() else {
    return None;
  };
  let [origin] = metadata["content_item_metadata"].as_array()?.as_slice() else {
    return None;
  };
  let [part] = item.content.as_slice() else { return None };
  if kind != MessageEvent::CODEX_USER_GOAL
    || origin["provenance"]["type"] != "harness"
    || origin["provenance"]["hook"] != false
    || part.content_type.as_deref() != Some("input_text")
  {
    return None;
  }
  let text = part
    .text
    .as_deref()?
    .strip_prefix("<codex_internal_context source=\"user_goal\">\nUser set the goal: \"")?
    .strip_suffix("\"\nUser set goal status: \"active\".\n</codex_internal_context>")?;
  if text.trim().is_empty() {
    return None;
  }
  Some(AgentEvent::Message(MessageEvent {
    provider: Provider::Codex,
    session_id: Some(session.into()),
    message_id: item.id.clone(),
    parent_id: None,
    role: Role::User,
    delivery: MessageDelivery::Unspecified,
    phase: Phase::Finished,
    text: text.into(),
    images: Vec::new(),
    response_metadata: None,
    timestamp: timestamp.map(str::to_owned),
    provenance: Some(MessageProvenance {
      source: Value::from(MessageEvent::CODEX_USER_GOAL),
      display: None,
      native: None,
      surface_op: None,
      source_event_seqs: None,
    }),
  }))
}
