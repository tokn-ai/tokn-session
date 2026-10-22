use crate::Inner;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::{
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
  if frame.rpc_id.is_empty() || frame.rpc_id.len() > 512 || frame.rpc_id.chars().any(char::is_control) {
    return Err("invalid DSH event correlation identity".into());
  }
  let native = serde_json::to_value(&frame).map_err(|error| error.to_string())?;
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
      let interaction = if frame.method == "question/requested" {
        let questions: Vec<api::Question> =
          serde_json::from_value(payload.get("questions").cloned().ok_or("DSH question has no items")?)
            .map_err(|_| "invalid DSH question items")?;
        if questions.is_empty() || questions.len() > 32 {
          return Err("invalid DSH question count".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut normalized = Vec::new();
        for question in questions {
          if question.id.is_empty() || !ids.insert(question.id.clone()) || question.options.len() > 128 {
            return Err("invalid DSH question identity or options".into());
          }
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
            allows_free_text: true,
            multiple: question.multi_select,
          });
        }
        PendingInteraction {
          interaction_id: frame.rpc_id.clone(),
          session_id: Some(session_id.clone()),
          kind: InteractionKind::Question,
          title: "Question".into(),
          questions: normalized,
          native,
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
          native,
        }
      };
      let mut state = inner.state.lock().await;
      if let Some(existing) = state.pending.get(&frame.rpc_id) {
        if existing.native != interaction.native {
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
              && interaction.native["payload"]["approvalId"] == payload["approvalId"]
          };
          matches.then(|| id.clone())
        })
        .collect();
      for id in &resolved {
        state.pending.remove(id);
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
        inner.emit(Some(session_id), RuntimeEventKind::State(snapshot));
      }
    }
    "host/session-removed" => {
      if let Some(session_id) = session_id {
        let mut state = inner.state.lock().await;
        state.sessions.remove(&session_id);
        let removed: Vec<_> = state
          .pending
          .iter()
          .filter(|(_, pending)| pending.session_id.as_deref() == Some(session_id.as_str()))
          .map(|(id, _)| id.clone())
          .collect();
        for id in &removed {
          state.pending.remove(id);
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
