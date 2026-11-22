use super::*;
use crate::{Connection, ConnectionInfo, TransportState, direct::DirectRoute};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite::Message};
use tokn_hub_client_core::{protocol, secure::NoiseResponder};
use url::Url;

#[derive(Default)]
struct Counts {
  handshakes: AtomicUsize,
  submissions: AtomicUsize,
  canceled: tokio::sync::Notify,
}

async fn record(socket: &mut WebSocketStream<TcpStream>) -> Option<Vec<u8>> {
  match socket.next().await {
    Some(Ok(Message::Binary(record))) => Some(record.to_vec()),
    _ => None,
  }
}

async fn serve(socket: TcpStream, identity: &NoiseIdentity, counts: &Counts) {
  let mut socket = accept_async(socket).await.unwrap();
  let first = record(&mut socket).await.unwrap();
  let (reply, mut channel) = NoiseResponder::new(identity).unwrap().accept(&first).unwrap();
  counts.handshakes.fetch_add(1, Ordering::SeqCst);
  socket.send(Message::Binary(reply.into())).await.unwrap();
  let mut paths = HashMap::new();
  let mut last_id = 0;
  while let Some(bytes) = record(&mut socket).await {
    let InnerMessage::Stream { stream_id, frame } = channel.decrypt(&bytes).unwrap() else {
      panic!("Expected a logical request");
    };
    let mut reply = Vec::new();
    match *frame {
      InnerMessage::DeviceRequest { path, .. } => {
        assert!(stream_id > last_id);
        last_id = stream_id;
        paths.insert(stream_id, path);
      }
      InnerMessage::DiscoveryRequest {} => {
        assert!(stream_id > last_id);
        last_id = stream_id;
        reply.push(InnerMessage::DiscoveryResponse { group: None });
      }
      InnerMessage::DirectConfigRequest {} => {
        assert!(stream_id > last_id);
        last_id = stream_id;
        reply.push(InnerMessage::DirectConfig { ice_servers: vec![] });
      }
      InnerMessage::DirectOffer { sdp } => {
        assert!(stream_id > last_id);
        last_id = stream_id;
        reply.push(InnerMessage::DirectAnswer { sdp });
      }
      InnerMessage::RequestBody { .. } | InnerMessage::Window { .. } => {}
      InnerMessage::Cancel {} => counts.canceled.notify_one(),
      InnerMessage::RequestEnd {} => {
        let path = paths.remove(&stream_id).unwrap();
        if path == "/api/v1/submit_session_input" {
          counts.submissions.fetch_add(1, Ordering::SeqCst);
          return; // An uncertain delivery ends every stream on this carrier.
        }
        let events = path == "/api/v1/events";
        reply.push(InnerMessage::Response {
          status: 200,
          content_type: Some(
            if events {
              "text/event-stream"
            } else {
              "application/json"
            }
            .into(),
          ),
        });
        if events {
          for _ in 0..RESPONSE_WINDOW {
            reply.push(InnerMessage::Chunk {
              data: protocol::encode(b"event: ready\ndata: {}\n\n"),
            });
          }
        } else {
          reply.push(InnerMessage::Chunk {
            data: protocol::encode(&serde_json::to_vec(&json!({"path":path})).unwrap()),
          });
          reply.push(InnerMessage::End {});
        }
      }
      _ => panic!("Unexpected client frame"),
    }
    for frame in reply {
      let record = channel.encrypt(&frame.on_stream(stream_id)).unwrap();
      if socket.send(Message::Binary(record.into())).await.is_err() {
        return;
      }
    }
  }
}

async fn fixture() -> (Arc<Connection>, Arc<Counts>, tokio::task::JoinHandle<()>) {
  let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
  let identity = Arc::new(NoiseIdentity::generate().unwrap());
  let endpoint = Url::parse(&format!("ws://{}", listener.local_addr().unwrap())).unwrap();
  let connection = Arc::new(Connection {
    info: ConnectionInfo {
      connection_id: "test".into(),
      endpoint: endpoint.to_string(),
      host_id: "machine".into(),
      host_public_key: identity.public_key(),
      discovery_group: None,
      transport: TransportState::default(),
    },
    endpoint,
    identity: Arc::new(NoiseIdentity::generate().unwrap()),
    cancellation: CancellationToken::new(),
    requests: Arc::new(Semaphore::new(MAX_STREAMS)),
    listening: AtomicBool::new(false),
    sink: Arc::new(|_, _| {}),
    direct: DirectRoute::default(),
  });
  let counts = Arc::new(Counts::default());
  let host_counts = counts.clone();
  let server = tokio::spawn(async move {
    let mut channels = tokio::task::JoinSet::new();
    loop {
      let (socket, _) = listener.accept().await.unwrap();
      let identity = identity.clone();
      let counts = host_counts.clone();
      channels.spawn(async move {
        serve(socket, &identity, &counts).await;
      });
    }
  });
  (connection, counts, server)
}

#[tokio::test]
async fn concurrent_first_use_and_paused_events_share_the_health_handshake() {
  let (connection, counts, server) = fixture().await;
  let (health, events, discovery) = tokio::try_join!(
    connection.request("health", json!({})),
    connection.exchange("POST", "/api/v1/events", b"{}"),
    connection.discovery(),
  )
  .unwrap();
  assert_eq!(health["path"], "/api/v1/health");
  assert!(discovery.is_none());
  // The host fills the event stream's receive window while the consumer
  // leaves it paused. Independent commands on the same channel still finish.
  let (sessions, status) = tokio::try_join!(
    connection.request("list_sessions", json!({})),
    connection.request("health", json!({})),
  )
  .unwrap();
  assert_eq!(sessions["path"], "/api/v1/list_sessions");
  assert_eq!(status["path"], "/api/v1/health");
  let session = connection.direct.session(&connection).await.unwrap();
  assert!(matches!(
    session
      .request_reply(InnerMessage::DirectConfigRequest {})
      .await
      .unwrap(),
    InnerMessage::DirectConfig { .. }
  ));
  assert!(matches!(
    session
      .request_reply(InnerMessage::DirectOffer { sdp: "v=0\r\n".into() })
      .await
      .unwrap(),
    InnerMessage::DirectAnswer { .. }
  ));
  drop(events);
  tokio::time::timeout(Duration::from_secs(2), counts.canceled.notified())
    .await
    .unwrap();
  assert_eq!(counts.handshakes.load(Ordering::SeqCst), 1);
  connection.cancellation.cancel();
  server.abort();
}

#[tokio::test]
async fn a_lost_command_is_never_replayed_when_a_new_request_reauthenticates() {
  let (connection, counts, server) = fixture().await;
  assert!(connection.request("submit_session_input", json!({})).await.is_err());
  assert_eq!(counts.handshakes.load(Ordering::SeqCst), 1);
  assert_eq!(
    connection.request("health", json!({})).await.unwrap()["path"],
    "/api/v1/health"
  );
  assert_eq!(counts.handshakes.load(Ordering::SeqCst), 2);
  assert_eq!(counts.submissions.load(Ordering::SeqCst), 1);
  connection.cancellation.cancel();
  server.abort();
}

#[tokio::test]
async fn a_quiet_relay_survives_the_individual_response_deadline() {
  let (connection, counts, server) = fixture().await;
  connection.request("health", json!({})).await.unwrap();
  tokio::task::yield_now().await;
  tokio::time::pause();
  tokio::time::advance(Duration::from_secs(120)).await;
  tokio::task::yield_now().await;
  tokio::time::resume();
  connection.request("health", json!({})).await.unwrap();
  assert_eq!(counts.handshakes.load(Ordering::SeqCst), 1);
  connection.cancellation.cancel();
  server.abort();
}
