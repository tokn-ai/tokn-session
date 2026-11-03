//! Sequenced normalization of current Gateway control and notification state.
use super::{MAX_SESSIONS, Streams, valid_id};
use crate::Inner;
use serde_json::Value;
use std::collections::BTreeMap;
use tokn_dsh_rpc_protocol::remote;
use tokn_interaction_core::*;

const MAX_PROJECTIONS: usize = 16_384;
const MAX_PROJECTION_BYTES: usize = 16 * 1024 * 1024;

struct ProjectionCut {
  seq: i64,
  value: Value,
  bytes: usize,
}

#[derive(Default)]
pub(super) struct MuxState {
  control_ready: bool,
  client_id: Option<String>,
  baselines: BTreeMap<String, i64>,
  cuts: BTreeMap<(String, String), ProjectionCut>,
  projection_bytes: usize,
}

pub(super) enum Action {
  Delegate { client_id: String, event_id: String },
  Cancel(String),
}

impl MuxState {
  pub(super) async fn ingest(
    &mut self,
    inner: &Inner,
    streams: &Streams,
    native: Value,
  ) -> Result<Option<Action>, String> {
    let frame = streams.decode(&native)?;
    let value = frame.value.unwrap();
    // Preserve the complete mux envelope, including stream correlation and any
    // provider-defined fields, independently of the normalized live view.
    let session_id = value
      .get("sessionId")
      .or_else(|| value.get("agentId"))
      .and_then(Value::as_str)
      .map(str::to_owned);
    inner.emit(session_id, RuntimeEventKind::Native(native));
    if frame.stream_id == streams.control {
      let frame: remote::ControlFrame = serde_json::from_value(value).map_err(|_| "Invalid DSH control frame")?;
      match frame {
        remote::ControlFrame::Baseline { value, .. } if !self.control_ready => {
          if value.projections.len() > MAX_SESSIONS {
            return Err("DSH control session limit exceeded".into());
          }
          self.control_ready = true;
          for (id, block) in value.projections {
            if !valid_id(&id) || block.as_of_seq < -1 {
              return Err("Invalid DSH control baseline identity or cursor".into());
            }
            self.baselines.insert(id.clone(), block.as_of_seq);
            for (key, value) in block.values {
              self.projection(inner, &id, key, value, block.as_of_seq).await?;
            }
          }
        }
        remote::ControlFrame::Projection {
          session_id,
          key,
          value,
          seq,
          ..
        } if self.control_ready && seq >= 0 => {
          self.projection(inner, &session_id, key, value, seq).await?;
        }
        _ => return Err("DSH control baseline ordering was lost".into()),
      }
      return Ok(None);
    }
    let event: remote::EventFrame = serde_json::from_value(value).map_err(|_| "Invalid DSH forwarded event")?;
    match event {
      remote::EventFrame::Ready { client_id, host, .. } if self.client_id.is_none() => {
        if !valid_id(&client_id) || host.get("home").and_then(Value::as_str).is_none() {
          return Err("Invalid DSH Remote generation".into());
        }
        self.client_id = Some(client_id);
      }
      remote::EventFrame::Ready { .. } => return Err("DSH Remote event generation changed".into()),
      _ if self.client_id.is_none() => return Err("DSH Remote event arrived before readiness".into()),
      remote::EventFrame::Emit { event, args, .. } => {
        self.notification(inner, &event, &args).await?;
      }
      remote::EventFrame::Waterfall {
        event,
        event_id,
        agent_id,
        request,
        ..
      } => {
        if !valid_id(&event)
          || !valid_id(&event_id)
          || !valid_id(&agent_id)
          || request.contains_key("agent")
          || request.contains_key("signal")
        {
          return Err("Invalid DSH Remote waterfall correlation".into());
        }
        return Ok(Some(Action::Delegate {
          client_id: self.client_id.clone().unwrap(),
          event_id,
        }));
      }
      remote::EventFrame::Cancel { event_id, .. } => {
        if !valid_id(&event_id) {
          return Err("Invalid DSH Remote cancellation identity".into());
        }
        return Ok(Some(Action::Cancel(event_id)));
      }
    }
    Ok(None)
  }

  async fn projection(&mut self, inner: &Inner, id: &str, key: String, value: Value, seq: i64) -> Result<(), String> {
    if !valid_id(id) || !valid_id(&key) {
      return Err("Invalid DSH projection identity".into());
    }
    if self.baselines.get(id).is_some_and(|floor| seq < *floor) {
      return Ok(());
    }
    let identity = (id.to_owned(), key.clone());
    let old = self.cuts.get(&identity);
    if let Some(old) = old {
      if seq < old.seq {
        return Ok(());
      }
      if seq == old.seq {
        return if value == old.value {
          Ok(())
        } else {
          Err("DSH projection replay changed an existing sequence".into())
        };
      }
    }
    let bytes = serde_json::to_vec(&value)
      .map_err(|_| "Invalid DSH projection value")?
      .len();
    let total = self
      .projection_bytes
      .saturating_sub(old.map_or(0, |cut| cut.bytes))
      .saturating_add(bytes);
    if old.is_none() && self.cuts.len() >= MAX_PROJECTIONS || total > MAX_PROJECTION_BYTES {
      return Err("DSH projection cache exceeds supported bounds".into());
    }
    let previous = old.map(|cut| cut.value.clone());
    self.cuts.insert(
      identity,
      ProjectionCut {
        seq,
        value: value.clone(),
        bytes,
      },
    );
    self.projection_bytes = total;
    let mut state = inner.state.lock().await;
    if !state.sessions.contains_key(id) && state.sessions.len() >= MAX_SESSIONS {
      return Err("DSH live session limit exceeded".into());
    }
    let observed_next = if key == "modelSelection" {
      Some(model(
        value.get("next").ok_or("DSH model projection has no next value")?,
      )?)
    } else {
      None
    };
    let current = state.sessions.entry(id.into()).or_insert_with(|| unknown_session(id));
    let mut activity = false;
    match key.as_str() {
      "modelSelection" => {
        let actual = value
          .get("lastUsed")
          .ok_or("DSH model projection has no lastUsed value")?;
        let next = value.get("next").ok_or("DSH model projection has no next value")?;
        current.model = model(if actual.is_null() { next } else { actual })?;
        activity = !actual.is_null() && previous.as_ref().and_then(|old| old.get("lastUsed")) != Some(actual);
      }
      "inbox" => {
        let turns = value["next-turn"]
          .as_array()
          .ok_or("Invalid DSH next-turn inbox projection")?;
        let steps = value["next-step"]
          .as_array()
          .ok_or("Invalid DSH next-step inbox projection")?;
        current.pending_inputs = Some((turns.len() + steps.len()) as u64);
      }
      "turnBoundary" => {
        let open = value
          .get("openTurnStartSeq")
          .ok_or("DSH turn boundary has no open-turn state")?;
        if !open.is_null() && open.as_u64().is_none() {
          return Err("Invalid DSH open-turn cursor".into());
        }
        current.status = if open.is_null() {
          ExecutionState::Idle
        } else {
          ExecutionState::Running
        };
        let last_turn = value
          .get("lastTurn")
          .and_then(Value::as_u64)
          .ok_or("DSH turn boundary has no turn counter")?;
        let nonblank = last_turn > 0 || !open.is_null();
        activity = previous.as_ref().map_or(nonblank, |old| old != &value);
      }
      _ => return Ok(()),
    }
    current.revision = Some(format!("remote:{seq}"));
    let snapshot = current.clone();
    if let Some(next) = observed_next {
      if !state.remote_model_selections.contains_key(id) && state.remote_model_selections.len() >= MAX_SESSIONS {
        return Err("DSH selected-model observation limit exceeded".into());
      }
      state.remote_model_selections.insert(id.into(), (seq, next));
    }
    if activity {
      bump_observation(&mut state.model_observations, id)?;
    }
    drop(state);
    inner.emit(Some(id.into()), RuntimeEventKind::State(snapshot));
    Ok(())
  }

  async fn notification(&self, inner: &Inner, event: &str, args: &[Value]) -> Result<(), String> {
    match event {
      "api-session/status" | "api-session/added" => {
        let (id, running) = if event == "api-session/status" {
          (
            args.first().and_then(Value::as_str),
            args.get(1).and_then(Value::as_bool),
          )
        } else {
          (
            args.first().and_then(|value| value["sessionId"].as_str()),
            args.first().and_then(|value| value["running"].as_bool()),
          )
        };
        let id = id
          .filter(|id| valid_id(id))
          .ok_or("Invalid DSH session notification identity")?;
        let running = running.ok_or("DSH session notification has no running state")?;
        let mut state = inner.state.lock().await;
        if !state.sessions.contains_key(id) && state.sessions.len() >= MAX_SESSIONS {
          return Err("DSH live session limit exceeded".into());
        }
        let current = state.sessions.entry(id.into()).or_insert_with(|| unknown_session(id));
        let next = if running {
          ExecutionState::Running
        } else {
          ExecutionState::Idle
        };
        let activity = current.status != next && (running || current.status == ExecutionState::Running);
        current.status = next;
        let snapshot = current.clone();
        if activity {
          bump_observation(&mut state.model_observations, id)?;
        }
        drop(state);
        inner.emit(Some(id.into()), RuntimeEventKind::State(snapshot));
      }
      "api-session/removed" => {
        let id = args
          .first()
          .and_then(Value::as_str)
          .filter(|id| valid_id(id))
          .ok_or("Invalid DSH removed-session identity")?;
        let mut state = inner.state.lock().await;
        state.sessions.remove(id);
        bump_observation(&mut state.model_observations, id)?;
        drop(state);
        inner.emit(Some(id.into()), RuntimeEventKind::State(unknown_session(id)));
      }
      _ => {}
    }
    Ok(())
  }
}

fn model(value: &Value) -> Result<Option<ModelIdentity>, String> {
  if value.is_null() {
    return Ok(None);
  }
  let provider = value["provider"]
    .as_str()
    .filter(|id| valid_id(id))
    .ok_or("Invalid DSH projected model provider")?;
  let id = value["model"]
    .as_str()
    .filter(|id| valid_id(id))
    .ok_or("Invalid DSH projected model identity")?;
  Ok(Some(ModelIdentity {
    model_provider: Some(provider.into()),
    model_id: id.into(),
    display_name: None,
  }))
}

fn unknown_session(id: &str) -> SessionState {
  SessionState {
    session_id: Some(id.into()),
    status: ExecutionState::Unknown,
    model: None,
    pending_inputs: None,
    revision: None,
  }
}

fn bump_observation(observations: &mut BTreeMap<String, u64>, id: &str) -> Result<(), String> {
  if !observations.contains_key(id) && observations.len() >= MAX_SESSIONS {
    return Err("DSH activity observation limit exceeded".into());
  }
  let epoch = observations.entry(id.into()).or_default();
  *epoch = epoch.wrapping_add(1);
  Ok(())
}
