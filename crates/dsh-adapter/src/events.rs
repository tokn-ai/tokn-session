use crate::Inner;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::{
  collections::{BTreeMap, BTreeSet, VecDeque},
  sync::{Arc, atomic::Ordering},
  time::Duration,
};
use tokio::{
  task::JoinHandle,
  time::{Instant, MissedTickBehavior},
};
use tokio_tungstenite::tungstenite::Message;
use tokn_dsh_rpc_protocol::api;
use tokn_interaction_core::*;

const MAX_SESSIONS: usize = 4096;
const MAX_PENDING: usize = 128;
const MAX_TOOL_CALLS: usize = 256;
const MAX_TOOL_ARGUMENT_BYTES: usize = 12 * 1024;

type ToolKey = (String, String);
#[derive(Clone, PartialEq)]
struct ToolPreview {
  name: String,
  // None also remembers an oversized/unrepresentable call so a reannouncement
  // cannot later substitute smaller input under the same live call identity.
  arguments: Option<String>,
}

#[derive(Default)]
pub(crate) struct ToolCallCache {
  calls: BTreeMap<ToolKey, ToolPreview>,
  order: VecDeque<ToolKey>,
}
impl ToolCallCache {
  pub(crate) fn clear(&mut self) {
    self.calls.clear();
    self.order.clear();
  }

  fn remove(&mut self, key: &ToolKey) {
    self.calls.remove(key);
    self.order.retain(|previous| previous != key);
  }

  fn clear_session(&mut self, session: &str) {
    self.calls.retain(|(owner, _), _| owner != session);
    self.order.retain(|(owner, _)| owner != session);
  }

  fn observe(&mut self, key: ToolKey, data: &Value, protected: &BTreeSet<ToolKey>) -> Result<(), String> {
    let Some(name) = data["name"].as_str().filter(|name| valid_tool_id(name)) else {
      return Ok(());
    };
    let arguments = data["arguments"]
      .as_str()
      .filter(|arguments| {
        arguments.len() <= MAX_TOOL_ARGUMENT_BYTES
          && serde_json::from_str::<Value>(arguments).is_ok_and(|value| value.is_object())
      })
      .map(str::to_owned);
    let preview = ToolPreview {
      name: name.into(),
      arguments,
    };
    if let Some(previous) = self.calls.get(&key) {
      if previous.arguments.is_some() && previous != &preview {
        return Err("DSH changed an observed live tool call".into());
      }
      return Ok(());
    }
    if self.calls.len() >= MAX_TOOL_CALLS {
      let Some(oldest) = self.order.iter().find(|key| !protected.contains(*key)).cloned() else {
        return Ok(());
      };
      self.remove(&oldest);
    }
    self.order.push_back(key.clone());
    self.calls.insert(key, preview);
    Ok(())
  }
}

fn valid_tool_id(value: &str) -> bool {
  !value.is_empty() && value.len() <= 512 && value.trim() == value && !value.chars().any(char::is_control)
}

fn approval_payload(pending: &PendingInteraction) -> &Value {
  let wire = &pending.native["wire"];
  if wire["type"] == "item" {
    &wire["value"]["request"]
  } else {
    &wire["payload"]
  }
}

fn approval_key(pending: &PendingInteraction) -> Option<ToolKey> {
  if pending.kind != InteractionKind::Permission {
    return None;
  }
  let call = approval_payload(pending)["callId"]
    .as_str()
    .filter(|call| valid_tool_id(call))?;
  Some((pending.session_id.clone()?, call.into()))
}

fn permission_display(pending: &mut PendingInteraction, preview: Option<&ToolPreview>) {
  let payload = approval_payload(pending);
  let reason = payload["reason"].as_str().unwrap_or("");
  let mut end = reason.len().min(2 * 1024);
  while !reason.is_char_boundary(end) {
    end -= 1;
  }
  let reason = &reason[..end];
  let operation = preview
    .filter(|preview| payload["toolName"] == preview.name)
    .and_then(|preview| {
      preview
        .arguments
        .as_ref()
        .map(|arguments| (preview.name.as_str(), arguments.as_str()))
    });
  let details = if let Some((name, arguments)) = operation {
    format!("Tool: {name}\nInput:\n{arguments}\n\nReason: {reason}")
  } else {
    format!(
      "The native tool input was not observed or cannot be displayed safely. Approval is disabled; deny this request or review it in DSH.\n\nReason: {reason}"
    )
  };
  let decisions = if operation.is_some() {
    serde_json::json!(["allow_once", "deny"])
  } else {
    serde_json::json!(["deny"])
  };
  pending.native["details"] = serde_json::json!(details);
  pending.native["permission_decisions"] = decisions;
}

fn observe_tools(
  state: &mut crate::StreamState,
  session: &str,
  event: &Value,
) -> Result<Vec<PendingInteraction>, String> {
  let call = match event["type"].as_str() {
    Some("tool/call") => {
      let Some(call) = event["data"]["callId"].as_str().filter(|call| valid_tool_id(call)) else {
        return Ok(Vec::new());
      };
      let key = (session.into(), call.into());
      let protected = state.pending.values().filter_map(approval_key).collect();
      state.tool_calls.observe(key.clone(), &event["data"], &protected)?;
      Some(key)
    }
    Some("tool/result") => {
      let call = event["data"]["message"]["source"]["callId"]
        .as_str()
        .or_else(|| event["data"]["callId"].as_str());
      let Some(call) = call else {
        return Ok(Vec::new());
      };
      let key = (session.into(), call.into());
      state.tool_calls.remove(&key);
      Some(key)
    }
    Some("turn/end") => {
      state.tool_calls.clear_session(session);
      None
    }
    _ => return Ok(Vec::new()),
  };
  let mut updates = Vec::new();
  for pending in state
    .pending
    .values_mut()
    .filter(|pending| pending.session_id.as_deref() == Some(session))
  {
    let Some(key) = approval_key(pending) else {
      continue;
    };
    if call.as_ref().is_some_and(|call| call != &key) {
      continue;
    }
    let before = pending.native.clone();
    permission_display(pending, state.tool_calls.calls.get(&key));
    if pending.native != before {
      updates.push(pending.clone());
    }
  }
  Ok(updates)
}

pub(crate) fn spawn(
  inner: Arc<Inner>,
  mut body: crate::transport::Downlink,
  max_frame_bytes: usize,
  heartbeat_interval: Duration,
  heartbeat_timeout: Duration,
) -> JoinHandle<()> {
  tokio::spawn(async move {
    let mut shutdown = inner.shutdown.subscribe();
    let mut heartbeat = tokio::time::interval_at(Instant::now() + heartbeat_interval, heartbeat_interval);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut pending_ping: Option<(Instant, Vec<u8>)> = None;
    let mut ping_id = 0_u64;
    let reason = loop {
      if *shutdown.borrow() || inner.disconnected.load(Ordering::Acquire) {
        return;
      }
      tokio::select! {
        changed = shutdown.changed() => {
          if changed.is_err() || *shutdown.borrow() { return; }
        }
        _ = heartbeat.tick(), if pending_ping.is_none() => {
          ping_id = ping_id.wrapping_add(1);
          let nonce = ping_id.to_be_bytes().to_vec();
          if !matches!(tokio::time::timeout(heartbeat_timeout, body.send(Message::Ping(nonce.clone().into()))).await, Ok(Ok(()))) {
            break "DSH downlink heartbeat could not be sent".into();
          }
          pending_ping = Some((Instant::now() + heartbeat_timeout, nonce));
        }
        _ = tokio::time::sleep_until(pending_ping.as_ref().map(|(deadline, _)| *deadline).unwrap_or_else(Instant::now)), if pending_ping.is_some() => {
          break "DSH downlink did not answer its heartbeat before the deadline".into();
        }
        next = body.next() => {
          match next {
            Some(Ok(Message::Text(text))) => {
              if text.len() > max_frame_bytes { break "DSH event exceeds the frame limit".into(); }
              let frame = match serde_json::from_str::<api::Request>(&text) {
                Ok(frame) if frame.native_type == "server-request" => frame,
                _ => break "DSH downlink returned an invalid server-request".into(),
              };
              if let Err(reason) = ingest(&inner, frame).await { break reason; }
            }
            Some(Ok(Message::Ping(_))) => {
              // Tungstenite queues the mandatory protocol pong while reading;
              // flushing sends no application request on the downlink.
              if !matches!(tokio::time::timeout(std::time::Duration::from_secs(5), body.flush()).await, Ok(Ok(()))) {
                break "DSH downlink ping failed".into();
              }
            }
            Some(Ok(Message::Pong(nonce))) => {
              if pending_ping.as_ref().is_some_and(|(_, expected)| nonce.as_ref() == expected.as_slice()) {
                pending_ping = None;
              }
            }
            Some(Ok(Message::Close(_))) => break "DSH event stream closed".into(),
            Some(Ok(_)) => break "DSH downlink returned a non-text frame".into(),
            Some(Err(_)) => break "DSH event transport was interrupted".into(),
            None => break "DSH event stream closed".into(),
          }
        }
      }
    };
    if !inner.closed.load(Ordering::Acquire) && !inner.disconnected.swap(true, Ordering::AcqRel) {
      // Both downlinks belong to one attachment. Once either loses continuity,
      // stop its peer so stale state can no longer arrive from a partial stream.
      let _ = inner.shutdown.send(true);
      inner.emit(None, RuntimeEventKind::Disconnected { reason });
    }
  })
}

async fn ingest(inner: &Inner, frame: api::Request) -> Result<(), String> {
  let native = serde_json::to_value(&frame).map_err(|error| error.to_string())?;
  ingest_native(inner, frame, native).await
}

pub(crate) async fn ingest_native(inner: &Inner, frame: api::Request, native: Value) -> Result<(), String> {
  if frame.rpc_id.is_empty() || frame.rpc_id.len() > 512 || frame.rpc_id.chars().any(char::is_control) {
    return Err("invalid DSH event correlation identity".into());
  }
  let payload = &frame.payload;
  if payload.get("type").and_then(Value::as_str) != Some(frame.method.as_str()) {
    return Err("DSH event method does not match its payload type".into());
  }
  let session_id = payload.get("sessionId").and_then(Value::as_str).map(str::to_string);
  if session_id
    .as_ref()
    .is_some_and(|id| id.is_empty() || id.len() > 512 || id.chars().any(char::is_control))
  {
    return Err("invalid DSH event session identity".into());
  }
  // Keep the complete frame, including provider-defined event fields and the
  // stable native rpcId, independently of normalized state/interaction views.
  inner.emit(session_id.clone(), RuntimeEventKind::Native(native.clone()));
  match frame.method.as_str() {
    "question/requested" | "approval/requested" => {
      let session_id = session_id.ok_or("DSH interaction has no session identity")?;
      let mut interaction = if frame.method == "question/requested" {
        let questions: Vec<api::Question> =
          serde_json::from_value(payload.get("questions").cloned().ok_or("DSH question has no items")?)
            .map_err(|_| "invalid DSH question items")?;
        if questions.is_empty() || questions.len() > 32 {
          return Err("invalid DSH question count".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut normalized = Vec::new();
        let mut details = Vec::new();
        for question in questions {
          if question.id.is_empty() || !ids.insert(question.id.clone()) || question.options.len() > 128 {
            return Err("invalid DSH question identity or options".into());
          }
          if let Some(detail) = question
            .extra
            .get("detail")
            .and_then(Value::as_str)
            .filter(|detail| !detail.trim().is_empty())
          {
            details.push(format!("{}\n{}", question.question, detail));
          }
          let allows_free_text = question
            .extra
            .get("intent")
            .and_then(|intent| intent.get("kind"))
            .and_then(Value::as_str)
            != Some("approval");
          normalized.push(QuestionItem {
            question_id: question.id,
            prompt: question.question,
            options: question
              .options
              .into_iter()
              .map(|option| QuestionOption {
                label: option.label,
                description: option.description,
              })
              .collect(),
            allows_free_text,
            allows_option_notes: allows_free_text && question.multi_select,
            is_secret: false,
            multiple: question.multi_select,
          });
        }
        PendingInteraction {
          interaction_id: frame.rpc_id.clone(),
          session_id: Some(session_id.clone()),
          kind: InteractionKind::Question,
          title: "Question".into(),
          questions: normalized,
          native: serde_json::json!({"wire":native,"details":(!details.is_empty()).then(|| details.join("\n\n"))}),
        }
      } else {
        if payload.get("approvalId").and_then(Value::as_str).is_none() {
          return Err("DSH approval has no audit identity".into());
        }
        PendingInteraction {
          interaction_id: frame.rpc_id.clone(),
          session_id: Some(session_id.clone()),
          kind: InteractionKind::Permission,
          title: payload
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or("Permission requested")
            .into(),
          questions: Vec::new(),
          native: serde_json::json!({"wire":native}),
        }
      };
      let mut state = inner.state.lock().await;
      if interaction.kind == InteractionKind::Permission {
        let preview = approval_key(&interaction).and_then(|key| state.tool_calls.calls.get(&key));
        permission_display(&mut interaction, preview);
      }
      if let Some(existing) = state.pending.get(&frame.rpc_id) {
        // Display metadata can gain a late operation preview or reserve an
        // uncertain response. Only the immutable native request proves replay.
        if existing.native["wire"] != interaction.native["wire"] {
          return Err("DSH replay changed a pending request identity".into());
        }
        return Ok(());
      }
      if state.pending.len() >= MAX_PENDING {
        return Err("DSH pending-interaction limit exceeded; refresh attachment".into());
      }
      state.pending.insert(frame.rpc_id, interaction.clone());
      drop(state);
      inner.emit(Some(session_id), RuntimeEventKind::InteractionRequested(interaction));
    }
    "question/resolved" | "approval/resolved" => {
      let session_id = session_id.ok_or("DSH resolved interaction has no session identity")?;
      let mut state = inner.state.lock().await;
      let resolved: Vec<String> = state
        .pending
        .iter()
        .filter_map(|(id, interaction)| {
          if interaction.session_id.as_deref() != Some(session_id.as_str()) {
            return None;
          }
          let matches = if frame.method == "question/resolved" {
            payload.get("questionRpcId").and_then(Value::as_str) == Some(id.as_str())
          } else {
            interaction.kind == InteractionKind::Permission
              && interaction.native.get("wire").unwrap_or(&interaction.native)["payload"]["approvalId"]
                == payload["approvalId"]
          };
          matches.then(|| id.clone())
        })
        .collect();
      for id in &resolved {
        state.pending.remove(id);
        state.responding.remove(id);
      }
      drop(state);
      for interaction_id in resolved {
        inner.emit(
          Some(session_id.clone()),
          RuntimeEventKind::InteractionResolved { interaction_id },
        );
      }
    }
    "host/session-status" | "session/subscribed" | "session/queue" | "session/event" => {
      let session_id = session_id.ok_or("DSH session event has no session identity")?;
      let mut state = inner.state.lock().await;
      if !state.sessions.contains_key(&session_id) && state.sessions.len() >= MAX_SESSIONS {
        return Err("DSH live session limit exceeded; refresh attachment".into());
      }
      let updates = if frame.method == "session/event" {
        observe_tools(&mut state, &session_id, &payload["event"])?
      } else {
        Vec::new()
      };
      let current = state
        .sessions
        .entry(session_id.clone())
        .or_insert_with(|| SessionState {
          session_id: Some(session_id.clone()),
          status: ExecutionState::Unknown,
          model: None,
          pending_inputs: None,
          revision: None,
        });
      let changed = match frame.method.as_str() {
        "host/session-status" => {
          let running = payload
            .get("running")
            .and_then(Value::as_bool)
            .ok_or("DSH status has no running state")?;
          current.status = if running {
            ExecutionState::Running
          } else {
            ExecutionState::Idle
          };
          true
        }
        "session/queue" => {
          current.pending_inputs = Some(
            payload
              .get("items")
              .and_then(Value::as_array)
              .ok_or("invalid DSH inbox snapshot")?
              .len() as u64,
          );
          true
        }
        "session/event" => {
          let event = &payload["event"];
          let config = match event.get("type").and_then(Value::as_str) {
            Some("request/header") => Some(&event["data"]["header"]["config"]),
            Some("request/context") => Some(&event["data"]),
            _ => None,
          };
          if let Some(config) = config {
            if let (Some(provider), Some(model)) = (config["provider"].as_str(), config["model"].as_str()) {
              current.model = Some(ModelIdentity {
                model_provider: Some(provider.into()),
                model_id: model.into(),
                display_name: None,
              });
            }
          }
          config.is_some()
        }
        _ => false,
      };
      let snapshot = current.clone();
      drop(state);
      if changed {
        inner.emit(Some(session_id.clone()), RuntimeEventKind::State(snapshot));
      }
      for pending in updates {
        inner.emit(
          Some(session_id.clone()),
          RuntimeEventKind::InteractionRequested(pending),
        );
      }
    }
    "host/session-removed" => {
      if let Some(session_id) = session_id {
        let mut state = inner.state.lock().await;
        state.sessions.remove(&session_id);
        state.tool_calls.clear_session(&session_id);
        let removed: Vec<_> = state
          .pending
          .iter()
          .filter(|(_, pending)| pending.session_id.as_deref() == Some(session_id.as_str()))
          .map(|(id, _)| id.clone())
          .collect();
        for id in &removed {
          state.pending.remove(id);
          state.responding.remove(id);
        }
        drop(state);
        for interaction_id in removed {
          inner.emit(
            Some(session_id.clone()),
            RuntimeEventKind::InteractionResolved { interaction_id },
          );
        }
      }
    }
    "stream/error" => return Err("DSH reported an event-stream error; attach again to recover".into()),
    _ => {}
  }
  Ok(())
}

#[cfg(test)]
pub(crate) async fn test_ingest(inner: &Inner, native: Value) -> Result<(), String> {
  ingest(inner, serde_json::from_value(native).unwrap()).await
}
