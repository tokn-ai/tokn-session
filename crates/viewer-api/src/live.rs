//! Per-connection live interests. HTTP owns snapshots and resource loading.
use super::{ApiError, ApiState, error, same_token};
use axum::{
  extract::{
    State, WebSocketUpgrade,
    ws::{Message, WebSocket},
  },
  http::{HeaderMap, StatusCode},
  response::IntoResponse,
};
use serde_json::{Value, json};
use std::{collections::HashSet, time::Duration};
use tokn_viewer_core::{runtime::ViewerEvent, updates::SessionUpdatesRequest};

pub(super) async fn upgrade(
  State(state): State<ApiState>,
  headers: HeaderMap,
  ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, ApiError> {
  if let Some(origin) = headers.get("origin").and_then(|value| value.to_str().ok()) {
    let same_origin = origin
      .parse::<axum::http::Uri>()
      .ok()
      .and_then(|uri| uri.authority().map(|authority| authority.to_string()))
      .is_some_and(|authority| headers.get("host").and_then(|value| value.to_str().ok()) == Some(authority.as_str()));
    if !same_origin && !state.origins.iter().any(|allowed| allowed == origin) {
      return Err(error(StatusCode::FORBIDDEN, "WebSocket origin is not allowed"));
    }
  }
  let permit = state
    .subscribers
    .clone()
    .try_acquire_owned()
    .map_err(|_| error(StatusCode::TOO_MANY_REQUESTS, "Too many viewer subscriptions"))?;
  Ok(ws.max_message_size(1024 * 1024).on_upgrade(move |socket| async move {
    let _permit = permit;
    run(socket, state).await;
  }))
}

async fn send(socket: &mut WebSocket, value: Value) -> Result<(), ()> {
  tokio::time::timeout(
    Duration::from_secs(10),
    socket.send(Message::Text(value.to_string().into())),
  )
  .await
  .map_err(|_| ())?
  .map_err(|_| ())
}

async fn run(mut socket: WebSocket, state: ApiState) {
  // Browsers cannot supply Authorization on a WebSocket handshake. The token
  // travels in the first frame, never in a URL, and no events precede auth.
  let auth = tokio::time::timeout(Duration::from_secs(5), socket.recv()).await;
  let Ok(Some(Ok(Message::Text(auth)))) = auth else {
    return;
  };
  let Ok(auth) = serde_json::from_str::<Value>(&auth) else {
    return;
  };
  if auth["kind"] != "authenticate"
    || state.token.as_ref().is_some_and(|token| {
      !auth["token"]
        .as_str()
        .is_some_and(|supplied| same_token(supplied.as_bytes(), token.as_bytes()))
    })
  {
    return;
  }
  let mut receiver = state.events.subscribe();
  if send(&mut socket, json!({"kind":"ready"})).await.is_err() {
    return;
  }
  let mut owned = HashSet::<String>::new();
  static NEXT_CONNECTION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
  let owner = format!(
    "socket:{}",
    NEXT_CONNECTION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
  );
  loop {
    tokio::select! {
      _ = state.shutdown.cancelled() => break,
      incoming = socket.recv() => {
        let Some(Ok(Message::Text(text))) = incoming else { break; };
        let Ok(frame) = serde_json::from_str::<Value>(&text) else { break; };
        if frame["kind"] == "ping" {
          let ids = owned.iter().cloned().collect::<Vec<_>>();
          if state.service.renew_session_subscriptions(&ids).is_err() { break; }
          if send(&mut socket, json!({"kind":"pong"})).await.is_err() { break; }
          continue;
        }
        if frame["kind"] != "subscribe" { break; }
        let request_id = frame["request_id"].clone();
        let result = match serde_json::from_value::<SessionUpdatesRequest>(frame["request"].clone()) {
          Ok(request) if owned.len() < 24 || owned.contains(&request.subscription_id) => {
            let id = request.subscription_id.clone();
            let unsubscribe = request.unsubscribe;
            let service = state.service.clone();
            let request_owner = owner.clone();
            let result = tokio::task::spawn_blocking(move || service.subscribe_session_owned(request, Some(request_owner))).await
              .map_err(|_| "Subscription task failed".to_owned()).and_then(|value| value);
            if result.is_ok() { if unsubscribe { owned.remove(&id); } else { owned.insert(id); } }
            result
          }
          _ => Err("Invalid session subscription".into()),
        };
        let reply = match result {
          Ok(result) => json!({"kind":"ack","request_id":request_id,"result":result}),
          Err(error) => json!({"kind":"ack","request_id":request_id,"error":error}),
        };
        if send(&mut socket, reply).await.is_err() { break; }
      }
      event = receiver.recv() => {
        let Ok(event) = event else { break; };
        let Some(value) = live_frame(event, &owned) else { continue; };
        if send(&mut socket, value).await.is_err() { break; }
      }
    }
  }
  state.service.release_session_subscriptions(&owner);
}

fn live_frame(event: ViewerEvent, owned: &HashSet<String>) -> Option<Value> {
  if event.event != "session-updated"
    || !event.payload["subscription_id"]
      .as_str()
      .is_some_and(|id| owned.contains(id))
  {
    return None;
  }
  Some(if event.payload["snapshot"] == true {
    json!({"kind":"event","event":"session-resync-required","payload":{"session_key":event.payload["session_key"],"subscription_id":event.payload["subscription_id"]}})
  } else {
    json!({"kind":"event","event":event.event,"payload":event.payload})
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn live_delivery_filters_foreign_interests_and_never_pushes_history_snapshots() {
    let owned = HashSet::from(["mine".to_owned()]);
    let frame = |id: &str, snapshot| ViewerEvent {
      event: "session-updated".into(),
      payload: json!({"subscription_id":id,"snapshot":snapshot,"items":[{"detail":"large tool output"}],"session_key":"one"}),
    };
    assert!(live_frame(frame("other", false), &owned).is_none());
    let update = live_frame(frame("mine", false), &owned).unwrap();
    assert_eq!(update["event"], "session-updated");
    let reset = live_frame(frame("mine", true), &owned).unwrap();
    assert_eq!(reset["event"], "session-resync-required");
    assert!(reset["payload"].get("items").is_none());
  }
}

#[cfg(test)]
mod socket_tests {
  use super::*;
  use futures_util::{SinkExt, StreamExt};
  use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message as WireMessage, client::IntoClientRequest},
  };

  #[tokio::test]
  async fn authenticates_before_delivery_and_excludes_foreign_session_traffic() {
    let root = tempfile::tempdir().unwrap();
    let service = tokn_viewer_core::ViewerService::native(root.path().join("index.sqlite")).unwrap();
    let (events, _) = tokio::sync::broadcast::channel(16);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let app = crate::router(service, events.clone(), Some("secret".into()), vec![], shutdown.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
      axum::serve(listener, app).await.unwrap();
    });
    let url = format!("ws://{address}/api/v1/live");
    let mut forged = url.clone().into_client_request().unwrap();
    forged
      .headers_mut()
      .insert("origin", "https://foreign.example".parse().unwrap());
    assert!(connect_async(forged).await.is_err());
    let (mut wrong, _) = connect_async(&url).await.unwrap();
    wrong
      .send(WireMessage::Text(
        json!({"kind":"authenticate","token":"wrong"}).to_string().into(),
      ))
      .await
      .unwrap();
    let received = tokio::time::timeout(Duration::from_secs(1), wrong.next())
      .await
      .unwrap();
    assert!(!matches!(received, Some(Ok(WireMessage::Text(_)))));
    let (mut socket, _) = connect_async(&url).await.unwrap();
    socket
      .send(WireMessage::Text(
        json!({"kind":"authenticate","token":"secret"}).to_string().into(),
      ))
      .await
      .unwrap();
    let ready = socket.next().await.unwrap().unwrap().into_text().unwrap();
    assert_eq!(serde_json::from_str::<Value>(&ready).unwrap()["kind"], "ready");
    events
      .send(ViewerEvent {
        event: "session-updated".into(),
        payload: json!({"subscription_id":"foreign","items":[{"detail":"private"}]}),
      })
      .unwrap();
    assert!(
      tokio::time::timeout(Duration::from_millis(30), socket.next())
        .await
        .is_err()
    );
    socket
      .send(WireMessage::Text(
        json!({"kind":"subscribe","request_id":"invalid","request":{
          "subscription_id":"mine","session_key":"forged","level":"steps","detail_keys":[]
        }})
        .to_string()
        .into(),
      ))
      .await
      .unwrap();
    let reply = socket.next().await.unwrap().unwrap().into_text().unwrap();
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["request_id"], "invalid");
    assert!(reply["error"].is_string());
    socket.close(None).await.unwrap();
    shutdown.cancel();
    server.abort();
  }
}
