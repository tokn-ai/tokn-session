//! Small native HTTP/event peers for testing viewer factory dispatch. Full
//! native payload compatibility is independently covered by adapter replays.
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) struct HttpPeer {
  pub endpoint: String,
  pub requests: Arc<Mutex<Vec<String>>>,
  task: tokio::task::JoinHandle<()>,
}

impl HttpPeer {
  pub async fn start(provider: Provider, directory: String) -> Self {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let task = tokio::spawn(async move {
      let mut peers = JoinSet::new();
      loop {
        let Ok((mut socket, _)) = listener.accept().await else {
          break;
        };
        let directory = directory.clone();
        let recorded = recorded.clone();
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
              let value = match path.as_str() {
                "/api/host.describe" => {
                  json!({"version":"viewer-test-dsh","cwd":directory,"attachedSessions":0,"canOpenPath":false})
                }
                "/api/llm.models" => json!({"groups":[]}),
                route => panic!("unexpected DSH native route {route}"),
              };
              json!({"type":"server-response","rpcId":request["rpcId"],"result":{"ok":true,"value":value}})
            }
            _ => unreachable!(),
          };
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
      task,
    }
  }
}

impl Drop for HttpPeer {
  fn drop(&mut self) {
    self.task.abort();
  }
}
