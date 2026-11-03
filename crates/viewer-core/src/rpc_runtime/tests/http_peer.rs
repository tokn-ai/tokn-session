//! Small native HTTP/event peers for testing viewer factory dispatch. Full
//! native payload compatibility is independently covered by adapter replays.
use super::*;
use std::collections::BTreeMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;

pub(crate) type DshRequestHandler = Arc<dyn Fn(&str, &Value) -> Option<Value> + Send + Sync>;

pub(crate) struct HttpPeer {
  pub endpoint: String,
  pub requests: Arc<Mutex<Vec<String>>>,
  pub catalog_result: Arc<Mutex<Value>>,
  pub active_downlinks: Arc<AtomicUsize>,
  response_gates: Arc<Mutex<BTreeMap<String, Arc<Semaphore>>>>,
  task: tokio::task::JoinHandle<()>,
}

struct DownlinkLease(Arc<AtomicUsize>);

impl Drop for DownlinkLease {
  fn drop(&mut self) {
    self.0.fetch_sub(1, Ordering::SeqCst);
  }
}

impl HttpPeer {
  pub async fn start(provider: Provider, directory: String) -> Self {
    Self::start_with_handler(provider, directory, None).await
  }

  /// Handles additional native DSH routes using their complete request envelope.
  /// Returning None closes that request without an acknowledgement.
  pub async fn start_dsh_with_handler(directory: String, handler: DshRequestHandler) -> Self {
    Self::start_with_handler(Provider::Dsh, directory, Some(handler)).await
  }

  /// Hold a route's response after its native handler has run, without blocking
  /// other requests or the event stream. Each released permit allows one reply.
  pub fn block_response(&self, route: &str) -> Arc<Semaphore> {
    let gate = Arc::new(Semaphore::new(0));
    self.response_gates.lock().unwrap().insert(route.into(), gate.clone());
    gate
  }

  async fn start_with_handler(provider: Provider, directory: String, handler: Option<DshRequestHandler>) -> Self {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let catalog_result = Arc::new(Mutex::new(json!({"ok":true,"value":{"groups":[]}})));
    let catalog = catalog_result.clone();
    let active_downlinks = Arc::new(AtomicUsize::new(0));
    let downlinks = active_downlinks.clone();
    let response_gates = Arc::new(Mutex::new(BTreeMap::<String, Arc<Semaphore>>::new()));
    let gates = response_gates.clone();
    let task = tokio::spawn(async move {
      let mut peers = JoinSet::new();
      loop {
        let Ok((mut socket, _)) = listener.accept().await else {
          break;
        };
        let directory = directory.clone();
        let recorded = recorded.clone();
        let catalog = catalog.clone();
        let downlinks = downlinks.clone();
        let handler = handler.clone();
        let gates = gates.clone();
        peers.spawn(async move {
          let mut preview = [0u8; 8192];
          loop {
            let Ok(count) = socket.peek(&mut preview).await else {
              return;
            };
            if count == 0 || count == preview.len() {
              return;
            }
            if preview[..count].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
              break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
          }
          let preview = String::from_utf8_lossy(&preview);
          let route = preview
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .to_owned();
          let path = route.split('?').next().unwrap().to_owned();
          recorded.lock().unwrap().push(path.clone());
          if path.starts_with("/api/events.") {
            let Ok(mut ws) = tokio_tungstenite::accept_async(socket).await else {
              return;
            };
            downlinks.fetch_add(1, Ordering::SeqCst);
            let _downlink = DownlinkLease(downlinks);
            while let Some(Ok(frame)) = ws.next().await {
              match frame {
                tokio_tungstenite::tungstenite::Message::Ping(bytes) => {
                  if ws
                    .send(tokio_tungstenite::tungstenite::Message::Pong(bytes))
                    .await
                    .is_err()
                  {
                    break;
                  }
                }
                tokio_tungstenite::tungstenite::Message::Close(_) => break,
                _ => {}
              }
            }
            return;
          }
          let mut bytes = Vec::new();
          let header_end = loop {
            let mut chunk = [0u8; 4096];
            let Ok(count) = socket.read(&mut chunk).await else {
              return;
            };
            if count == 0 {
              return;
            }
            bytes.extend_from_slice(&chunk[..count]);
            if bytes.len() > 64 * 1024 {
              return;
            }
            if let Some(index) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
              break index + 4;
            }
          };
          let headers = String::from_utf8_lossy(&bytes[..header_end]);
          let content_length: usize = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim().parse().unwrap())
            .unwrap_or(0);
          assert!(content_length < 64 * 1024);
          while bytes.len() < header_end + content_length {
            let mut chunk = [0u8; 4096];
            let Ok(count) = socket.read(&mut chunk).await else {
              return;
            };
            if count == 0 {
              return;
            }
            bytes.extend_from_slice(&chunk[..count]);
          }
          if path == "/api/event" {
            let _ = socket
              .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n")
              .await;
            // Keep the native stream alive until the adapter closes it.
            let _ = socket.read(&mut [0u8; 1]).await;
            return;
          }
          let request: Value =
            serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap_or(Value::Null);
          let result = match provider {
            Provider::OpenCode => match path.as_str() {
              "/api/health" => json!({"healthy":true}),
              "/global/health" => json!({"version":"viewer-test-opencode"}),
              "/doc" => {
                let mut document = json!({"paths":{}});
                for route in [
                  "/api/event",
                  "/api/session/active",
                  "/api/model",
                  "/api/agent",
                  "/api/provider",
                  "/api/command",
                  "/api/session/{sessionID}/question",
                  "/api/session/{sessionID}/permission",
                ] {
                  document["paths"][route] = json!({"get":{}});
                }
                document["paths"]["/api/session/{sessionID}/prompt"] = json!({"post":{}});
                document
              }
              "/api/model" | "/api/command" | "/api/agent" | "/api/provider" => {
                json!({"location":{"directory":directory},"data":[]})
              }
              route => panic!("unexpected OpenCode native route {route}"),
            },
            Provider::Dsh => {
              let result = match path.as_str() {
                "/api/host.describe" => {
                  json!({"ok":true,"value":{"version":"viewer-test-dsh","cwd":directory,"attachedSessions":0,"canOpenPath":false}})
                }
                "/api/llm.models" => catalog.lock().unwrap().clone(),
                route => match &handler {
                  Some(handler) => match handler(route, &request) {
                    Some(result) => result,
                    None => return,
                  },
                  None => panic!("unexpected DSH native route {route}"),
                },
              };
              json!({"type":"server-response","rpcId":request["rpcId"],"result":result})
            }
            _ => unreachable!(),
          };
          let gate = gates.lock().unwrap().get(&path).cloned();
          if let Some(gate) = gate {
            gate.acquire_owned().await.unwrap().forget();
          }
          let body = result.to_string();
          let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
          );
          let _ = socket.write_all(head.as_bytes()).await;
          let _ = socket.write_all(body.as_bytes()).await;
        });
      }
    });
    Self {
      endpoint,
      requests,
      catalog_result,
      active_downlinks,
      response_gates,
      task,
    }
  }
}

impl Drop for HttpPeer {
  fn drop(&mut self) {
    self.task.abort();
  }
}
