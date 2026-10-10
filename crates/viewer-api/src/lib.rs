//! HTTP adapter and static browser host for viewer-core.
use axum::{
  Json, Router,
  extract::{DefaultBodyLimit, Path, Request, State},
  http::{HeaderValue, Method, StatusCode, header},
  middleware::{self, Next},
  response::{
    IntoResponse, Response, Sse,
    sse::{Event, KeepAlive},
  },
  routing::{any, get, post},
};
use serde_json::{Value, json};
use std::{convert::Infallible, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{Semaphore, broadcast};
use tokio_util::sync::CancellationToken;
use tokn_viewer_core::{ViewerService, runtime::ViewerEvent};
use tower_http::{
  cors::CorsLayer,
  services::{ServeDir, ServeFile},
};

mod shared;

#[derive(Clone)]
struct ApiState {
  service: ViewerService,
  events: broadcast::Sender<ViewerEvent>,
  token: Option<Arc<String>>,
  requests: Arc<Semaphore>,
  subscribers: Arc<Semaphore>,
  shutdown: CancellationToken,
}

type ApiError = (StatusCode, Json<Value>);
fn error(status: StatusCode, message: impl Into<String>) -> ApiError {
  (status, Json(json!({"error": message.into()})))
}

pub fn router(
  service: ViewerService,
  events: broadcast::Sender<ViewerEvent>,
  token: Option<String>,
  origins: Vec<HeaderValue>,
  shutdown: CancellationToken,
) -> Router {
  let state = ApiState {
    service,
    events,
    token: token.map(Arc::new),
    requests: Arc::new(Semaphore::new(16)),
    subscribers: Arc::new(Semaphore::new(32)),
    shutdown,
  };
  Router::new()
    .route("/api/v1/health", get(|| async { Json(json!({"version": 1})) }))
    .route("/api/v1/events", get(event_stream))
    .route(
      "/api/v1/shared",
      post(shared::command).layer(DefaultBodyLimit::max(1024 * 1024 + 64 * 1024)),
    )
    .route("/api/v1/{command}", post(command))
    .route("/api", any(api_not_found))
    .route("/api/{*path}", any(api_not_found))
    .layer(middleware::from_fn_with_state(state.clone(), authenticate))
    .layer(DefaultBodyLimit::max(1024 * 1024))
    .layer(
      CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
    )
    .with_state(state)
}

/// Serve a Vite production build without allowing its SPA fallback to hide
/// misspelled API routes.
pub fn with_web_ui(app: Router, web_root: PathBuf) -> Result<Router, String> {
  let index = web_root.join("index.html");
  if !index.is_file() {
    return Err(format!(
      "Viewer web UI is missing at {}. Run `pnpm --dir apps/viewer build` or pass --web-root.",
      index.display()
    ));
  }
  Ok(app.fallback_service(ServeDir::new(web_root).fallback(ServeFile::new(index))))
}

async fn api_not_found() -> ApiError {
  error(StatusCode::NOT_FOUND, "Unknown viewer API route")
}

async fn authenticate(State(state): State<ApiState>, request: Request, next: Next) -> Response {
  if let Some(token) = &state.token {
    let supplied = request
      .headers()
      .get(header::AUTHORIZATION)
      .and_then(|v| v.to_str().ok())
      .and_then(|v| v.strip_prefix("Bearer "));
    if !supplied.is_some_and(|value| same_token(value.as_bytes(), token.as_bytes())) {
      return error(StatusCode::UNAUTHORIZED, "Invalid viewer API token").into_response();
    }
  }
  next.run(request).await
}

fn same_token(left: &[u8], right: &[u8]) -> bool {
  let mut difference = left.len() ^ right.len();
  for (index, byte) in right.iter().enumerate() {
    difference |= usize::from(left.get(index).copied().unwrap_or(0) ^ byte);
  }
  difference == 0
}

async fn command(
  State(state): State<ApiState>,
  Path(command): Path<String>,
  Json(payload): Json<Value>,
) -> Result<Json<Value>, ApiError> {
  let permit = state
    .requests
    .clone()
    .try_acquire_owned()
    .map_err(|_| error(StatusCode::TOO_MANY_REQUESTS, "Viewer is busy; retry shortly"))?;
  // Live input uses bounded async sockets. Target validation is shared with
  // desktop in viewer-core; dropping this HTTP request never resends input.
  if matches!(command.as_str(), "get_session_input_status" | "submit_session_input") {
    let _permit = permit;
    let request = payload.get("request").cloned().unwrap_or_else(|| json!({}));
    let response = match command.as_str() {
      "get_session_input_status" => {
        let request = serde_json::from_value(request).map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
        serde_json::to_value(
          state
            .service
            .get_session_input_status(request)
            .await
            .map_err(|e| error(StatusCode::BAD_REQUEST, e))?,
        )
      }
      _ => {
        let request = serde_json::from_value(request).map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
        serde_json::to_value(
          state
            .service
            .submit_session_input(request)
            .await
            .map_err(|e| error(StatusCode::BAD_REQUEST, e))?,
        )
      }
    };
    return response
      .map(Json)
      .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Could not encode input response"));
  }
  // The permit lives with blocking work even when the HTTP caller disconnects.
  tokio::task::spawn_blocking(move || {
    let _permit = permit;
    dispatch(&state.service, &command, payload).map(Json)
  })
  .await
  .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Viewer request failed"))?
}

fn dispatch(service: &ViewerService, command: &str, payload: Value) -> Result<Value, ApiError> {
  let request = payload.get("request").cloned().unwrap_or_else(|| json!({}));
  if matches!(
    command,
    "list_session_children"
      | "load_event_page"
      | "load_session_updates"
      | "load_event_detail"
      | "load_trajectory_event_page"
      | "acknowledge_session_attention"
  ) {
    let field = if command == "list_session_children" {
      "parent_session_key"
    } else {
      "session_key"
    };
    let key = request
      .get(field)
      .and_then(Value::as_str)
      .ok_or_else(|| error(StatusCode::BAD_REQUEST, "Missing session key"))?;
    service.validate_session_key(key).map_err(|_| {
      error(
        StatusCode::NOT_FOUND,
        "Session is not available in this machine's catalog",
      )
    })?;
  }
  macro_rules! call {
    ($method:ident) => {{
      let request = serde_json::from_value(request).map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
      let result = service
        .$method(request)
        .map_err(|e| error(StatusCode::BAD_REQUEST, e))?;
      serde_json::to_value(result)
    }};
  }
  let result = match command {
    "list_sessions" => call!(list_sessions),
    "list_session_children" => call!(list_session_children),
    "load_event_page" => call!(load_event_page),
    "load_session_updates" => call!(load_session_updates),
    "update_session_view" => call!(update_session_view),
    "load_event_detail" => call!(load_event_detail),
    "load_trajectory_event_page" => call!(load_trajectory_event_page),
    "acknowledge_session_attention" => call!(acknowledge_session_attention),
    "get_session_index_progress" => serde_json::to_value(service.session_index_progress()),
    "retry_session_index" => serde_json::to_value(
      service
        .request_session_index_retry()
        .map_err(|e| error(StatusCode::BAD_REQUEST, e))?,
    ),
    "get_relay_status" => serde_json::to_value(service.relay.status()),
    _ => return Err(error(StatusCode::NOT_FOUND, "Unknown viewer command")),
  };
  result.map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Could not encode viewer response"))
}

async fn event_stream(State(state): State<ApiState>) -> Result<impl IntoResponse, ApiError> {
  let permit = state
    .subscribers
    .clone()
    .try_acquire_owned()
    .map_err(|_| error(StatusCode::TOO_MANY_REQUESTS, "Too many viewer subscriptions"))?;
  let mut receiver = state.events.subscribe();
  let stream = async_stream::stream! {
    let _permit = permit;
    yield Ok::<_, Infallible>(Event::default().event("ready").data("{}"));
    loop {
      let received = tokio::select! { biased; _ = state.shutdown.cancelled() => break, event = receiver.recv() => event };
      match received {
        Ok(event) => yield Ok(Event::default().event(event.event).data(event.payload.to_string())),
        Err(broadcast::error::RecvError::Lagged(_)) => {
          // Reconnect forces a fresh catalog/timeline; never apply an incomplete sequence.
          break;
        }
        Err(broadcast::error::RecvError::Closed) => break,
      }
    }
  };
  Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

#[cfg(test)]
mod tests {
  use super::*;
  use axum::body::{Body, to_bytes};
  use tower::ServiceExt;

  #[tokio::test]
  async fn serves_the_browser_without_exposing_or_masking_api_routes() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("index.html"), "<main>viewer shell</main>").unwrap();
    std::fs::write(root.path().join("app.js"), "console.log('viewer')").unwrap();
    let data = tempfile::tempdir().unwrap();
    let service = ViewerService::native(data.path().join("index.sqlite")).unwrap();
    let (events, _) = broadcast::channel(16);
    let app = with_web_ui(
      router(
        service,
        events,
        Some("test-secret".into()),
        vec![],
        CancellationToken::new(),
      ),
      root.path().to_path_buf(),
    )
    .unwrap();

    for path in ["/", "/sessions/selected", "/app.js"] {
      let response = app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
      assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
    let response = app
      .oneshot(
        Request::builder()
          .method(Method::POST)
          .uri("/api/v1/missing")
          .header(header::AUTHORIZATION, "Bearer test-secret")
          .header(header::CONTENT_TYPE, "application/json")
          .body(Body::from("{}"))
          .unwrap(),
      )
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
      response.headers().get(header::CONTENT_TYPE).unwrap(),
      "application/json"
    );
  }

  #[tokio::test]
  async fn protects_http_and_events_and_rejects_forged_keys() {
    let root = tempfile::tempdir().unwrap();
    let service = ViewerService::native(root.path().join("index.sqlite")).unwrap();
    let (events, _) = broadcast::channel(16);
    let app = router(
      service,
      events,
      Some("test-secret".into()),
      vec![],
      CancellationToken::new(),
    );
    for path in ["health", "events"] {
      let response = app
        .clone()
        .oneshot(
          Request::builder()
            .uri(format!("/api/v1/{path}"))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
      assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let response = app
      .clone()
      .oneshot(
        Request::builder()
          .uri("/api/v1/health")
          .header(header::AUTHORIZATION, "Bearer test-secret")
          .body(Body::empty())
          .unwrap(),
      )
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
      .oneshot(
        Request::builder()
          .method(Method::POST)
          .uri("/api/v1/load_event_page")
          .header(header::AUTHORIZATION, "Bearer test-secret")
          .header(header::CONTENT_TYPE, "application/json")
          .body(Body::from(r#"{"request":{"session_key":"../../private-file"}}"#))
          .unwrap(),
      )
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = to_bytes(response.into_body(), 1024).await.unwrap();
    assert!(!String::from_utf8_lossy(&body).contains("private-file"));
  }

  #[tokio::test]
  async fn session_view_rejects_undiscovered_selected_and_candidate_keys() {
    let root = tempfile::tempdir().unwrap();
    let service = ViewerService::native(root.path().join("index.sqlite")).unwrap();
    let (events, _) = broadcast::channel(16);
    let app = router(service, events, None, vec![], CancellationToken::new());
    for (selected, candidates) in [
      (json!("../../private-file"), json!([])),
      (Value::Null, json!(["../../private-file"])),
    ] {
      let response = app
        .clone()
        .oneshot(
          Request::builder()
            .method(Method::POST)
            .uri("/api/v1/update_session_view")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
              json!({"request":{
                "view_id":"test-view", "revision":1,
                "session_key":selected, "candidate_session_keys":candidates
              }})
              .to_string(),
            ))
            .unwrap(),
        )
        .await
        .unwrap();
      assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
  }

  #[tokio::test]
  async fn http_matches_core_and_sse_reports_live_updates() {
    use http_body_util::BodyExt;
    use tokn_session_relay::{ProviderRoot, RelayConfig};
    use tokn_viewer_core::{
      model::ListSessionsRequest,
      relay::{RelayMode, RelaySettings},
    };
    let root = tempfile::tempdir().unwrap();
    let sessions = root.path().join("sessions");
    std::fs::create_dir(&sessions).unwrap();
    std::fs::write(
      sessions.join("one.jsonl"),
      concat!(
        "{\"type\":\"session\",\"id\":\"one\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"message\":{\"role\":\"user\",\"content\":\"hello remotely\"}}\n"
      ),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("tcp://{}", listener.local_addr().unwrap());
    let config = RelayConfig::new(vec![ProviderRoot::new(
      serde_json::from_str("\"pi\"").unwrap(),
      sessions,
    )]);
    let server = tokio::spawn(tokn_viewer_core::service_server::serve_listener(listener, config));
    let service = ViewerService::native(root.path().join("index.sqlite")).unwrap();
    service
      .relay
      .configure(RelaySettings {
        mode: RelayMode::External,
        endpoint,
        ..Default::default()
      })
      .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
      while !service.relay.has_catalog() {
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
    })
    .await
    .unwrap();
    let direct = serde_json::to_value(service.list_sessions(ListSessionsRequest::default()).unwrap()).unwrap();
    let (events, _) = broadcast::channel(2);
    let app = router(
      service.clone(),
      events.clone(),
      None,
      vec!["http://localhost:1437".parse().unwrap()],
      CancellationToken::new(),
    );
    let response = app
      .clone()
      .oneshot(
        Request::builder()
          .method(Method::POST)
          .uri("/api/v1/list_sessions")
          .header(header::CONTENT_TYPE, "application/json")
          .header(header::ORIGIN, "http://localhost:1437")
          .body(Body::from(r#"{"request":{}}"#))
          .unwrap(),
      )
      .await
      .unwrap();
    assert_eq!(
      response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
      "http://localhost:1437"
    );
    assert_eq!(
      serde_json::from_slice::<Value>(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap(),
      direct
    );
    let key = direct["sessions"][0]["session_key"].as_str().unwrap();
    for request in [
      json!({"view_id":"remote-view", "revision":1, "session_key":key, "candidate_session_keys":[key]}),
      json!({"view_id":"remote-view", "revision":2, "session_key":null, "candidate_session_keys":[]}),
    ] {
      let response = app
        .clone()
        .oneshot(
          Request::builder()
            .method(Method::POST)
            .uri("/api/v1/update_session_view")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"request":request}).to_string()))
            .unwrap(),
        )
        .await
        .unwrap();
      assert_eq!(response.status(), StatusCode::OK);
    }
    let response = app
      .clone()
      .oneshot(
        Request::builder()
          .method(Method::POST)
          .uri("/api/v1/load_event_page")
          .header(header::CONTENT_TYPE, "application/json")
          .body(Body::from(json!({"request":{"session_key":key}}).to_string()))
          .unwrap(),
      )
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let page: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    assert!(page["total_events"].as_u64().unwrap() > 0);
    let response = app
      .clone()
      .oneshot(Request::builder().uri("/api/v1/events").body(Body::empty()).unwrap())
      .await
      .unwrap();
    let mut body = response.into_body();
    let ready = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert!(String::from_utf8_lossy(&ready).contains("event: ready"));
    events
      .send(ViewerEvent {
        event: "relay-changed".into(),
        payload: json!({"session_key":key,"reset":false}),
      })
      .unwrap();
    let update = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert!(String::from_utf8_lossy(&update).contains("relay-changed"));
    for _ in 0..4 {
      events
        .send(ViewerEvent {
          event: "relay-changed".into(),
          payload: json!({}),
        })
        .unwrap();
    }
    assert!(
      body.frame().await.is_none(),
      "lagged SSE closes so clients refresh on reconnect"
    );
    let response = app
      .oneshot(
        Request::builder()
          .method(Method::POST)
          .uri("/api/v1/list_sessions")
          .header(header::CONTENT_TYPE, "application/json")
          .body(Body::from(" ".repeat(1024 * 1024 + 1)))
          .unwrap(),
      )
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    service.relay.shutdown().await;
    server.abort();
  }
}
