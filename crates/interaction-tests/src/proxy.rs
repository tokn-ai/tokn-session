use crate::{TestResult, Trace, WireRecord};
use axum::{
  Router,
  body::{Body, to_bytes},
  extract::{
    State, WebSocketUpgrade,
    ws::{Message as AxumMessage, WebSocket},
  },
  http::{Request, StatusCode},
  response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::{sync::watch, task::JoinHandle};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use tokn_opencode_rpc_protocol::SseDecoder;

const MAX_BODY: usize = 16 * 1024 * 1024;
struct Inner {
  upstream: Url,
  client: reqwest::Client,
  trace: Trace,
}
pub struct RecordingProxy {
  url: String,
  stop: watch::Sender<bool>,
  task: JoinHandle<()>,
}
impl RecordingProxy {
  pub async fn start(upstream: &str, trace: Trace) -> TestResult<Self> {
    let mut upstream = Url::parse(upstream)?;
    if !matches!(upstream.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")) || upstream.scheme() != "http" {
      return Err("Native recording only permits loopback HTTP upstreams".into());
    }
    if !upstream.path().ends_with('/') {
      upstream.set_path(&format!("{}/", upstream.path()));
    }
    let inner = Arc::new(Inner {
      upstream,
      client: reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .build()?,
      trace,
    });
    let app = Router::new().fallback(forward).with_state(inner);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/", listener.local_addr()?);
    let (stop, mut receiver) = watch::channel(false);
    let task = tokio::spawn(async move {
      let _ = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
          let _ = receiver.changed().await;
        })
        .await;
    });
    Ok(Self { url, stop, task })
  }
  pub fn url(&self) -> String {
    self.url.clone()
  }
  pub async fn close(&self) {
    let _ = self.stop.send(true);
    self.task.abort();
  }
}
impl Drop for RecordingProxy {
  fn drop(&mut self) {
    let _ = self.stop.send(true);
    self.task.abort();
  }
}

async fn forward(State(inner): State<Arc<Inner>>, request: Request<Body>) -> Response {
  match forward_inner(inner, request).await {
    Ok(response) => response,
    Err(error) => (StatusCode::BAD_GATEWAY, format!("Native recording proxy: {error}")).into_response(),
  }
}

async fn forward_inner(inner: Arc<Inner>, mut request: Request<Body>) -> TestResult<Response> {
  let path = request
    .uri()
    .path_and_query()
    .map(|path| path.as_str())
    .unwrap_or("/")
    .to_owned();
  let url = inner.upstream.join(path.trim_start_matches('/'))?;
  let authorization = request.headers().get("authorization").cloned();
  if request
    .headers()
    .get("upgrade")
    .is_some_and(|value| value == "websocket")
  {
    let (parts, _) = request.into_parts();
    let mut parts = parts;
    let upgrade = WebSocketUpgrade::from_request_parts(&mut parts, &())
      .await
      .map_err(|error| error.to_string())?;
    let mut ws_url = url;
    ws_url.set_scheme("ws").map_err(|_| "Invalid native WebSocket URL")?;
    let mut upstream_request = ws_url.as_str().into_client_request()?;
    if let Some(value) = authorization {
      upstream_request.headers_mut().insert("authorization", value);
    }
    let (upstream, _) = tokio_tungstenite::connect_async(upstream_request).await?;
    return Ok(
      upgrade
        .on_upgrade(move |client| bridge_ws(client, upstream, inner.trace.clone(), path))
        .into_response(),
    );
  }
  let method = request.method().clone();
  let content_type = request.headers().get("content-type").cloned();
  let body = to_bytes(std::mem::replace(request.body_mut(), Body::empty()), MAX_BODY).await?;
  let request_json = if body.is_empty() {
    Value::Null
  } else {
    serde_json::from_slice(&body)?
  };
  let slot = inner
    .trace
    .begin_http(method.to_string(), path.clone(), request_json.clone());
  let mut outgoing = inner.client.request(method.clone(), url).body(body);
  if let Some(value) = authorization {
    outgoing = outgoing.header("authorization", value);
  }
  if let Some(value) = content_type {
    outgoing = outgoing.header("content-type", value);
  }
  let response = outgoing.send().await?;
  let status = response.status();
  let content_type = response.headers().get("content-type").cloned();
  if content_type
    .as_ref()
    .is_some_and(|value| value.to_str().unwrap_or("").starts_with("text/event-stream"))
  {
    let trace = inner.trace.clone();
    let channel = path.clone();
    trace.complete_http(slot, status.as_u16(), Value::Null);
    let stream: std::pin::Pin<Box<dyn futures_util::Stream<Item = TestResult<axum::body::Bytes>> + Send>> = Box::pin(
      async_stream::try_stream! {
        let mut decoder = SseDecoder::new(MAX_BODY);
        let mut incoming = response.bytes_stream();
        while let Some(bytes) = incoming.next().await {
          let bytes = bytes?;
          for frame in decoder.push(&bytes)? {
            trace.push(WireRecord::Event { channel: channel.clone(), body: serde_json::from_str(&frame).unwrap_or(Value::String(frame)) });
          }
          yield bytes;
        }
      },
    );
    return Ok(
      Response::builder()
        .status(status)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))?,
    );
  }
  let mut bytes = Vec::new();
  let mut stream = response.bytes_stream();
  while let Some(chunk) = stream.next().await {
    let chunk = chunk?;
    if bytes.len().saturating_add(chunk.len()) > MAX_BODY {
      return Err("Native response exceeds capture limit".into());
    }
    bytes.extend_from_slice(&chunk);
  }
  let response_json = if bytes.is_empty() {
    Value::Null
  } else {
    serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
  };
  inner.trace.complete_http(slot, status.as_u16(), response_json);
  let mut response = Response::builder().status(status);
  if let Some(value) = content_type {
    response = response.header("content-type", value);
  }
  Ok(response.body(Body::from(bytes))?)
}

async fn bridge_ws(
  client: WebSocket,
  upstream: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
  trace: Trace,
  channel: String,
) {
  let (mut client_sink, mut client_stream) = client.split();
  let (mut upstream_sink, mut upstream_stream) = upstream.split();
  loop {
    tokio::select! {
      message = upstream_stream.next() => {
        let Some(Ok(message)) = message else { break; };
        let message = match message {
          Message::Text(text) => { trace.push(WireRecord::Event { channel: channel.clone(), body: serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text.to_string())) }); AxumMessage::Text(text.to_string().into()) },
          Message::Binary(bytes) => AxumMessage::Binary(bytes),
          Message::Ping(bytes) => AxumMessage::Ping(bytes), Message::Pong(bytes) => AxumMessage::Pong(bytes),
          Message::Close(_) => break,
          _ => continue,
        };
        if client_sink.send(message).await.is_err() { break; }
      }
      message = client_stream.next() => {
        let Some(Ok(message)) = message else { break; };
        let message = match message {
          AxumMessage::Text(text) => Message::Text(text.to_string().into()),
          AxumMessage::Binary(bytes) => Message::Binary(bytes),
          AxumMessage::Ping(bytes) => Message::Ping(bytes), AxumMessage::Pong(bytes) => Message::Pong(bytes),
          AxumMessage::Close(_) => break,
        };
        if upstream_sink.send(message).await.is_err() { break; }
      }
    }
  }
  let _ = upstream_sink.close().await;
  let _ = client_sink.close().await;
}

use axum::extract::FromRequestParts;
