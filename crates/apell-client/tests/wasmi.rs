//! Run the real release guest in Apell's 32M-fuel Wasmi runtime. The generic
//! services below implement its SDK contract; the peer is a real loopback WS
//! host speaking SPAKE2 and Noise, without provider credentials or agent work.
use apell_runtime::{HostError, HostEvents, HostServices, RuntimeHandle, ServiceFuture};
use futures_util::{
  SinkExt, StreamExt,
  stream::{SplitSink, SplitStream},
};
use serde_json::{Value, json};
use std::{
  collections::BTreeMap,
  path::PathBuf,
  process::Command,
  sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
  },
  time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex as AsyncMutex, Notify, watch};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};
use tokn_hub_client_core::{
  account::{DEVICE_TOKEN, VIEWER_START},
  discovery::{DISCOVERY_ADDRESSES_HEADER, DiscoveredMachine, DiscoveryGroup, DiscoveryResult},
  pairing::{ClientKind, HostPairing, TotpSecret, is_pairing_record},
  presence::{MachinePresence, PRESENCE_VERSIONS_HEADER, PresenceRequest, PresenceResult},
  protocol,
  secure::{InnerMessage, MAX_CHUNK, NoiseIdentity, NoiseResponder},
};

const HOST_ID: &str = "11111111-2222-4333-8444-555555555555";
const TEST_ACCOUNT_ID: &str = "11111111-2222-4333-8444-999999999999";
type ClientSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

fn guest() -> Vec<u8> {
  static WASM: OnceLock<Vec<u8>> = OnceLock::new();
  WASM
    .get_or_init(|| {
      let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
      if let Some(path) = std::env::var_os("TOKN_APELL_TEST_WASM") {
        return std::fs::read(root.join(path)).expect("read supplied applet test module");
      }
      let target = root.join("target/apell-integration");
      let status = Command::new(env!("CARGO"))
        .current_dir(&root)
        .args([
          "build",
          "--release",
          "--locked",
          "--offline",
          "--target",
          "wasm32-unknown-unknown",
          "-p",
          "tokn-session-apell",
          "--target-dir",
        ])
        .arg(&target)
        .env_remove("CARGO_MAKEFLAGS")
        .env_remove("MAKEFLAGS")
        .status()
        .expect("compile the Wasmi guest");
      assert!(
        status.success(),
        "applet integration requires the wasm32-unknown-unknown target"
      );
      std::fs::read(target.join("wasm32-unknown-unknown/release/tokn_session_apell.wasm")).unwrap()
    })
    .clone()
}

struct Socket {
  read: AsyncMutex<SplitStream<ClientSocket>>,
  write: AsyncMutex<SplitSink<ClientSocket, Message>>,
  stop: watch::Sender<bool>,
}
#[derive(Default)]
struct Services {
  sockets: Mutex<BTreeMap<String, Arc<Socket>>>,
  next: AtomicUsize,
  secrets: Mutex<BTreeMap<String, String>>,
  config: Mutex<BTreeMap<String, Value>>,
  discovery: Mutex<Option<DiscoveryResult>>,
  presence: Mutex<Option<PresenceResult>>,
  presence_requests: AtomicUsize,
  account_approved: AtomicBool,
  fail_next_secret_write: AtomicBool,
}
impl Services {
  fn socket(&self, handle: &str) -> Result<Arc<Socket>, String> {
    self
      .sockets
      .lock()
      .unwrap()
      .get(handle)
      .cloned()
      .ok_or("Closed fixture socket".into())
  }
  async fn call_inner(&self, method: &str, params: Value) -> Result<Value, String> {
    let text = |key: &str| params[key].as_str().ok_or_else(|| format!("Missing fixture {key}"));
    match method {
      "websocket.open" => {
        let url = text("url")?;
        assert!(url.starts_with("ws://127.0.0.1:"));
        let (io, _) = tokio_tungstenite::connect_async(url)
          .await
          .map_err(|_| "Fixture connection failed")?;
        let id = format!("fixture-{}", self.next.fetch_add(1, Ordering::SeqCst));
        let (write, read) = io.split();
        self.sockets.lock().unwrap().insert(
          id.clone(),
          Arc::new(Socket {
            read: AsyncMutex::new(read),
            write: AsyncMutex::new(write),
            stop: watch::channel(false).0,
          }),
        );
        Ok(json!({"socket_handle":id}))
      }
      "websocket.send" => {
        let socket = self.socket(text("socket_handle")?)?;
        let bytes = protocol::decode(text("data")?, 65535)?;
        if *socket.stop.borrow() {
          return Err("Fixture socket closed".into());
        }
        socket
          .write
          .lock()
          .await
          .send(Message::Binary(bytes.into()))
          .await
          .map_err(|_| "Fixture send failed")?;
        Ok(Value::Null)
      }
      "websocket.receive" => {
        let socket = self.socket(text("socket_handle")?)?;
        let mut stopping = socket.stop.subscribe();
        let mut io = socket.read.lock().await;
        loop {
          if *stopping.borrow() {
            return Ok(json!({"data":null}));
          }
          tokio::select! {
            _ = stopping.changed() => return Ok(json!({"data":null})),
            frame = io.next() => match frame {
              Some(Ok(Message::Binary(bytes))) => return Ok(json!({"data":protocol::encode(&bytes)})),
              Some(Ok(Message::Ping(_))) => { socket.write.lock().await.flush().await.map_err(|_|"Fixture ping failed")?; },
              Some(Ok(Message::Pong(_))) => {},
              Some(Ok(Message::Close(_))) | None => return Ok(json!({"data":null})),
              _ => return Err("Fixture receive failed".into()),
            }
          }
        }
      }
      "websocket.close" => {
        let socket = self.sockets.lock().unwrap().remove(text("socket_handle")?);
        if let Some(socket) = socket {
          socket.stop.send_replace(true);
          let _ = socket.write.lock().await.close().await;
        }
        Ok(Value::Null)
      }
      "secrets.get" => Ok(json!({"value":self.secrets.lock().unwrap().get(text("key")?).cloned()})),
      "secrets.set" => {
        if self.fail_next_secret_write.swap(false, Ordering::SeqCst) {
          return Err("Fixture preference write failed".into());
        }
        self
          .secrets
          .lock()
          .unwrap()
          .insert(text("key")?.into(), text("value")?.into());
        Ok(Value::Null)
      }
      "secrets.delete" => {
        self.secrets.lock().unwrap().remove(text("key")?);
        Ok(Value::Null)
      }
      "crypto.random_bytes" => {
        assert_eq!(params["length"], 32);
        let fresh = NoiseIdentity::generate()?;
        Ok(json!({"data":protocol::encode(fresh.private_key())}))
      }
      "runtime.sleep" => {
        tokio::time::sleep(Duration::from_millis(params["duration_ms"].as_u64().unwrap())).await;
        Ok(Value::Null)
      }
      "config.get" => {
        let value = self.config.lock().unwrap().get(text("key")?).cloned();
        Ok(json!({"found":value.is_some(),"value":value}))
      }
      "config.set" => {
        self
          .config
          .lock()
          .unwrap()
          .insert(text("key")?.into(), params["value"].clone());
        Ok(Value::Null)
      }
      "http.request" => {
        let url = url::Url::parse(text("url")?).map_err(|_| "Invalid fixture URL")?;
        if url.path().starts_with("/hub/v1/account/") {
          return self.account_http(&url, &params);
        }
        if url.path() == "/hub/v1/presence" {
          assert_eq!(params["method"], "POST");
          assert_eq!(params["timeout_ms"], 10000);
          assert_eq!(
            params["headers"],
            json!([
              {"name":"Content-Type","value":"application/json"},
              {"name":"Accept","value":"application/json"},
              {"name":PRESENCE_VERSIONS_HEADER,"value":"1"},
            ])
          );
          let body: Vec<u8> = serde_json::from_value(params["body"].clone()).unwrap();
          let request: PresenceRequest = serde_json::from_slice(&body).unwrap();
          request.validate().unwrap();
          self.presence_requests.fetch_add(1, Ordering::SeqCst);
          let result = self.presence.lock().unwrap().clone().unwrap_or_else(|| PresenceResult {
            machines: request
              .host_ids
              .into_iter()
              .map(|host_id| MachinePresence {
                online: host_id == HOST_ID,
                connector_version: (host_id == HOST_ID).then(|| "0.1.1".into()),
                host_id,
              })
              .collect(),
          });
          return Ok(json!({"status":200,"body":serde_json::to_vec(&result).unwrap()}));
        }
        if url.path() == "/hub/v1/resolve/personal/fixture" {
          return Ok(json!({"status":200,"body":serde_json::to_vec(&json!({
            "host_id":HOST_ID,
            "machine_address":"personal:fixture",
            "name":"Named machine",
            "online":true,
          })).unwrap()}));
        }
        let discovery = self
          .discovery
          .lock()
          .unwrap()
          .clone()
          .ok_or("Missing discovery fixture")?;
        assert_eq!(
          url.path(),
          format!("/hub/v1/discovery/{}/{}", discovery.namespace, discovery.secret_digest)
        );
        assert_eq!(
          params["headers"],
          json!([{"name":DISCOVERY_ADDRESSES_HEADER,"value":"1"}])
        );
        Ok(json!({"status":200,"body":serde_json::to_vec(&discovery).unwrap()}))
      }
      _ => Err(format!("Unexpected fixture service {method}")),
    }
  }
  fn account_http(&self, url: &url::Url, request: &Value) -> Result<Value, String> {
    let response = |status, value: Value| {
      Ok(json!({
        "status":status,
        "headers":[{"name":"content-type","value":"application/json"}],
        "body":serde_json::to_vec(&value).unwrap(),
      }))
    };
    let token = format!("tkna_{}", protocol::encode(&[21; 32]));
    let device_code = protocol::encode(&[42; 32]);
    let authorized = request["headers"]
      .as_array()
      .unwrap()
      .iter()
      .any(|header| header["name"] == "Authorization" && header["value"] == format!("Bearer {token}"));
    let supported = url.host_str() != Some("old-hub.example");
    let approved = self.account_approved.load(Ordering::SeqCst);
    match url.path() {
      "/hub/v1/account/capabilities" => {
        assert!(!authorized);
        response(200, json!({"enabled":true,"viewer_device_login":supported}))
      }
      VIEWER_START => {
        assert!(!authorized);
        if !supported {
          return response(404, Value::Null);
        }
        response(
          200,
          json!({
            "device_code":device_code,"user_code":"ABCD-EFGH","expires_in":600,"interval":5,
          }),
        )
      }
      DEVICE_TOKEN => {
        assert!(!authorized);
        let body: Vec<u8> = serde_json::from_value(request["body"].clone()).unwrap();
        assert_eq!(
          serde_json::from_slice::<Value>(&body).unwrap(),
          json!({"device_code":device_code})
        );
        if !approved {
          return response(202, Value::Null);
        }
        response(
          200,
          json!({"token":token,"user_id":TEST_ACCOUNT_ID,"username":"personal"}),
        )
      }
      "/hub/v1/account/me" | "/hub/v1/account/machines" | "/hub/v1/account/logout" => {
        assert!(supported, "credential sent to an unrelated origin");
        assert!(authorized);
        if !approved {
          return response(401, Value::Null);
        }
        match url.path() {
          "/hub/v1/account/me" => response(200, json!({"user_id":TEST_ACCOUNT_ID,"username":"personal"})),
          "/hub/v1/account/machines" => response(
            200,
            json!({"machines":[
              {"host_id":HOST_ID,"machine_address":"personal:fixture","name":"Named machine","online":true},
            ]}),
          ),
          _ => {
            self.account_approved.store(false, Ordering::SeqCst);
            response(204, Value::Null)
          }
        }
      }
      _ => Err("Unexpected account request".into()),
    }
  }
  async fn close(&self) {
    let sockets = std::mem::take(&mut *self.sockets.lock().unwrap());
    for (_, socket) in sockets {
      socket.stop.send_replace(true);
      let _ = socket.write.lock().await.close().await;
    }
  }
}
struct Adapter(Arc<Services>);
impl HostServices for Adapter {
  fn call(&self, method: String, params: Value, _events: HostEvents) -> ServiceFuture {
    let services = self.0.clone();
    Box::pin(async move { services.call_inner(&method, params).await.map_err(HostError::new) })
  }
}

#[derive(Default)]
struct PeerSignals {
  push_event: Notify,
  stall_next_handshake: AtomicBool,
  handshake_stalled: Notify,
  stalled_socket_closed: Notify,
}
struct Peer {
  hub: String,
  key: String,
  secret: Arc<TotpSecret>,
  mutations: Arc<AtomicUsize>,
  drop_ack: Arc<AtomicBool>,
  revoked: Arc<AtomicBool>,
  discovery: Arc<Mutex<Option<DiscoveryGroup>>>,
  signals: Arc<PeerSignals>,
  task: tokio::task::JoinHandle<()>,
}
impl Drop for Peer {
  fn drop(&mut self) {
    self.task.abort();
  }
}
async fn record(socket: &mut WebSocketStream<tokio::net::TcpStream>) -> Result<Vec<u8>, String> {
  loop {
    match socket.next().await {
      Some(Ok(Message::Binary(bytes))) => return Ok(bytes.to_vec()),
      Some(Ok(Message::Ping(_))) => socket.flush().await.map_err(|_| "Peer ping failed")?,
      _ => return Err("Peer stream closed".into()),
    }
  }
}
fn now() -> u64 {
  SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}
impl Peer {
  async fn start() -> Self {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub = format!("http://{}/", listener.local_addr().unwrap());
    let identity = Arc::new(NoiseIdentity::generate().unwrap());
    let key = identity.public_key();
    let secret = Arc::new(TotpSecret::generate());
    let discovery = Arc::new(Mutex::new(Some(DiscoveryGroup {
      namespace: "personal".into(),
      secret_digest: protocol::encode(&[1; 32]),
    })));
    let paired = Arc::new(Mutex::new(None::<String>));
    let mutations = Arc::new(AtomicUsize::new(0));
    let drop_ack = Arc::new(AtomicBool::new(false));
    let revoked = Arc::new(AtomicBool::new(false));
    let signals = Arc::new(PeerSignals::default());
    let server_signals = signals.clone();
    let (counter, lost, revocation, seed, group) = (
      mutations.clone(),
      drop_ack.clone(),
      revoked.clone(),
      secret.clone(),
      discovery.clone(),
    );
    let task = tokio::spawn(async move {
      let mut clients = tokio::task::JoinSet::new();
      loop {
        let (socket, _) = listener.accept().await.unwrap();
        let (identity, secret, paired, counter, lost, revoked, discovery) = (
          identity.clone(),
          seed.clone(),
          paired.clone(),
          counter.clone(),
          lost.clone(),
          revocation.clone(),
          group.clone(),
        );
        let signals = server_signals.clone();
        clients.spawn(async move {
          let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
          let first = record(&mut socket).await.unwrap();
          if is_pairing_record(&first) {
            let (state, response) = HostPairing::respond(&secret, HOST_ID, &identity, &first, now()).unwrap();
            socket.send(Message::Binary(response.into())).await.unwrap();
            let (authorized, ack) = state
              .finish_with_discovery(
                &record(&mut socket).await.unwrap(),
                now(),
                discovery.lock().unwrap().clone(),
              )
              .unwrap();
            assert_eq!(authorized.client_kind, ClientKind::Native);
            *paired.lock().unwrap() = Some(authorized.client_public_key);
            socket.send(Message::Binary(ack.into())).await.unwrap();
            return;
          }
          let (response, mut channel) = NoiseResponder::new(&identity).unwrap().accept(&first).unwrap();
          assert_eq!(paired.lock().unwrap().as_deref(), Some(channel.remote_public_key()));
          if signals.stall_next_handshake.swap(false, Ordering::SeqCst) {
            signals.handshake_stalled.notify_one();
            assert!(record(&mut socket).await.is_err());
            signals.stalled_socket_closed.notify_one();
            return;
          }
          socket.send(Message::Binary(response.into())).await.unwrap();
          let mut requests = BTreeMap::<u32, (String, Vec<u8>)>::new();
          let mut previous = 0;
          let mut live_stream = None;
          loop {
            let bytes = tokio::select! {
              result = record(&mut socket) => match result {
                Ok(bytes) => bytes,
                Err(_) => break,
              },
              _ = signals.push_event.notified(), if live_stream.is_some() => {
                let event = InnerMessage::Chunk {
                  data: protocol::encode(b"event: session-updated\ndata: {\"session_key\":\"after-reattach\"}\n\n"),
                };
                let bytes = channel.encrypt(&event.on_stream(live_stream.unwrap())).unwrap();
                if socket.send(Message::Binary(bytes.into())).await.is_err() {
                  break;
                }
                continue;
              }
            };
            let InnerMessage::Stream { stream_id, frame } = channel.decrypt(&bytes).unwrap() else {
              panic!("Expected a multiplexed request");
            };
            let mut reply = Vec::new();
            match *frame {
              InnerMessage::DiscoveryRequest {} => {
                assert!(stream_id > previous);
                previous = stream_id;
                reply.push(InnerMessage::DiscoveryResponse {
                  group: discovery.lock().unwrap().clone(),
                });
              }
              InnerMessage::DeviceRequest { method, path } => {
                assert!(stream_id > previous);
                previous = stream_id;
                if path == "/api/v1/events" {
                  assert_eq!(method, "POST");
                }
                requests.insert(stream_id, (path, Vec::new()));
              }
              InnerMessage::RequestBody { data } => {
                requests
                  .get_mut(&stream_id)
                  .unwrap()
                  .1
                  .extend(protocol::decode(&data, MAX_CHUNK).unwrap());
              }
              InnerMessage::RequestEnd {} => {
                let (path, body) = requests.remove(&stream_id).unwrap();
                if path == "/api/v1/submit_session_input" {
                  counter.fetch_add(1, Ordering::SeqCst);
                  assert_eq!(
                    serde_json::from_slice::<Value>(&body).unwrap()["request"]["prompt"],
                    "fixture prompt once"
                  );
                  if lost.load(Ordering::SeqCst) {
                    return;
                  }
                }
                let status = if revoked.load(Ordering::SeqCst) { 401 } else { 200 };
                let live = path == "/api/v1/events" && status == 200;
                let payload = if status == 401 {
                  serde_json::to_vec(&json!({"error":"Device revoked"})).unwrap()
                } else if live {
                  live_stream = Some(stream_id);
                  assert!(
                    !serde_json::from_slice::<Value>(&body).unwrap()["subscription_prefix"]
                      .as_str()
                      .unwrap()
                      .is_empty()
                  );
                  b"event: ready\ndata: {}\n\nevent: session-updated\ndata: {\"session_key\":\"fixture\"}\n\n".to_vec()
                } else if path == "/api/v1/health" {
                  serde_json::to_vec(&json!({"version":protocol::VIEWER_API_VERSION})).unwrap()
                } else {
                  serde_json::to_vec(&json!({"fixture":"response","path":path,"padding":"x".repeat(70_000)})).unwrap()
                };
                reply.push(InnerMessage::Response {
                  status,
                  content_type: Some(if live { "text/event-stream" } else { "application/json" }.into()),
                });
                assert!(payload.chunks(MAX_CHUNK).len() <= protocol::RESPONSE_WINDOW);
                reply.extend(payload.chunks(MAX_CHUNK).map(|chunk| InnerMessage::Chunk {
                  data: protocol::encode(chunk),
                }));
                if !live {
                  reply.push(InnerMessage::End {});
                }
              }
              InnerMessage::Window { credits: 1 } => {}
              InnerMessage::Cancel {} => {
                requests.remove(&stream_id);
                if live_stream == Some(stream_id) {
                  live_stream = None;
                }
              }
              other => panic!("Invalid peer request {other:?}"),
            }
            for frame in reply {
              if socket
                .send(Message::Binary(
                  channel.encrypt(&frame.on_stream(stream_id)).unwrap().into(),
                ))
                .await
                .is_err()
              {
                return;
              }
            }
          }
        });
      }
    });
    Self {
      hub,
      key,
      secret,
      mutations,
      drop_ack,
      revoked,
      discovery,
      signals,
      task,
    }
  }
}

async fn invoke(run: &RuntimeHandle, command: &str, params: Value) -> Result<Value, HostError> {
  tokio::time::timeout(Duration::from_secs(30), run.invoke(command, params))
    .await
    .expect("Wasmi command deadline exceeded")
}
async fn chunk(run: &RuntimeHandle, events: &mut tokio::sync::broadcast::Receiver<Value>, name: &str) -> Value {
  loop {
    let event = tokio::time::timeout(Duration::from_secs(20), events.recv())
      .await
      .unwrap()
      .unwrap();
    if event["name"] == name {
      let payload = event["payload"].clone();
      invoke(
        run,
        "ack_bridge_chunk",
        json!({"channel_id":payload["channel_id"],"sequence":payload["sequence"]}),
      )
      .await
      .unwrap();
      return payload;
    }
  }
}

#[tokio::test]
#[ignore = "requires a compiled wasm guest; CI runs this test explicitly"]
async fn interpreted_guest_pairs_streams_reuses_view_connections_and_never_replays_uncertain_input() {
  let peer = Peer::start().await;
  let services = Arc::new(Services::default());
  let discovery_group = peer.discovery.lock().unwrap().clone().unwrap();
  *services.discovery.lock().unwrap() = Some(DiscoveryResult {
    retired: false,
    namespace: discovery_group.namespace.clone(),
    secret_digest: discovery_group.secret_digest.clone(),
    machines: vec![
      DiscoveredMachine {
        host_id: HOST_ID.into(),
        name: "Paired machine".into(),
        online: true,
        machine_address: Some(format!("{}:paired", discovery_group.namespace)),
      },
      DiscoveredMachine {
        host_id: "11111111-2222-4333-8444-666666666666".into(),
        name: "Unpaired machine".into(),
        online: false,
        machine_address: None,
      },
    ],
  });
  let wasm = guest();
  let run = RuntimeHandle::start(wasm.clone(), Arc::new(Adapter(services.clone())))
    .await
    .unwrap();
  invoke(&run, "initialize", json!({})).await.unwrap();
  // Presence is public, exact routing metadata and must not initialize identity
  // storage, open a host socket, or depend on an existing pairing.
  let unpaired_id = "11111111-2222-4333-8444-666666666666";
  let presence = invoke(
    &run,
    "hub_client_presence",
    json!({"hub_url":peer.hub,"host_ids":[HOST_ID,unpaired_id]}),
  )
  .await
  .unwrap();
  assert_eq!(
    presence,
    json!({"machines":[
      {"host_id":HOST_ID,"online":true,"connector_version":"0.1.1"},
      {"host_id":unpaired_id,"online":false},
    ]})
  );
  assert_eq!(services.presence_requests.load(Ordering::SeqCst), 1);
  assert!(services.secrets.lock().unwrap().is_empty());
  assert!(services.config.lock().unwrap().is_empty());
  assert!(services.sockets.lock().unwrap().is_empty());
  assert!(
    invoke(
      &run,
      "hub_client_presence",
      json!({"hub_url":peer.hub,"host_ids":["invalid"]})
    )
    .await
    .is_err()
  );
  assert_eq!(
    invoke(&run, "hub_client_presence", json!({"hub_url":peer.hub,"host_ids":[]}))
      .await
      .unwrap(),
    json!({"machines":[]})
  );
  assert_eq!(services.presence_requests.load(Ordering::SeqCst), 1);
  *services.presence.lock().unwrap() = Some(PresenceResult {
    machines: vec![MachinePresence {
      host_id: unpaired_id.into(),
      online: true,
      connector_version: None,
    }],
  });
  assert!(
    invoke(
      &run,
      "hub_client_presence",
      json!({"hub_url":peer.hub,"host_ids":[HOST_ID]})
    )
    .await
    .is_err()
  );
  assert_eq!(services.presence_requests.load(Ordering::SeqCst), 2);
  *services.presence.lock().unwrap() = None;
  let before = invoke(&run, "hub_client_status", json!({"hub_url":peer.hub}))
    .await
    .unwrap();
  assert!(before["hosts"].as_array().unwrap().is_empty());
  assert!(
    invoke(
      &run,
      "hub_client_discover",
      json!({"hub_url":peer.hub,"namespace":discovery_group.namespace,"secret_digest":discovery_group.secret_digest})
    )
    .await
    .is_err()
  );
  let host = invoke(
    &run,
    "hub_client_pair",
    json!({"hub_url":peer.hub,"host_id":HOST_ID,"code":peer.secret.code_at(now()),"expected_host_public_key":peer.key}),
  )
  .await
  .unwrap();
  assert_eq!(host["host_public_key"], peer.key);
  assert_eq!(host["discovery_group"], json!(discovery_group));
  let discovered = invoke(
    &run,
    "hub_client_discover",
    json!({"hub_url":peer.hub,"namespace":discovery_group.namespace,"secret_digest":discovery_group.secret_digest}),
  )
  .await
  .unwrap();
  assert_eq!(discovered["machines"].as_array().unwrap().len(), 2);
  assert_eq!(
    discovered["machines"][0]["machine_address"],
    format!("{}:paired", discovery_group.namespace)
  );
  let status = invoke(&run, "hub_client_status", json!({"hub_url":peer.hub}))
    .await
    .unwrap();
  assert_eq!(status["hosts"].as_array().unwrap().len(), 1);
  assert!(status["selected_host_id"].is_null());
  let previous_sockets = services.next.load(Ordering::SeqCst);
  // Four views finishing startup together still claim one run connection.
  let simultaneous = futures_util::future::join_all((0..4).map(|_| {
    invoke(
      &run,
      "hub_client_open",
      json!({"hub_url":peer.hub,"host_id":HOST_ID,"host_public_key":peer.key}),
    )
  }))
  .await;
  let mut descriptor = simultaneous.first().unwrap().as_ref().unwrap().clone();
  for current in simultaneous {
    assert_eq!(current.unwrap()["connection_id"], descriptor["connection_id"]);
  }
  for _ in 0..12 {
    let current = invoke(
      &run,
      "hub_client_open",
      json!({"hub_url":peer.hub,"host_id":HOST_ID,"host_public_key":peer.key}),
    )
    .await
    .unwrap();
    if !descriptor.is_null() {
      assert_eq!(current["connection_id"], descriptor["connection_id"]);
    }
    descriptor = current;
  }
  assert_eq!(
    invoke(&run, "snapshot", json!({})).await.unwrap()["connections"]
      .as_array()
      .unwrap()
      .len(),
    1
  );
  let id = descriptor["connection_id"].as_str().unwrap().to_owned();
  *peer.discovery.lock().unwrap() = None;
  let refreshed = invoke(
    &run,
    "hub_client_open",
    json!({"hub_url":peer.hub,"host_id":HOST_ID,"host_public_key":peer.key}),
  )
  .await
  .unwrap();
  assert!(refreshed["discovery_group"].is_null());
  let named = invoke(
    &run,
    "hub_client_remember_metadata",
    json!({
      "hub_url":peer.hub,"host_id":HOST_ID,"machine_address":"personal:fixture","name":"Named machine",
    }),
  )
  .await
  .unwrap();
  assert!(named["discovery_group"].is_null());
  assert_eq!(named["host_public_key"], peer.key);
  *peer.discovery.lock().unwrap() = Some(discovery_group.clone());
  let refreshed = invoke(
    &run,
    "hub_client_open",
    json!({"hub_url":peer.hub,"host_id":HOST_ID,"host_public_key":peer.key}),
  )
  .await
  .unwrap();
  assert_eq!(refreshed["discovery_group"], json!(discovery_group));
  let named = invoke(
    &run,
    "hub_client_remember_metadata",
    json!({
      "hub_url":peer.hub,"host_id":HOST_ID,"machine_address":"personal:fixture","name":"Named machine",
    }),
  )
  .await
  .unwrap();
  assert_eq!(named["discovery_group"], json!(discovery_group));
  assert_eq!(named["host_public_key"], peer.key);
  let before_retirement = invoke(&run, "hub_client_status", json!({"hub_url":peer.hub}))
    .await
    .unwrap();
  let current_directory = services.discovery.lock().unwrap().clone();
  services.discovery.lock().unwrap().as_mut().unwrap().machines.clear();
  let discovery_params = json!({
    "hub_url":peer.hub,"namespace":discovery_group.namespace,"secret_digest":discovery_group.secret_digest,
  });
  invoke(&run, "hub_client_discover", discovery_params.clone())
    .await
    .unwrap();
  assert_eq!(
    invoke(&run, "hub_client_status", json!({"hub_url":peer.hub}))
      .await
      .unwrap(),
    before_retirement,
    "an empty current group must retain saved metadata"
  );
  services.discovery.lock().unwrap().as_mut().unwrap().retired = true;
  let retired = invoke(&run, "hub_client_discover", discovery_params.clone())
    .await
    .unwrap();
  assert_eq!(retired["retired"], true);
  let after_retirement = invoke(&run, "hub_client_status", json!({"hub_url":peer.hub}))
    .await
    .unwrap();
  assert!(after_retirement["hosts"][0]["discovery_group"].is_null());
  let mut expected_retirement = before_retirement.clone();
  expected_retirement["hosts"][0]
    .as_object_mut()
    .unwrap()
    .remove("discovery_group");
  assert_eq!(
    after_retirement, expected_retirement,
    "retirement changes only the cached group"
  );
  assert!(invoke(&run, "hub_client_discover", discovery_params).await.is_err());
  *services.discovery.lock().unwrap() = current_directory;
  // Existing authorization and pins still open the host and relearn its current group.
  invoke(
    &run,
    "hub_client_open",
    json!({"hub_url":peer.hub,"host_id":HOST_ID,"host_public_key":peer.key}),
  )
  .await
  .unwrap();
  assert_eq!(
    invoke(&run, "hub_client_status", json!({"hub_url":peer.hub}))
      .await
      .unwrap(),
    before_retirement
  );
  let mut events = run.subscribe();
  let reader = run.clone();
  let connection_id = id.clone();
  let request = tokio::spawn(async move {
    invoke(
      &reader,
      "hub_client_request",
      json!({"connection_id":connection_id,"command":"list_sessions","payload":{},"response_id":"list-response"}),
    )
    .await
  });
  let mut body = Vec::new();
  for sequence in 0..3 {
    let event = chunk(&run, &mut events, "hub-client-response").await;
    assert_eq!(event["sequence"], sequence);
    body.extend(protocol::decode(event["data"].as_str().unwrap(), MAX_CHUNK).unwrap());
  }
  assert_eq!(request.await.unwrap().unwrap()["status"], 200);
  assert_eq!(
    serde_json::from_slice::<Value>(&body).unwrap()["padding"]
      .as_str()
      .unwrap()
      .len(),
    70_000
  );
  invoke(&run, "hub_client_listen", json!({"connection_id":id}))
    .await
    .unwrap();
  let stream = chunk(&run, &mut events, "hub-client-stream-chunk").await;
  let bytes = protocol::decode(stream["data"].as_str().unwrap(), MAX_CHUNK).unwrap();
  assert!(std::str::from_utf8(&bytes).unwrap().contains("event: session-updated"));
  let open_params = json!({"hub_url":peer.hub,"host_id":HOST_ID,"host_public_key":peer.key});
  services.fail_next_secret_write.store(true, Ordering::SeqCst);
  let reattach_error = invoke(&run, "hub_client_open", open_params.clone()).await.unwrap_err();
  assert!(reattach_error.message.contains("Fixture preference write failed"));
  let snapshot = invoke(&run, "snapshot", json!({})).await.unwrap();
  assert_eq!(snapshot["connections"].as_array().unwrap().len(), 1);
  assert_eq!(snapshot["connections"][0]["connection_id"], id);
  peer.signals.push_event.notify_one();
  let followup = chunk(&run, &mut events, "hub-client-stream-chunk").await;
  assert_eq!(followup["channel_id"], stream["channel_id"]);
  assert_eq!(followup["generation"], stream["generation"]);
  let bytes = protocol::decode(followup["data"].as_str().unwrap(), MAX_CHUNK).unwrap();
  assert!(std::str::from_utf8(&bytes).unwrap().contains("after-reattach"));
  assert_eq!(
    services.next.load(Ordering::SeqCst) - previous_sockets,
    1,
    "concurrent opens, discovery, commands and the live stream share one authenticated socket"
  );
  // Start a workspace without a live listener so only the command can dial the replacement session.
  invoke(&run, "hub_client_close", json!({"connection_id":id}))
    .await
    .unwrap();
  let workspace = invoke(&run, "hub_client_open", open_params).await.unwrap();
  let id = workspace["connection_id"].as_str().unwrap().to_owned();
  // Losing a mutation acknowledgement must never cause guest reconnect to send it again.
  peer.drop_ack.store(true, Ordering::SeqCst);
  assert!(invoke(&run,"hub_client_request",json!({"connection_id":id,"command":"submit_session_input","payload":{"request":{"prompt":"fixture prompt once"}},"response_id":"lost-ack"})).await.unwrap_err().message.contains("not replayed"));
  peer.signals.stall_next_handshake.store(true, Ordering::SeqCst);
  let reader = run.clone();
  let command_id = id.clone();
  let reconnect = tokio::spawn(async move {
    invoke(
      &reader,
      "hub_client_request",
      json!({"connection_id":command_id,"command":"list_sessions","payload":{},"response_id":"stalled-read"}),
    )
    .await
  });
  tokio::time::timeout(Duration::from_secs(5), peer.signals.handshake_stalled.notified())
    .await
    .expect("the command reached the replacement Noise handshake");
  tokio::time::timeout(Duration::from_secs(3), async {
    invoke(&run, "hub_client_close", json!({"connection_id":id}))
      .await
      .unwrap();
    assert!(reconnect.await.unwrap().is_err());
    peer.signals.stalled_socket_closed.notified().await;
  })
  .await
  .expect("disconnect must cancel the command and close the stalled socket");
  assert_eq!(peer.mutations.load(Ordering::SeqCst), 1);

  run.stop().await;
  services.close().await;
  let reopened = RuntimeHandle::start(wasm, Arc::new(Adapter(services.clone())))
    .await
    .unwrap();
  invoke(&reopened, "initialize", json!({})).await.unwrap();
  let after = invoke(&reopened, "hub_client_status", json!({"hub_url":peer.hub}))
    .await
    .unwrap();
  assert_eq!(after["device_public_key"], before["device_public_key"]);
  assert_eq!(after["hosts"].as_array().unwrap().len(), 1);
  assert_eq!(after["hosts"][0]["discovery_group"], json!(discovery_group));
  peer.revoked.store(true, Ordering::SeqCst);
  assert!(
    invoke(
      &reopened,
      "hub_client_open",
      json!({"hub_url":peer.hub,"host_id":HOST_ID,"host_public_key":peer.key})
    )
    .await
    .is_err()
  );
  assert!(
    invoke(&reopened, "snapshot", json!({})).await.unwrap()["connections"]
      .as_array()
      .unwrap()
      .is_empty()
  );
  reopened.stop().await;
  services.close().await;
}

#[tokio::test]
#[ignore = "requires a compiled wasm guest; CI runs this test explicitly"]
async fn interpreted_guest_keeps_scoped_account_login_out_of_the_view_and_machine_storage() {
  let hub_url = "https://hub.example/";
  let hub = json!({"hub_url":hub_url});
  let services = Arc::new(Services::default());
  let wasm = guest();
  let run = RuntimeHandle::start(wasm.clone(), Arc::new(Adapter(services.clone())))
    .await
    .unwrap();
  invoke(&run, "initialize", json!({})).await.unwrap();
  let device = invoke(&run, "hub_client_status", hub.clone()).await.unwrap();
  let old_hub = "https://old-hub.example";
  let old_attempt = json!({"hub_url":old_hub,"attempt_id":"11111111-2222-4333-8444-777777777777"});
  assert_eq!(
    invoke(&run, "hub_account_status", json!({"hub_url":old_hub}))
      .await
      .unwrap(),
    json!({"supported":false}),
  );
  assert!(invoke(&run, "hub_account_login_start", old_attempt).await.is_err());
  let first = json!({"hub_url":hub_url,"attempt_id":"11111111-2222-4333-8444-888888888888"});
  let second = json!({"hub_url":hub_url,"attempt_id":"11111111-2222-4333-8444-999999999999"});
  invoke(&run, "hub_account_login_start", first.clone()).await.unwrap();
  let challenge = invoke(&run, "hub_account_login_start", second.clone()).await.unwrap();
  assert_eq!(
    challenge["verification_url"],
    format!("{}account?user_code=ABCD-EFGH&device=viewer", hub_url)
  );
  assert_eq!(challenge.as_object().unwrap().len(), 4);
  invoke(&run, "hub_account_login_cancel", first.clone()).await.unwrap();
  assert!(invoke(&run, "hub_account_login_poll", first).await.is_err());
  assert_eq!(
    invoke(&run, "hub_account_login_poll", second.clone()).await.unwrap(),
    json!({"interval_ms":5000}),
  );
  services.account_approved.store(true, Ordering::SeqCst);
  let account = json!({"user_id":TEST_ACCOUNT_ID,"username":"personal"});
  assert_eq!(
    invoke(&run, "hub_account_login_poll", second).await.unwrap(),
    json!({"account":account,"interval_ms":0}),
  );
  assert_eq!(
    invoke(&run, "hub_account_machines", json!({"hub_url":old_hub}))
      .await
      .unwrap(),
    json!({"machines":[]}),
  );
  run.stop().await;
  let reopened = RuntimeHandle::start(wasm, Arc::new(Adapter(services.clone())))
    .await
    .unwrap();
  invoke(&reopened, "initialize", json!({})).await.unwrap();
  assert_eq!(
    invoke(&reopened, "hub_account_status", hub.clone()).await.unwrap(),
    json!({"supported":true,"account":account}),
  );
  assert_eq!(
    invoke(&reopened, "hub_account_machines", hub.clone()).await.unwrap(),
    json!({"account":account,"machines":[
      {"host_id":HOST_ID,"machine_address":"personal:fixture","name":"Named machine","online":true},
    ]}),
  );
  invoke(&reopened, "hub_account_logout", hub.clone()).await.unwrap();
  assert!(!services.account_approved.load(Ordering::SeqCst));
  assert_eq!(
    invoke(&reopened, "hub_account_status", hub.clone()).await.unwrap(),
    json!({"supported":true}),
  );
  assert_eq!(invoke(&reopened, "hub_client_status", hub).await.unwrap(), device);
  reopened.stop().await;
}
