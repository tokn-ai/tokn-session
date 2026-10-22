use crate::{Fixture, TestResult, WireRecord};
use axum::{
  Json, Router,
  body::{Body, to_bytes},
  extract::{
    FromRequestParts, State, WebSocketUpgrade,
    ws::{Message, WebSocket},
  },
  http::{Request, StatusCode},
  response::{IntoResponse, Response, Sse, sse::Event},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::{
  collections::{BTreeMap, HashSet},
  convert::Infallible,
  sync::{Arc, Mutex},
};
use tokio::{
  sync::{Notify, watch},
  task::JoinHandle,
};

struct Progress {
  consumed: HashSet<usize>,
  bindings: BTreeMap<String, String>,
  errors: Vec<String>,
}
struct Inner {
  fixture: Fixture,
  progress: Mutex<Progress>,
  notify: Notify,
}
pub struct ReplayServer {
  url: String,
  inner: Arc<Inner>,
  stop: watch::Sender<bool>,
  task: JoinHandle<()>,
}
impl ReplayServer {
  pub async fn start(fixture: Fixture) -> TestResult<Self> {
    if fixture.schema_version != 1 || fixture.upstream_revision.len() != 40 {
      return Err("Missing native fixture provenance".into());
    }
    let inner = Arc::new(Inner {
      fixture,
      progress: Mutex::new(Progress {
        consumed: HashSet::new(),
        bindings: BTreeMap::new(),
        errors: Vec::new(),
      }),
      notify: Notify::new(),
    });
    let app = Router::new().fallback(replay).with_state(inner.clone());
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
    Ok(Self { url, inner, stop, task })
  }
  pub fn url(&self) -> String {
    self.url.clone()
  }
  /// Every captured mutation must be exercised. Read polling can vary with
  /// scheduling; repeated reads never substitute for a missing write.
  pub fn assert_consumed(&self) -> TestResult {
    let progress = self.inner.progress.lock().unwrap();
    if !progress.errors.is_empty() {
      return Err(progress.errors.join("\n").into());
    }
    let missing = self
      .inner
      .fixture
      .records
      .iter()
      .enumerate()
      .filter(|(index, record)| is_mutation(record) && !progress.consumed.contains(index))
      .map(|(_, record)| format!("{record:?}"))
      .collect::<Vec<_>>();
    if missing.is_empty() {
      Ok(())
    } else {
      Err(format!("Unreplayed native mutations: {}", missing.join("\n")).into())
    }
  }
  pub async fn close(&self) {
    let _ = self.stop.send(true);
    self.task.abort();
  }
}
impl Drop for ReplayServer {
  fn drop(&mut self) {
    let _ = self.stop.send(true);
    self.task.abort();
  }
}

fn is_mutation(record: &WireRecord) -> bool {
  let WireRecord::Http { method, path, .. } = record else {
    return false;
  };
  method != "GET"
    && !["host.describe", "session.list", "llm.models", "session.get"]
      .iter()
      .any(|read| path.ends_with(read))
}

fn bind(expected: &Value, actual: &Value, bindings: &mut BTreeMap<String, String>, field: Option<&str>) -> bool {
  match (expected, actual) {
    (Value::String(expected), Value::String(actual)) => {
      let variable = matches!(field, Some("rpcId" | "id" | "request_id")) && uuid::Uuid::parse_str(expected).is_ok();
      if !variable {
        return expected == actual;
      }
      if uuid::Uuid::parse_str(actual).is_err() {
        return false;
      }
      if let Some(bound) = bindings.get(expected) {
        return bound == actual;
      }
      if bindings
        .iter()
        .any(|(captured, bound)| captured != expected && bound == actual)
      {
        return false;
      }
      // Identity bindings also reserve their actual UUID, so a later distinct
      // captured request cannot reuse it without detection.
      bindings.insert(expected.clone(), actual.clone());
      true
    }
    (Value::Object(expected), Value::Object(actual)) => {
      expected.len() == actual.len()
        && expected.iter().all(|(key, value)| {
          actual
            .get(key)
            .is_some_and(|actual| bind(value, actual, bindings, Some(key.as_str())))
        })
    }
    (Value::Array(expected), Value::Array(actual)) => {
      expected.len() == actual.len()
        && expected
          .iter()
          .zip(actual)
          .all(|(expected, actual)| bind(expected, actual, bindings, field))
    }
    _ => expected == actual,
  }
}
fn substitute(mut value: Value, bindings: &BTreeMap<String, String>) -> Value {
  fn visit(value: &mut Value, bindings: &BTreeMap<String, String>) {
    match value {
      Value::String(text) => {
        if let Some(replacement) = bindings.get(text) {
          *text = replacement.clone();
        }
      }
      Value::Array(values) => {
        for value in values {
          visit(value, bindings);
        }
      }
      Value::Object(values) => {
        for value in values.values_mut() {
          visit(value, bindings);
        }
      }
      _ => {}
    }
  }
  visit(&mut value, bindings);
  value
}
fn same_path(expected: &str, actual: &str) -> bool {
  let url = |path: &str| reqwest::Url::parse(&format!("http://fixture/{}", path.trim_start_matches('/'))).unwrap();
  let expected = url(expected);
  let actual = url(actual);
  expected.path() == actual.path()
    && expected.query_pairs().collect::<BTreeMap<_, _>>() == actual.query_pairs().collect::<BTreeMap<_, _>>()
}

async fn replay(State(inner): State<Arc<Inner>>, mut request: Request<Body>) -> Response {
  let path = request
    .uri()
    .path_and_query()
    .map(|value| value.as_str())
    .unwrap_or("/")
    .to_owned();
  if request
    .headers()
    .get("upgrade")
    .is_some_and(|value| value == "websocket")
  {
    let (mut parts, _) = request.into_parts();
    return match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
      Ok(upgrade) => upgrade
        .on_upgrade(move |socket| replay_ws(inner, path, socket))
        .into_response(),
      Err(error) => error.into_response(),
    };
  }
  let method = request.method().to_string();
  if inner
    .fixture
    .records
    .iter()
    .any(|record| matches!(record, WireRecord::Event {channel,..} if same_path(channel, &path)))
    && method == "GET"
  {
    let stream = async_stream::stream! {
      for index in event_indices(&inner, &path) {
        wait_until_ready(&inner, index).await;
        let WireRecord::Event { body, .. } = &inner.fixture.records[index] else { unreachable!() };
        let body = substitute(body.clone(), &inner.progress.lock().unwrap().bindings);
        yield Ok::<_, Infallible>(Event::default().data(body.to_string()));
      }
      std::future::pending::<()>().await;
    };
    return Sse::new(stream).into_response();
  }
  let bytes = match to_bytes(std::mem::replace(request.body_mut(), Body::empty()), 16 * 1024 * 1024).await {
    Ok(bytes) => bytes,
    Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
  };
  let body: Value = if bytes.is_empty() {
    Value::Null
  } else {
    match serde_json::from_slice(&bytes) {
      Ok(body) => body,
      Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    }
  };
  let mut progress = inner.progress.lock().unwrap();
  let ready = |index: usize, consumed: &HashSet<usize>| {
    inner.fixture.records[..index]
      .iter()
      .enumerate()
      .all(|(prior, record)| !is_mutation(record) || consumed.contains(&prior))
  };
  let mut found = None;
  for (index, record) in inner.fixture.records.iter().enumerate() {
    let WireRecord::Http {
      method: expected_method,
      path: expected_path,
      request: expected,
      status,
      response,
    } = record
    else {
      continue;
    };
    if expected_method != &method
      || !same_path(expected_path, &path)
      || progress.consumed.contains(&index)
      || !ready(index, &progress.consumed)
    {
      continue;
    }
    let mut bindings = progress.bindings.clone();
    if bind(expected, &body, &mut bindings, None) {
      found = Some((index, *status, response.clone(), bindings));
      break;
    }
  }
  // State polling is observational, and may repeat the latest native result
  // once every captured read of that shape has been consumed.
  if found.is_none() {
    for (index, record) in inner.fixture.records.iter().enumerate().rev() {
      let WireRecord::Http {
        method: expected_method,
        path: expected_path,
        request: expected,
        status,
        response,
      } = record
      else {
        continue;
      };
      if is_mutation(record)
        || expected_method != &method
        || !same_path(expected_path, &path)
        || !ready(index, &progress.consumed)
      {
        continue;
      }
      let mut bindings = progress.bindings.clone();
      if bind(expected, &body, &mut bindings, None) {
        found = Some((index, *status, response.clone(), bindings));
        break;
      }
    }
  }
  let Some((index, status, response, bindings)) = found else {
    let error = format!("Unexpected replay request: {method} {path} {body}");
    progress.errors.push(error.clone());
    return (StatusCode::CONFLICT, error).into_response();
  };
  progress.bindings = bindings;
  progress.consumed.insert(index);
  let response = substitute(response, &progress.bindings);
  drop(progress);
  inner.notify.notify_waiters();
  let status = StatusCode::from_u16(status).unwrap();
  if status == StatusCode::NO_CONTENT {
    status.into_response()
  } else {
    (status, Json(response)).into_response()
  }
}

fn event_indices(inner: &Inner, path: &str) -> Vec<usize> {
  inner
    .fixture
    .records
    .iter()
    .enumerate()
    .filter_map(|(index, record)| {
      matches!(record, WireRecord::Event { channel, .. } if same_path(channel, path)).then_some(index)
    })
    .collect()
}
async fn wait_until_ready(inner: &Inner, index: usize) {
  loop {
    let notify = inner.notify.notified();
    tokio::pin!(notify);
    notify.as_mut().enable();
    let ready = {
      let progress = inner.progress.lock().unwrap();
      inner.fixture.records[..index]
        .iter()
        .enumerate()
        .all(|(index, record)| !is_mutation(record) || progress.consumed.contains(&index))
    };
    if ready {
      return;
    }
    notify.await;
  }
}
async fn replay_ws(inner: Arc<Inner>, path: String, socket: WebSocket) {
  let (mut sink, mut incoming) = socket.split();
  let events = async {
    for index in event_indices(&inner, &path) {
      wait_until_ready(&inner, index).await;
      let WireRecord::Event { body, .. } = &inner.fixture.records[index] else {
        unreachable!()
      };
      let value = substitute(body.clone(), &inner.progress.lock().unwrap().bindings);
      if sink.send(Message::Text(value.to_string().into())).await.is_err() {
        return;
      }
    }
    std::future::pending::<()>().await;
  };
  let receive = async {
    while let Some(message) = incoming.next().await {
      if !matches!(message, Ok(Message::Ping(_) | Message::Pong(_) | Message::Text(_))) {
        break;
      }
    }
  };
  tokio::select! { _ = events => {}, _ = receive => {} }
}
