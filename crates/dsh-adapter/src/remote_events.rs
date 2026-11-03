//! Current Gateway control/events multiplexing. Unsupported waterfalls delegate
//! with native `next`; they are never presented as answerable interactions.
mod state;

use crate::{Inner, transport::Downlink};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use state::{Action, MuxState};
use std::{
  collections::{BTreeMap, BTreeSet, VecDeque},
  sync::{Arc, atomic::Ordering},
  time::Duration,
};
use tokio::{
  task::{AbortHandle, JoinHandle, JoinSet},
  time::{Instant, MissedTickBehavior},
};
use tokio_tungstenite::tungstenite::Message;
use tokn_dsh_rpc_protocol::remote;
use tokn_interaction_core::*;

const MAX_OPENING_FRAMES: usize = 256;
const MAX_OPENING_BYTES: usize = 8 * 1024 * 1024;
const MAX_SESSIONS: usize = 4096;
const MAX_DELEGATIONS: usize = 128;
const MAX_RETIRED: usize = 256;

pub(crate) struct Prepared {
  body: Downlink,
  streams: Streams,
  opening: Vec<Value>,
  max_frame_bytes: usize,
}

struct Streams {
  control: String,
  events: String,
}

impl Streams {
  fn decode(&self, native: &Value) -> Result<remote::StreamFrame, String> {
    let frame: remote::StreamFrame =
      serde_json::from_value(native.clone()).map_err(|_| "Invalid DSH Remote mux frame")?;
    if frame.stream_id != self.control && frame.stream_id != self.events {
      return Err("DSH Remote frame belongs to another logical stream".into());
    }
    if frame.native_type != "item" || frame.value.is_none() || frame.error.is_some() {
      return Err("DSH Remote logical stream ended or failed; reconnect to refresh its baseline".into());
    }
    Ok(frame)
  }
}

fn valid_id(id: &str) -> bool {
  !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control)
}

pub(crate) async fn prepare(
  mut body: Downlink,
  max_frame_bytes: usize,
  timeout: Duration,
) -> Result<Prepared, InteractionError> {
  tokio::time::timeout(timeout, async move {
    let streams = Streams {
      control: uuid::Uuid::new_v4().to_string(),
      events: uuid::Uuid::new_v4().to_string(),
    };
    for (id, endpoint) in [(&streams.control, "session/control"), (&streams.events, "$events")] {
      let open = serde_json::to_string(&remote::OpenStream::new(id, endpoint)).expect("static Remote stream payload");
      body
        .send(Message::Text(open.into()))
        .await
        .map_err(|_| "Could not open DSH Remote logical streams")?;
    }
    let mut opening = Vec::new();
    let mut bytes = 0_usize;
    let mut control_ready = false;
    let mut events_ready = false;
    while !control_ready || !events_ready {
      let native = match body.next().await {
        Some(Ok(Message::Text(text))) if text.len() <= max_frame_bytes => {
          bytes = bytes.saturating_add(text.len());
          if bytes > MAX_OPENING_BYTES || opening.len() >= MAX_OPENING_FRAMES {
            return Err("DSH Remote readiness exceeds supported bounds");
          }
          serde_json::from_str::<Value>(&text).map_err(|_| "Invalid DSH Remote readiness JSON")?
        }
        Some(Ok(Message::Ping(_))) => {
          body
            .flush()
            .await
            .map_err(|_| "DSH Remote readiness heartbeat failed")?;
          continue;
        }
        Some(Ok(Message::Pong(_))) => continue,
        _ => return Err("DSH Remote stream closed before its baseline was ready"),
      };
      let frame = streams
        .decode(&native)
        .map_err(|_| "Invalid DSH Remote readiness frame")?;
      let value = frame.value.unwrap();
      if frame.stream_id == streams.control {
        let control: remote::ControlFrame =
          serde_json::from_value(value).map_err(|_| "Invalid DSH control readiness")?;
        match control {
          remote::ControlFrame::Baseline { value, .. } if !control_ready => {
            if value.projections.len() > MAX_SESSIONS || value.projections.values().any(|block| block.as_of_seq < -1) {
              return Err("DSH control baseline exceeds supported bounds");
            }
            control_ready = true;
          }
          remote::ControlFrame::Projection { .. } if control_ready => {}
          _ => return Err("DSH control stream did not begin with one baseline"),
        }
      } else {
        let event: remote::EventFrame = serde_json::from_value(value).map_err(|_| "Invalid DSH event readiness")?;
        match event {
          remote::EventFrame::Ready { client_id, host, .. } if !events_ready => {
            if !valid_id(&client_id) || host.get("home").and_then(Value::as_str).is_none() {
              return Err("Invalid DSH Remote event generation");
            }
            events_ready = true;
          }
          remote::EventFrame::Ready { .. } => return Err("DSH event generation changed within one attachment"),
          _ if events_ready => {}
          _ => return Err("DSH event stream did not begin with a ready generation"),
        }
      }
      opening.push(native);
    }
    Ok(Prepared {
      body,
      streams,
      opening,
      max_frame_bytes,
    })
  })
  .await
  .map_err(|_| InteractionError::unavailable("DSH Remote event readiness timed out"))?
  .map_err(InteractionError::unavailable)
}

type Delegations = JoinSet<(String, Result<(), String>)>;

fn retire(id: String, retired: &mut BTreeSet<String>, order: &mut VecDeque<String>) {
  if retired.insert(id.clone()) {
    order.push_back(id);
  }
  while order.len() > MAX_RETIRED {
    retired.remove(&order.pop_front().unwrap());
  }
}

fn dispatch(
  inner: &Arc<Inner>,
  action: Option<Action>,
  tasks: &mut Delegations,
  pending: &mut BTreeMap<String, AbortHandle>,
  retired: &mut BTreeSet<String>,
  order: &mut VecDeque<String>,
) -> Result<(), String> {
  match action {
    Some(Action::Delegate { client_id, event_id }) => {
      if pending.contains_key(&event_id) || retired.contains(&event_id) {
        return Err("DSH reused a Remote waterfall identity".into());
      }
      if pending.len() >= MAX_DELEGATIONS || tasks.len() >= MAX_DELEGATIONS {
        return Err("DSH Remote waterfall limit exceeded".into());
      }
      let inner = inner.clone();
      let id = event_id.clone();
      let pending_id = event_id.clone();
      let handle = tasks.spawn(async move {
        // This is delegation to the native answerer chain, not an accepted user
        // answer. Current-generation callback shapes are deliberately read-only.
        let result = inner
          .transport
          .call::<Value>(
            &uuid::Uuid::new_v4().to_string(),
            "$events/result",
            json!({"clientId":client_id,"eventId":event_id,"outcome":{"kind":"next"}}),
            false,
          )
          .await
          .map(|_| ())
          .map_err(|_| "DSH Remote event delegation was not confirmed".to_owned());
        (id, result)
      });
      pending.insert(pending_id, handle);
    }
    Some(Action::Cancel(id)) => {
      if let Some(task) = pending.remove(&id) {
        task.abort();
      }
      retire(id, retired, order);
    }
    None => {}
  }
  Ok(())
}

pub(crate) fn spawn(
  inner: Arc<Inner>,
  prepared: Prepared,
  heartbeat_interval: Duration,
  heartbeat_timeout: Duration,
) -> JoinHandle<()> {
  tokio::spawn(async move {
    let Prepared {
      mut body,
      streams,
      opening,
      max_frame_bytes,
    } = prepared;
    let mut mux = MuxState::default();
    let mut tasks = JoinSet::new();
    let mut pending = BTreeMap::new();
    let mut retired = BTreeSet::new();
    let mut order = VecDeque::new();
    let mut shutdown = inner.shutdown.subscribe();
    let mut heartbeat = tokio::time::interval_at(Instant::now() + heartbeat_interval, heartbeat_interval);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut ping: Option<(Instant, Vec<u8>)> = None;
    let mut ping_id = 0_u64;
    let result: Result<(), String> = async {
      for native in opening {
        let action = mux.ingest(&inner, &streams, native).await?;
        dispatch(&inner, action, &mut tasks, &mut pending, &mut retired, &mut order)?;
      }
      loop {
        if *shutdown.borrow() || inner.disconnected.load(Ordering::Acquire) { return Ok(()); }
        tokio::select! {
          changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return Ok(()); } }
          completed = tasks.join_next(), if !tasks.is_empty() => {
            match completed {
              Some(Ok((id, result))) => {
                pending.remove(&id);
                retire(id, &mut retired, &mut order);
                result?;
              }
              Some(Err(error)) if !error.is_cancelled() => return Err("DSH Remote event delegation was interrupted".into()),
              _ => {},
            }
          }
          _ = heartbeat.tick(), if ping.is_none() => {
            ping_id = ping_id.wrapping_add(1);
            let nonce = ping_id.to_be_bytes().to_vec();
            if !matches!(tokio::time::timeout(heartbeat_timeout, body.send(Message::Ping(nonce.clone().into()))).await, Ok(Ok(()))) {
              return Err("DSH Remote heartbeat could not be sent".into());
            }
            ping = Some((Instant::now() + heartbeat_timeout, nonce));
          }
          _ = tokio::time::sleep_until(ping.as_ref().map(|(deadline, _)| *deadline).unwrap_or_else(Instant::now)), if ping.is_some() => {
            return Err("DSH Remote stream did not answer its heartbeat".into());
          }
          next = body.next() => {
            match next {
              Some(Ok(Message::Text(text))) if text.len() <= max_frame_bytes => {
                let native = serde_json::from_str(&text).map_err(|_| "Invalid DSH Remote JSON frame")?;
                let action = mux.ingest(&inner, &streams, native).await?;
                dispatch(&inner, action, &mut tasks, &mut pending, &mut retired, &mut order)?;
              }
              Some(Ok(Message::Ping(_))) => {
                if !matches!(tokio::time::timeout(heartbeat_timeout, body.flush()).await, Ok(Ok(()))) { return Err("DSH Remote heartbeat flush failed".into()); }
              }
              Some(Ok(Message::Pong(nonce))) => {
                if ping.as_ref().is_some_and(|(_, expected)| nonce.as_ref() == expected.as_slice()) { ping = None; }
              }
              _ => return Err("DSH Remote event continuity was lost; reconnect to refresh its baseline".into()),
            }
          }
        }
      }
    }.await;
    tasks.abort_all();
    if let Err(reason) = result {
      if !inner.closed.load(Ordering::Acquire) && !inner.disconnected.swap(true, Ordering::AcqRel) {
        let _ = inner.shutdown.send(true);
        inner.emit(None, RuntimeEventKind::Disconnected { reason });
      }
    }
  })
}
