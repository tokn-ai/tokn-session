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
use tokn_viewer_core::session_list::SESSION_LIST_CAPACITY_ERROR;
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

const VIEWER_PROTOCOL_VERSION: u32 = 4;

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
    .route(
      "/api/v1/health",
      get(|| async { Json(json!({"version": VIEWER_PROTOCOL_VERSION})) }),
    )
    .route("/api/v1/events", get(general_events).post(scoped_events))
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
  if matches!(
    command.as_str(),
    "get_session_input_status"
      | "submit_session_input"
      | "get_session_question_status"
      | "submit_session_question_answers"
      | "upload_image_chunk"
      | "get_session_model_status"
      | "list_session_models"
      | "get_session_control"
      | "list_session_interactions"
      | "respond_session_interaction"
      | "set_session_control"
      | "cancel_session_turn"
      | "get_rpc_runtime_profiles"
      | "validate_rpc_runtime_profile"
      | "save_rpc_runtime_profile"
      | "delete_rpc_runtime_profile"
      | "list_rpc_runtimes"
      | "start_rpc_runtime"
      | "close_rpc_runtime"
  ) {
    let _permit = permit;
    let request = payload.get("request").cloned().unwrap_or_else(|| json!({}));
    macro_rules! rpc_call {
      ($method:ident) => {{
        let request =
          serde_json::from_value(request).map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid runtime request"))?;
        serde_json::to_value(
          state
            .service
            .$method(request)
            .await
            .map_err(|e| error(StatusCode::BAD_REQUEST, e))?,
        )
      }};
    }
    let response = match command.as_str() {
      "get_session_control" => rpc_call!(get_session_control),
      "list_session_interactions" => rpc_call!(list_session_interactions),
      "respond_session_interaction" => rpc_call!(respond_session_interaction),
      "set_session_control" => rpc_call!(set_session_control),
      "cancel_session_turn" => rpc_call!(cancel_session_turn),
      "get_rpc_runtime_profiles" => serde_json::to_value(
        state
          .service
          .get_rpc_runtime_profiles()
          .await
          .map_err(|e| error(StatusCode::BAD_REQUEST, e))?,
      ),
      "validate_rpc_runtime_profile" => rpc_call!(validate_rpc_runtime_profile),
      "save_rpc_runtime_profile" => rpc_call!(save_rpc_runtime_profile),
      "delete_rpc_runtime_profile" => rpc_call!(delete_rpc_runtime_profile),
      "list_rpc_runtimes" => serde_json::to_value(
        state
          .service
          .list_rpc_runtimes()
          .await
          .map_err(|e| error(StatusCode::BAD_REQUEST, e))?,
      ),
      "start_rpc_runtime" => rpc_call!(start_rpc_runtime),
      "close_rpc_runtime" => rpc_call!(close_rpc_runtime),
      "get_session_model_status" => {
        let request = serde_json::from_value(request).map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
        serde_json::to_value(
          state
            .service
            .get_session_model_status(request)
            .await
            .map_err(|e| error(StatusCode::BAD_REQUEST, e))?,
        )
      }
      "list_session_models" => {
        let request = serde_json::from_value(request).map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
        serde_json::to_value(
          state
            .service
            .list_session_models(request)
            .await
            .map_err(|e| error(StatusCode::BAD_REQUEST, e))?,
        )
      }
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
      "upload_image_chunk" => {
        let request: tokn_viewer_core::images::UploadImageChunkRequest =
          serde_json::from_value(request).map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
        validate_remote_image_target(&request.target)?;
        serde_json::to_value(
          state
            .service
            .upload_image_chunk(request)
            .await
            .map_err(|e| error(StatusCode::BAD_REQUEST, e))?,
        )
      }
      "get_session_question_status" => {
        let request = serde_json::from_value(request).map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
        serde_json::to_value(
          state
            .service
            .get_session_question_status(request)
            .await
            .map_err(|e| error(StatusCode::BAD_REQUEST, e))?,
        )
      }
      "submit_session_question_answers" => {
        let request = serde_json::from_value(request).map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
        serde_json::to_value(
          state
            .service
            .submit_session_question_answers(request)
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

fn validate_remote_image_target(target: &tokn_viewer_core::images::ImageTarget) -> Result<(), ApiError> {
  match target {
    tokn_viewer_core::images::ImageTarget::Session { .. } => Ok(()),
    tokn_viewer_core::images::ImageTarget::Project { .. } => Err(error(
      StatusCode::FORBIDDEN,
      "Project image uploads are only available in the local desktop app",
    )),
  }
}

fn dispatch(service: &ViewerService, command: &str, payload: Value) -> Result<Value, ApiError> {
  let request = payload.get("request").cloned().unwrap_or_else(|| json!({}));
  if matches!(
    command,
    "list_session_children"
      | "load_event_page"
      | "inspect_session_event"
      | "load_event_detail"
      | "load_trajectory_event_page"
      | "acknowledge_session_attention"
      | "load_session_image"
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
  let command_error = |message: String| {
    let status = if command == "open_session_list" && message == SESSION_LIST_CAPACITY_ERROR {
      StatusCode::TOO_MANY_REQUESTS
    } else {
      StatusCode::BAD_REQUEST
    };
    error(status, message)
  };
  macro_rules! call {
    ($method:ident) => {{
      let request = serde_json::from_value(request).map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
      let result = service.$method(request).map_err(command_error)?;
      serde_json::to_value(result)
    }};
  }
  let result = match command {
    "list_sessions" => call!(list_sessions),
    "list_session_children" => call!(list_session_children),
    "open_session_list" => call!(open_session_list),
    "release_session_list" => call!(release_session_list),
    "load_event_page" => call!(load_event_page),
    "open_session" => call!(open_session),
    "release_session" => call!(release_session),
    "subscribe_background_updates" => call!(subscribe_background),
    "load_session_details" => call!(load_session_details),
    "inspect_session_event" => call!(inspect_session_event),
    "load_session_image" => call!(load_session_image),
    "release_image_upload" => {
      let request: tokn_viewer_core::images::ReleaseImageUploadRequest =
        serde_json::from_value(request).map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
      validate_remote_image_target(&request.target)?;
      serde_json::to_value(
        service
          .release_image_upload(request)
          .map_err(|e| error(StatusCode::BAD_REQUEST, e))?,
      )
    }
    "renew_session_subscriptions" | "renew_session_lists" => {
      let ids: Vec<String> = serde_json::from_value(payload.get("ids").cloned().unwrap_or_else(|| json!([])))
        .map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
      let renewal = if command == "renew_session_lists" {
        service.renew_session_lists(&ids)
      } else {
        if ids.len() > 24 {
          return Err(error(StatusCode::BAD_REQUEST, "Too many subscriptions"));
        }
        service.renew_session_subscriptions(&ids)
      }
      .map_err(|e| error(StatusCode::BAD_REQUEST, e))?;
      serde_json::to_value(renewal)
    }
    "renew_background_subscriptions" => {
      let ids: Vec<String> = serde_json::from_value(payload.get("ids").cloned().unwrap_or_else(|| json!([])))
        .map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
      if ids.len() > 24 {
        return Err(error(StatusCode::BAD_REQUEST, "Too many background subscriptions"));
      }
      let result = service
        .renew_background_subscriptions(&ids)
        .map_err(|e| error(StatusCode::BAD_REQUEST, e))?;
      Ok(result)
    }
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
    "get_machine_settings" => serde_json::to_value(
      service
        .get_machine_settings()
        .map_err(|e| error(StatusCode::FORBIDDEN, e))?,
    ),
    _ => return Err(error(StatusCode::NOT_FOUND, "Unknown viewer command")),
  };
  result.map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Could not encode viewer response"))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamSubscription {
  subscription_prefix: String,
}

async fn general_events(State(state): State<ApiState>) -> Result<impl IntoResponse, ApiError> {
  event_stream(state, None)
}

async fn scoped_events(
  State(state): State<ApiState>,
  Json(request): Json<StreamSubscription>,
) -> Result<impl IntoResponse, ApiError> {
  event_stream(state, Some(request.into_prefix()?))
}

impl StreamSubscription {
  fn into_prefix(self) -> Result<String, ApiError> {
    let prefix = self.subscription_prefix;
    if prefix.is_empty()
      || prefix.len() > 64
      || !prefix
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
      return Err(error(StatusCode::BAD_REQUEST, "Invalid subscription prefix"));
    }
    Ok(prefix)
  }
}

fn stream_event(mut event: ViewerEvent, prefix: Option<&str>) -> Option<ViewerEvent> {
  if matches!(
    event.event.as_str(),
    "session-updated" | "background-session-updated" | "session-list-updated" | "session-subscription-invalidated"
  ) {
    let id = event.payload["subscription_id"].as_str()?;
    if id.split_once(':')?.0 != prefix? {
      return None;
    }
    if event.event == "session-subscription-invalidated"
      || (event.event == "session-updated" && event.payload["snapshot"] == true)
    {
      event.event = "session-subscriptions-lost".into();
      event.payload = json!({"subscription_ids":[id]});
    }
  }
  Some(event)
}

fn event_stream(state: ApiState, subscription_prefix: Option<String>) -> Result<impl IntoResponse, ApiError> {
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
        Ok(event) => {
          let Some(event) = stream_event(event, subscription_prefix.as_deref()) else { continue };
          yield Ok(Event::default().event(event.event).data(event.payload.to_string()));
        },
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
  async fn image_routes_keep_project_uploads_native_and_validate_read_catalog_scope() {
    let directory = tempfile::tempdir().unwrap();
    let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
    let (events, _) = broadcast::channel(16);
    let app = router(service, events, None, vec![], CancellationToken::new());
    let request = |command: &str, payload: Value| {
      Request::builder()
        .method(Method::POST)
        .uri(format!("/api/v1/{command}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"request": payload}).to_string()))
        .unwrap()
    };
    for (command, payload) in [
      (
        "upload_image_chunk",
        json!({
          "target":{"type":"project", "project_id":"project"}, "upload_id":"upload", "offset":0,
          "total_bytes":1, "mime_type":"image/png", "name":"picture.png", "data_base64":"YQ=="
        }),
      ),
      (
        "release_image_upload",
        json!({"target":{"type":"project", "project_id":"project"}, "upload_id":"upload"}),
      ),
    ] {
      let response = app.clone().oneshot(request(command, payload)).await.unwrap();
      assert_eq!(response.status(), StatusCode::FORBIDDEN);
      let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
      assert!(body["error"].as_str().unwrap().contains("local desktop"));
    }
    let response = app
      .oneshot(request(
        "load_session_image",
        json!({
          "session_key":"outside-catalog", "event_key":"event.v1.0", "image_id":"image.v1.0", "variant":"thumbnail"
        }),
      ))
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
  }

  #[tokio::test]
  async fn runtime_configuration_requires_administrator_auth_and_redacts_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
    let (events, _) = broadcast::channel(16);
    let app = router(
      service,
      events,
      Some("administrator-token".into()),
      vec![],
      CancellationToken::new(),
    );
    let request = |command: &str, payload: Value, authenticated: bool| {
      let mut request = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/v1/{command}"))
        .header(header::CONTENT_TYPE, "application/json");
      if authenticated {
        request = request.header(header::AUTHORIZATION, "Bearer administrator-token");
      }
      request.body(Body::from(payload.to_string())).unwrap()
    };
    for command in [
      "get_rpc_runtime_profiles",
      "validate_rpc_runtime_profile",
      "save_rpc_runtime_profile",
      "delete_rpc_runtime_profile",
      "list_rpc_runtimes",
      "start_rpc_runtime",
      "close_rpc_runtime",
    ] {
      let response = app.clone().oneshot(request(command, json!({}), false)).await.unwrap();
      assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{command}");
    }
    let response = app
      .clone()
      .oneshot(request(
        "save_rpc_runtime_profile",
        json!({"request":{
          "expected_revision":0,
          "profile":{
            "profile_id":null, "name":"Codex endpoint", "provider":"codex", "mode":"attached",
            "executable":null, "arguments":[], "working_directory":null,
            "endpoint":"wss://rpc.example.test/", "environment":null,
            "credentials":{"type":"bearer", "token":"private-runtime-credential"}, "is_default":true
          }
        }}),
        true,
      ))
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let saved: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(saved["profiles"][0]["credential_kind"], "bearer");
    assert!(
      !std::str::from_utf8(&body)
        .unwrap()
        .contains("private-runtime-credential")
    );
    let response = app
      .clone()
      .oneshot(request("get_rpc_runtime_profiles", json!({}), true))
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), saved);
    let response = app
      .oneshot(request(
        "validate_rpc_runtime_profile",
        json!({"request":{
          "credentials":{"type":"private-runtime-credential"}
        }}),
        true,
      ))
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    assert!(
      !std::str::from_utf8(&body)
        .unwrap()
        .contains("private-runtime-credential")
    );
  }

  #[tokio::test]
  async fn machine_settings_requires_host_auth_and_remains_read_only() {
    let directory = tempfile::tempdir().unwrap();
    let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
    let (events, _) = broadcast::channel(16);
    let app = router(
      service,
      events,
      Some("machine-token".into()),
      vec![],
      CancellationToken::new(),
    );
    let request = |authenticated: bool| {
      let mut request = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/get_machine_settings")
        .header(header::CONTENT_TYPE, "application/json");
      if authenticated {
        request = request.header(header::AUTHORIZATION, "Bearer machine-token");
      }
      request
        .body(Body::from(
          r#"{"reader":{"editable":true},"hosting":{"editable":true}}"#,
        ))
        .unwrap()
    };
    let rejected = app.clone().oneshot(request(false)).await.unwrap();
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
    let response = app.oneshot(request(true)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap()).unwrap();
    assert_eq!(value["version"], 1);
    assert_eq!(value["reader"]["editable"], false);
    assert_eq!(value["sources_editable"], false);
    assert_eq!(value["hosting"]["editable"], false);
    assert!(value["reader"]["edit_reason"].is_string());
    assert_eq!(
      value["sources"].as_array().unwrap().len(),
      tokn_session_relay::PROVIDERS.len()
    );
  }

  #[tokio::test]
  async fn subscription_renewal_returns_missing_identities_as_an_object() {
    let root = tempfile::tempdir().unwrap();
    let service = ViewerService::native(root.path().join("index.sqlite")).unwrap();
    let (events, _) = broadcast::channel(16);
    let app = router(service, events, None, vec![], CancellationToken::new());
    let response = app
      .oneshot(
        Request::builder()
          .method(Method::POST)
          .uri("/api/v1/renew_session_subscriptions")
          .header(header::CONTENT_TYPE, "application/json")
          .body(Body::from(json!({"ids":["missing"]}).to_string()))
          .unwrap(),
      )
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024).await.unwrap();
    assert_eq!(
      serde_json::from_slice::<Value>(&body).unwrap(),
      json!({"renewed_ids":[],"missing_ids":["missing"]})
    );
  }

  #[tokio::test]
  async fn background_interest_registers_and_renews_without_loading_a_timeline() {
    let data = tempfile::tempdir().unwrap();
    let service = ViewerService::native(data.path().join("index.sqlite")).unwrap();
    let (events, _) = broadcast::channel(16);
    let app = router(service, events, None, vec![], CancellationToken::new());
    let request = |command: &str, payload: Value| {
      Request::builder()
        .method("POST")
        .uri(format!("/api/v1/{command}"))
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap()
    };
    let response = app
      .clone()
      .oneshot(request(
        "subscribe_background_updates",
        json!({
          "request":{"subscription_id":"sidebar","scope":"all"}
        }),
      ))
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1024).await.unwrap()).unwrap();
    assert_eq!(value["subscription_id"], "sidebar");
    assert_eq!(value["revision"], "0");
    assert!(value.get("items").is_none());
    let response = app
      .clone()
      .oneshot(request("renew_background_subscriptions", json!({"ids":["sidebar"]})))
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1024).await.unwrap()).unwrap();
    assert_eq!(value["renewed"], true);
    let response = app
      .oneshot(request(
        "subscribe_background_updates",
        json!({
          "request":{"subscription_id":"sidebar","scope":"all","unsubscribe":true}
        }),
      ))
      .await
      .unwrap();
    let value: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1024).await.unwrap()).unwrap();
    assert_eq!(value["unsubscribed"], true);
  }

  #[tokio::test]
  async fn event_streams_filter_viewer_interests_and_signal_resets_without_sending_history() {
    use http_body_util::BodyExt;
    async fn next_frame(body: &mut Body) -> String {
      let bytes = body.frame().await.unwrap().unwrap().into_data().unwrap();
      String::from_utf8(bytes.to_vec()).unwrap()
    }
    let data = tempfile::tempdir().unwrap();
    let service = ViewerService::native(data.path().join("index.sqlite")).unwrap();
    let (events, _) = broadcast::channel(16);
    let app = router(service, events.clone(), None, vec![], CancellationToken::new());
    let scoped = |prefix: &str| {
      Request::post("/api/v1/events")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"subscription_prefix":prefix}).to_string()))
        .unwrap()
    };
    for invalid in ["", "other:viewer", &"x".repeat(65)] {
      assert_eq!(
        app.clone().oneshot(scoped(invalid)).await.unwrap().status(),
        StatusCode::BAD_REQUEST
      );
    }
    let mut mine = app.clone().oneshot(scoped("viewer")).await.unwrap().into_body();
    let mut other = app.clone().oneshot(scoped("viewer2")).await.unwrap().into_body();
    let mut general = app
      .oneshot(Request::get("/api/v1/events").body(Body::empty()).unwrap())
      .await
      .unwrap()
      .into_body();
    for stream in [&mut mine, &mut other, &mut general] {
      assert!(next_frame(stream).await.starts_with("event: ready\n"));
    }
    for (name, payload) in [
      (
        "session-updated",
        json!({"subscription_id":"viewer2:timeline","snapshot":false}),
      ),
      (
        "background-session-updated",
        json!({"subscription_id":"viewer:sidebar"}),
      ),
      (
        "session-list-updated",
        json!({"subscription_id":"viewer:list","sessions":[{"revision":"b".repeat(64),"session":{"session_key":"one","title":"changed"}}]}),
      ),
      (
        "session-updated",
        json!({"subscription_id":"viewer:timeline","snapshot":true,"items":["x".repeat(65536)]}),
      ),
      ("session-notification", json!({"session_key":"one"})),
    ] {
      events
        .send(ViewerEvent {
          event: name.into(),
          payload,
        })
        .unwrap();
    }
    assert!(next_frame(&mut other).await.starts_with("event: session-updated\n"));
    assert!(
      next_frame(&mut mine)
        .await
        .starts_with("event: background-session-updated\n")
    );
    assert!(next_frame(&mut mine).await.starts_with("event: session-list-updated\n"));
    let reset = next_frame(&mut mine).await;
    assert_eq!(
      reset,
      "event: session-subscriptions-lost\ndata: {\"subscription_ids\":[\"viewer:timeline\"]}\n\n"
    );
    for stream in [&mut mine, &mut other, &mut general] {
      assert!(next_frame(stream).await.starts_with("event: session-notification\n"));
    }
  }

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

  async fn post_json(app: &Router, command: &str, payload: Value) -> Value {
    let response = app
      .clone()
      .oneshot(
        Request::post(format!("/api/v1/{command}"))
          .header(header::CONTENT_TYPE, "application/json")
          .body(Body::from(payload.to_string()))
          .unwrap(),
      )
      .await
      .unwrap();
    let status = response.status();
    let value: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    assert_eq!(status, StatusCode::OK, "{command}: {value}");
    value
  }

  async fn assert_grant_can_resume(app: &Router, session_key: &str) {
    let scoped = |command, payload| {
      json!({
        "principal":"viewer", "session_keys":[session_key], "command":command, "payload":payload
      })
    };
    let open = json!({"session_key":session_key,"subscription_id":"first"});
    let cold = post_json(app, "shared", scoped("open_session", json!({"request":open}))).await;
    assert_eq!(cold["snapshot"], true);
    assert!(!cold["items"].as_array().unwrap().is_empty());
    assert_eq!(cold["subscription_id"], "first");
    let hash = cold["state_hash"].as_str().unwrap();
    assert_eq!(hash.len(), 64);
    let renewal = post_json(
      app,
      "shared",
      scoped("renew_session_subscriptions", json!({"ids":["first","gone"]})),
    )
    .await;
    assert_eq!(renewal, json!({"renewed_ids":["first"],"missing_ids":["gone"]}));
    let release = post_json(app, "shared", scoped("release_session", json!({"request":open}))).await;
    assert_eq!(release, json!({"subscription_id":"first","unsubscribed":true}));
    let mut next = json!({"session_key":session_key,"subscription_id":"second","resume":{
      "subscription_id":"first","generation":cold["generation"],"revision":cold["revision"],"state_hash":cold["state_hash"]
    }});
    let mut divergent = next.clone();
    divergent["subscription_id"] = json!("divergent");
    divergent["resume"]["state_hash"] = json!(if hash.starts_with('0') {
      "1".repeat(64)
    } else {
      "0".repeat(64)
    });
    let fresh = post_json(app, "shared", scoped("open_session", json!({"request":divergent}))).await;
    assert_eq!(fresh["snapshot"], true);
    assert!(fresh["base_state_hash"].is_null());
    let warm = post_json(app, "shared", scoped("open_session", json!({"request":next}))).await;
    assert_eq!(warm["snapshot"], false);
    assert_eq!(warm["base_revision"], cold["revision"]);
    assert_eq!(warm["base_state_hash"], cold["state_hash"]);
    assert_eq!(warm["state_hash"], cold["state_hash"]);
    assert_eq!(warm["subscription_id"], "second");
    assert_eq!(warm["items"], json!([]));
    // The same session key and revision from an independent viewer cannot
    // establish continuity across the privileged and shared projections.
    next["resume"]["subscription_id"] = json!("second");
    let owner = post_json(app, "open_session", json!({"request":next})).await;
    assert_eq!(owner["snapshot"], true);
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
    assert_grant_can_resume(&app, key).await;
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
