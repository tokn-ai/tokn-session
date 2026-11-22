//! The current Gateway generation is separate from legacy ApiProxy downlinks.
use crate::{Inner, transport::Downlink};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
  collections::{BTreeMap, BTreeSet},
  sync::{Arc, atomic::Ordering},
  time::Duration,
};
use tokio::{
  sync::{Mutex, mpsc, watch},
  task::JoinHandle,
  time::{Instant, MissedTickBehavior},
};
use tokio_tungstenite::tungstenite::Message;
use tokn_dsh_rpc_protocol::{
  api,
  gateway::{ClientFrame, RemoteEvent, ServerFrame},
};
use tokn_interaction_core::*;

type Ready = watch::Sender<Option<Result<(), String>>>;
pub(crate) struct Observe {
  session: String,
  ready: Ready,
}
pub(crate) struct Gateway {
  sender: mpsc::Sender<Observe>,
  observed: Mutex<BTreeMap<String, watch::Receiver<Option<Result<(), String>>>>>,
  timeout: Duration,
}

impl Gateway {
  pub(crate) fn new(timeout: Duration) -> (Self, mpsc::Receiver<Observe>) {
    let (sender, receiver) = mpsc::channel(64);
    (
      Self {
        sender,
        observed: Mutex::new(BTreeMap::new()),
        timeout,
      },
      receiver,
    )
  }

  /// Both native subscriptions must be ready before sending input. Detached
  /// callers share the same observation instead of reopening streams on retry.
  pub(crate) async fn observe(&self, session: &str) -> Result<(), InteractionError> {
    let mut observed = self.observed.lock().await;
    let mut receiver = if let Some(receiver) = observed.get(session) {
      receiver.clone()
    } else {
      if observed.len() >= 4096 {
        return Err(InteractionError::unavailable("DSH observation limit exceeded"));
      }
      let (ready, receiver) = watch::channel(None);
      self
        .sender
        .try_send(Observe {
          session: session.into(),
          ready,
        })
        .map_err(|_| InteractionError::unavailable("DSH Gateway observation channel is unavailable"))?;
      observed.insert(session.into(), receiver.clone());
      receiver
    };
    drop(observed);
    tokio::time::timeout(self.timeout, async {
      loop {
        if let Some(result) = receiver.borrow().clone() {
          return result.map_err(InteractionError::unavailable);
        }
        receiver
          .changed()
          .await
          .map_err(|_| InteractionError::unavailable("DSH observation ended before readiness"))?;
      }
    })
    .await
    .map_err(|_| InteractionError::unavailable("DSH observation readiness timed out"))?
  }
}

async fn send(socket: &mut Downlink, frame: &ClientFrame, timeout: Duration) -> Result<(), String> {
  let text = serde_json::to_string(frame).map_err(|_| "Invalid DSH stream request")?;
  tokio::time::timeout(timeout, socket.send(Message::Text(text.into())))
    .await
    .map_err(|_| "DSH stream write timed out")?
    .map_err(|_| "DSH stream write failed".into())
}

pub(crate) async fn handshake(socket: &mut Downlink, timeout: Duration) -> Result<String, InteractionError> {
  let operation = async {
    send(
      socket,
      &ClientFrame::Open {
        stream_id: "events".into(),
        endpoint: "$events".into(),
        payload: json!({"args":{}}),
      },
      timeout,
    )
    .await?;
    send(
      socket,
      &ClientFrame::End {
        stream_id: "events".into(),
      },
      timeout,
    )
    .await?;
    loop {
      match socket.next().await {
        Some(Ok(Message::Text(text))) => {
          if let ServerFrame::Item { stream_id, value } =
            serde_json::from_str(&text).map_err(|_| "Invalid DSH Gateway opening frame")?
          {
            if stream_id == "events" {
              if let RemoteEvent::Ready { client_id, .. } =
                serde_json::from_value(value).map_err(|_| "Invalid DSH event readiness")?
              {
                if valid_id(&client_id) {
                  return Ok::<String, String>(client_id);
                }
              }
            }
          }
          return Err("DSH Gateway did not publish event readiness".into());
        }
        Some(Ok(Message::Ping(_))) => socket.flush().await.map_err(|_| "DSH opening ping failed")?,
        Some(Ok(Message::Pong(_))) => {}
        _ => return Err("DSH Gateway closed before event readiness".into()),
      }
    }
  };
  tokio::time::timeout(timeout, operation)
    .await
    .map_err(|_| InteractionError::unavailable("DSH Gateway readiness timed out"))?
    .map_err(InteractionError::unavailable)
}

struct Subscription {
  session: String,
  state: bool,
  ready: Ready,
  opened: bool,
}

struct QuestionClaim {
  value: Value,
  native: Value,
  exposed: bool,
  deadline: Option<Instant>,
}

pub(crate) fn spawn(
  inner: Arc<Inner>,
  mut socket: Downlink,
  mut commands: mpsc::Receiver<Observe>,
  interval: Duration,
  timeout: Duration,
) -> JoinHandle<()> {
  tokio::spawn(async move {
    let mut shutdown = inner.shutdown.subscribe();
    let mut heartbeat = tokio::time::interval_at(Instant::now() + interval, interval);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut ping: Option<(Instant, Vec<u8>)> = None;
    let mut serial = 0_u64;
    let mut subscriptions: BTreeMap<String, Subscription> = BTreeMap::new();
    let mut claims: BTreeMap<String, QuestionClaim> = BTreeMap::new();
    // Native End may arrive after our Cancel. Retain only those in-flight
    // cancellations, bounded by the same limit as active business streams.
    let mut retired_claims = BTreeSet::new();
    let reason: String = loop {
      let question_deadline = claims.values().filter_map(|claim| claim.deadline).min();
      tokio::select! {
        changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { break "DSH Gateway closed".into(); } }
        command = commands.recv() => {
          let Some(command) = command else { break "DSH Gateway command channel closed".into(); };
          let mut failure = None;
          for state in [false, true] {
            serial += 1;
            let id = format!("observe-{serial}");
            let endpoint = if state { "session/observeState" } else { "session/observeEvent" };
            let request = if state { json!({"sessionId":command.session}) } else { json!({"address":{"kind":"session","sessionId":command.session}}) };
            let frame = ClientFrame::Open { stream_id: id.clone(), endpoint: endpoint.into(), payload: json!({"args":{"request":request}}) };
            subscriptions.insert(id.clone(), Subscription { session: command.session.clone(), state, ready: command.ready.clone(), opened: false });
            if let Err(reason) = send(&mut socket, &frame, timeout).await { failure = Some(reason); break; }
            if let Err(reason) = send(&mut socket, &ClientFrame::End { stream_id: id }, timeout).await { failure = Some(reason); break; }
          }
          if let Some(reason) = failure { break reason; }
        }
        _ = heartbeat.tick(), if ping.is_none() => {
          serial += 1;
          let nonce = serial.to_be_bytes().to_vec();
          if !matches!(tokio::time::timeout(timeout, socket.send(Message::Ping(nonce.clone().into()))).await, Ok(Ok(()))) { break "DSH Gateway heartbeat write failed".into(); }
          ping = Some((Instant::now() + timeout, nonce));
        }
        _ = tokio::time::sleep_until(ping.as_ref().map(|(deadline, _)| *deadline).unwrap_or_else(Instant::now)), if ping.is_some() => { break "DSH Gateway heartbeat timed out".into(); }
        _ = tokio::time::sleep_until(question_deadline.unwrap_or_else(Instant::now)), if question_deadline.is_some() => {
          let expired: Vec<_> = claims.iter().filter(|(_, claim)| claim.deadline.is_some_and(|deadline| deadline <= Instant::now())).map(|(id, _)| id.clone()).collect();
          let mut failure = None;
          for id in expired {
            let claim = claims.remove(&id).expect("expired question claim");
            if let Err(reason) = expire_question(&inner, &mut socket, &id, claim, &mut retired_claims, timeout).await {
              failure = Some(reason);
              break;
            }
          }
          if let Some(reason) = failure { break reason; }
        }
        next = socket.next() => match next {
          Some(Ok(Message::Text(text))) => {
            let native: Value = match serde_json::from_str(&text) { Ok(native) => native, Err(_) => break "Invalid DSH Gateway JSON".into() };
            let frame: ServerFrame = match serde_json::from_value(native.clone()) { Ok(frame) => frame, Err(_) => break "Invalid DSH Gateway stream frame".into() };
            match frame {
              ServerFrame::Item { stream_id, value } if stream_id == "events" => {
                if value["type"] == "cancel" {
                  let cancelled: Vec<_> = claims.iter().filter(|(_, claim)| claim.value["eventId"] == value["eventId"]).map(|(id, _)| id.clone()).collect();
                  let mut failure = None;
                  for id in cancelled {
                    claims.remove(&id);
                    retired_claims.insert(id.clone());
                    if let Err(reason) = send(&mut socket, &ClientFrame::Cancel { stream_id: id }, timeout).await { failure = Some(reason); break; }
                  }
                  if let Some(reason) = failure { break reason; }
                }
                if value["type"] == "waterfall" && value["event"] == "user-questions/request" && value["request"]["wait"]["timed"] == true {
                  let Some(agent) = value["agentId"].as_str().filter(|id| valid_id(id)) else { break "Invalid timed-question Agent identity".into(); };
                  let Some(event_id) = value["eventId"].as_str().filter(|id| valid_id(id)) else { break "Invalid timed-question event identity".into(); };
                  let Some(call) = value["request"]["wait"]["callId"].as_str().filter(|id| valid_id(id)) else { break "Invalid timed-question call identity".into(); };
                  if let Some(existing) = claims.values().find(|claim| claim.value["eventId"] == event_id) {
                    if existing.value != value { break "DSH changed an active timed-question identity".into(); }
                    continue;
                  }
                  if claims.len() + retired_claims.len() >= 128 { break "DSH question-claim limit exceeded".into(); }
                  serial += 1;
                  let id = format!("claim-{serial}");
                  let open = ClientFrame::Open { stream_id: id.clone(), endpoint: "userQuestions/attachWait".into(), payload: json!({"args":{"agentId":agent,"callId":call}}) };
                  if let Err(reason) = send(&mut socket, &open, timeout).await { break reason; }
                  if let Err(reason) = send(&mut socket, &ClientFrame::End { stream_id: id.clone() }, timeout).await { break reason; }
                  claims.insert(id, QuestionClaim { value, native, exposed: false, deadline: None });
                } else if let Err(reason) = event(&inner, value, native).await { break reason; }
              }
              ServerFrame::Item { stream_id, .. } if retired_claims.contains(&stream_id) => {}
              ServerFrame::End { stream_id } | ServerFrame::Error { stream_id, .. } if retired_claims.remove(&stream_id) => {}
              ServerFrame::Item { stream_id, value } if claims.contains_key(&stream_id) => {
                let claim = claims.get_mut(&stream_id).expect("known question claim");
                let Some(remaining) = value["remainingMs"].as_u64().filter(|remaining| *remaining <= 2_147_483_647) else { break "Invalid DSH question-claim readiness".into(); };
                if claim.deadline.is_some() { break "Invalid DSH question-claim readiness".into(); }
                // Establish the deadline before publishing a card. An already
                // expired native wait must never become answerable in the UI.
                claim.deadline = Some(Instant::now() + Duration::from_millis(remaining));
                if remaining == 0 { continue; }
                inner.emit(claim.value["agentId"].as_str().map(str::to_owned), RuntimeEventKind::Native(native));
                if let Err(reason) = event(&inner, claim.value.clone(), claim.native.clone()).await { break reason; }
                claim.exposed = true;
              }
              ServerFrame::Item { stream_id, value } => {
                let Some(subscription) = subscriptions.get_mut(&stream_id) else { break "DSH Gateway returned an unknown stream identity".into(); };
                let result = if subscription.state { state(&inner, &subscription.session, value, native).await }
                  else if value["type"] == "ready" { Ok(()) }
                  else if value["type"] == "event" {
                    let payload = json!({"type":"session/event","sessionId":subscription.session,"event":value["event"]});
                    project(&inner, "session/event", &stream_id, payload, native).await
                  } else { Err("Invalid DSH Session event frame".into()) };
                if let Err(reason) = result { break reason; }
                subscription.opened = true;
                let session = subscription.session.clone();
                let ready = subscription.ready.clone();
                if subscriptions.values().filter(|item| item.session == session).all(|item| item.opened) { let _ = ready.send(Some(Ok(()))); }
              }
              ServerFrame::End { stream_id } | ServerFrame::Error { stream_id, .. } if claims.contains_key(&stream_id) => {
                let claim = claims.remove(&stream_id).expect("known question claim");
                let id = claim.value["eventId"].as_str().expect("validated question identity");
                inner.emit(claim.value["agentId"].as_str().map(str::to_owned), RuntimeEventKind::Native(native));
                let mut cache = inner.state.lock().await;
                cache.responding.remove(id);
                let pending = cache.pending.remove(id);
                drop(cache);
                if let Some(pending) = pending {
                  inner.emit(pending.session_id, RuntimeEventKind::InteractionResolved { interaction_id: id.into() });
                }
                if !claim.exposed { let _ = inner.transport.respond(id, json!({"kind":"next"})).await; }
              }
              ServerFrame::End { .. } | ServerFrame::Error { .. } => break "DSH Gateway logical stream ended; attach again to recover".into(),
            }
          }
          Some(Ok(Message::Ping(_))) => { if !matches!(tokio::time::timeout(timeout, socket.flush()).await, Ok(Ok(()))) { break "DSH Gateway ping failed".into(); } }
          Some(Ok(Message::Pong(nonce))) => { if ping.as_ref().is_some_and(|(_, expected)| nonce.as_ref() == expected.as_slice()) { ping = None; } }
          _ => break "DSH Gateway stream was interrupted".into(),
        }
      }
    };
    for subscription in subscriptions.values() {
      let _ = subscription.ready.send(Some(Err(reason.clone())));
    }
    if !inner.closed.load(Ordering::Acquire) && !inner.disconnected.swap(true, Ordering::AcqRel) {
      let _ = inner.shutdown.send(true);
      inner.emit(None, RuntimeEventKind::Disconnected { reason });
    }
  })
}

async fn expire_question(
  inner: &Inner,
  socket: &mut Downlink,
  stream_id: &str,
  claim: QuestionClaim,
  retired: &mut BTreeSet<String>,
  timeout: Duration,
) -> Result<(), String> {
  let id = claim.value["eventId"].as_str().expect("validated question identity");
  let mut cache = inner.state.lock().await;
  // User responses reserve the same pending request under this lock. A
  // dispatched or unknown answer owns settlement; its countdown cannot send
  // a second, contradictory waterfall outcome.
  let responding = cache.responding.contains(id);
  let pending = if responding { None } else { cache.pending.remove(id) };
  let send_timeout = !responding && (pending.is_some() || !claim.exposed);
  drop(cache);
  if let Some(pending) = pending {
    inner.emit(
      pending.session_id,
      RuntimeEventKind::InteractionResolved {
        interaction_id: id.into(),
      },
    );
  }
  if send_timeout {
    // One attempt only. Releasing the native claim below also lets the Host's
    // original deadline expire if this result could not be confirmed.
    let _ = inner.transport.respond(id, json!({"kind":"rejected","error":{
      "name":"UserQuestionError", "message":"ask_user_question timed out before the user answered", "code":"ASK_TIMED_OUT"
    }})).await;
  }
  retired.insert(stream_id.into());
  send(
    socket,
    &ClientFrame::Cancel {
      stream_id: stream_id.into(),
    },
    timeout,
  )
  .await
}

fn valid_id(value: &str) -> bool {
  !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
}

async fn project(inner: &Inner, method: &str, id: &str, payload: Value, native: Value) -> Result<(), String> {
  crate::events::ingest_native(
    inner,
    api::Request {
      native_type: "server-request".into(),
      rpc_id: id.into(),
      method: method.into(),
      payload,
      extra: Default::default(),
    },
    native,
  )
  .await
}

async fn state(inner: &Inner, session: &str, value: Value, native: Value) -> Result<(), String> {
  let state: tokn_dsh_rpc_protocol::gateway::SessionState =
    serde_json::from_value(value).map_err(|_| "Invalid DSH passive Session state")?;
  inner.emit(Some(session.into()), RuntimeEventKind::Native(native));
  let mut cache = inner.state.lock().await;
  if !cache.sessions.contains_key(session) && cache.sessions.len() >= 4096 {
    return Err("DSH live session limit exceeded".into());
  }
  let current = cache.sessions.entry(session.into()).or_insert_with(|| SessionState {
    session_id: Some(session.into()),
    status: ExecutionState::Unknown,
    model: None,
    pending_inputs: None,
    revision: None,
  });
  current.status = if state.running {
    ExecutionState::Running
  } else {
    ExecutionState::Idle
  };
  current.pending_inputs = state.queue.as_ref().map(|queue| queue.len() as u64);
  let snapshot = current.clone();
  drop(cache);
  inner.emit(Some(session.into()), RuntimeEventKind::State(snapshot));
  Ok(())
}

async fn event(inner: &Inner, value: Value, native: Value) -> Result<(), String> {
  let event: RemoteEvent = serde_json::from_value(value).map_err(|_| "Invalid DSH forwarded event")?;
  match event {
    RemoteEvent::Ready { .. } => Err("DSH Gateway changed its event generation".into()),
    RemoteEvent::Emit { event, args } => {
      let session = if event == "api-session/added" {
        args.first().and_then(|value| value["sessionId"].as_str())
      } else {
        args.first().and_then(Value::as_str)
      };
      match event.as_str() {
        "api-session/status" => {
          project(
            inner,
            "host/session-status",
            "status",
            json!({"type":"host/session-status","sessionId":session,"running":args.get(1)}),
            native,
          )
          .await
        }
        "api-session/removed" => {
          project(
            inner,
            "host/session-removed",
            "removed",
            json!({"type":"host/session-removed","sessionId":session}),
            native,
          )
          .await
        }
        _ => {
          inner.emit(
            session
              .filter(|_| event.starts_with("api-session/"))
              .map(str::to_string),
            RuntimeEventKind::Native(native),
          );
          Ok(())
        }
      }
    }
    RemoteEvent::Cancel { event_id } => {
      if !valid_id(&event_id) {
        return Err("Invalid DSH waterfall cancellation identity".into());
      }
      let mut cache = inner.state.lock().await;
      cache.responding.remove(&event_id);
      let pending = cache.pending.remove(&event_id);
      drop(cache);
      inner.emit(
        pending.as_ref().and_then(|pending| pending.session_id.clone()),
        RuntimeEventKind::Native(native),
      );
      if let Some(pending) = pending {
        inner.emit(
          pending.session_id,
          RuntimeEventKind::InteractionResolved {
            interaction_id: event_id,
          },
        );
      }
      Ok(())
    }
    RemoteEvent::Waterfall {
      event,
      event_id,
      agent_id,
      request,
    } => {
      if !valid_id(&event_id) || !valid_id(&agent_id) {
        return Err("Invalid DSH waterfall identity".into());
      }
      let method = match event.as_str() {
        "approval/request" => "approval/requested",
        "user-questions/request" => "question/requested",
        _ => {
          inner.emit(Some(agent_id), RuntimeEventKind::Native(native));
          let _ = inner
            .transport
            .respond(&event_id, json!({"kind":"next"}))
            .await
            .map_err(|_| "DSH could not delegate unsupported interaction")?;
          return Ok(());
        }
      };
      let mut payload = request;
      payload["type"] = json!(method);
      payload["sessionId"] = json!(agent_id);
      if method == "approval/requested" {
        payload["approvalId"] = json!(event_id);
      }
      project(inner, method, &event_id, payload, native).await
    }
  }
}
