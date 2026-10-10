//! The public Hub boundary: owner authentication, host selection, and forwarding.
use crate::{
  auth::AuthState,
  store::{MachineRecord, NamespaceError, Store},
  tunnel::HubTunnels,
};
use axum::{
  Json, Router,
  body::{Body, Bytes},
  extract::{DefaultBodyLimit, Path, Request, State, WebSocketUpgrade, rejection::JsonRejection},
  http::{HeaderMap, HeaderValue, Method, StatusCode, header},
  middleware::{self, Next},
  response::{IntoResponse, Response},
  routing::{any, get, post},
};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokn_hub_client_core::address::ResolvedMachine;
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

#[derive(Clone)]
pub struct HubState {
  pub auth: AuthState,
  pub store: Store,
  pub tunnels: HubTunnels,
}

impl HubState {
  pub fn new(store: Store, public_url: &str) -> Result<Self, String> {
    Ok(Self {
      auth: AuthState::new(store.clone(), public_url)?,
      tunnels: HubTunnels::new(store.clone()),
      store,
    })
  }
}

type ApiError = (StatusCode, Json<Value>);

fn error(status: StatusCode, message: impl Into<String>) -> ApiError {
  (status, Json(json!({ "error": message.into() })))
}

pub fn router(state: HubState) -> Router {
  let protected = Router::new()
    .route("/hub/v1/hosts", get(hosts))
    .route("/hub/v1/hosts/{host_id}", axum::routing::delete(revoke_host))
    .route("/hub/v1/namespaces", get(namespaces).post(create_namespace))
    .route(
      "/hub/v1/namespaces/{username}/machines/{machine_name}",
      post(bind_machine),
    )
    .route("/hub/v1/enrollments", get(enrollments))
    .route("/hub/v1/enrollments/approve", post(approve_host))
    .route("/hosts/{host_id}/api/v1/{command}", any(proxy))
    .layer(middleware::from_fn_with_state(state.clone(), authorize));
  let auth = state.auth.router();
  Router::new()
    .merge(protected)
    .route("/hub/v1/tunnel", get(tunnel))
    .route("/hub/v1/secure/{host_id}", get(secure_tunnel))
    .route("/hub/v1/resolve/{username}/{machine_name}", get(resolve_machine))
    .route("/hub/v1/health", get(|| async { Json(json!({ "version": 1 })) }))
    .route("/hub", any(not_found))
    .route("/hub/{*path}", any(not_found))
    .route("/hosts", any(not_found))
    .route("/hosts/{host_id}", any(not_found))
    .route("/hosts/{host_id}/{*path}", any(not_found))
    .route("/api", any(not_found))
    .route("/api/{*path}", any(not_found))
    .with_state(state)
    .merge(auth)
    .layer(DefaultBodyLimit::max(1024 * 1024))
    .layer(middleware::from_fn(response_headers))
}

pub fn with_web_ui(app: Router, web_root: PathBuf) -> Result<Router, String> {
  let index = web_root.join("index.html");
  if !index.is_file() {
    return Err(format!(
      "Viewer web UI is missing at {}. Run `pnpm --dir apps/viewer build` or pass --api-only.",
      index.display()
    ));
  }
  let files = ServeDir::new(web_root).fallback(ServeFile::new(index));
  Ok(
    app
      .fallback(move |request: Request| {
        let files = files.clone();
        async move {
          // Cover malformed and trailing-slash API paths as well as normal misses.
          // A failed API lookup must never turn into a successful HTML response.
          if matches!(
            request.uri().path().split('/').nth(1),
            Some("hub" | "hosts" | "api" | "paired")
          ) {
            not_found().await.into_response()
          } else {
            files
              .oneshot(request)
              .await
              .expect("static service is infallible")
              .into_response()
          }
        }
      })
      .layer(middleware::from_fn(response_headers)),
  )
}

async fn not_found() -> ApiError {
  error(StatusCode::NOT_FOUND, "Unknown Hub API route")
}

async fn response_headers(request: Request, next: Next) -> Response {
  // Native passkey completion is a cross-origin form navigation to the app's
  // one-shot loopback callback. Preserve the document Origin for its exact
  // origin check without disclosing paths or the fragment-held ceremony.
  let referrer_policy = if request.uri().path() == "/passkey" {
    "origin"
  } else {
    "no-referrer"
  };
  let mut response = next.run(request).await;
  response
    .headers_mut()
    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
  response
    .headers_mut()
    .insert(header::REFERRER_POLICY, HeaderValue::from_static(referrer_policy));
  response
    .headers_mut()
    .insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
  response
    .headers_mut()
    .insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
  response
}

async fn authorize(State(state): State<HubState>, request: Request, next: Next) -> Response {
  if let Err(error) = state.auth.check_optional_origin(request.headers()) {
    return error.into_response();
  }
  let principal = match state.auth.authorize(request.headers()) {
    Ok(principal) => principal,
    Err(error) => return error.into_response(),
  };
  let cancellation = principal.cancellation;
  let response = tokio::select! {
    biased;
    _ = cancellation.cancelled() => return error(StatusCode::UNAUTHORIZED, "Hub session expired").into_response(),
    response = next.run(request) => response,
  };
  // Logout and session expiry also terminate an already-open SSE subscription.
  let (parts, body) = response.into_parts();
  let mut incoming = body.into_data_stream();
  let stream = async_stream::stream! {
    loop {
      let chunk = tokio::select! {
        biased;
        _ = cancellation.cancelled() => break,
        chunk = incoming.next() => chunk,
      };
      match chunk {
        Some(chunk) => yield chunk,
        None => break,
      }
    }
  };
  Response::from_parts(parts, Body::from_stream(stream))
}

async fn hosts(State(state): State<HubState>) -> Result<Json<Value>, ApiError> {
  let hosts = state.store.host_catalog().map_err(internal)?;
  Ok(Json(json!({
    "hosts": hosts.into_iter().map(|host| {
      let mut entry = json!({
        "online": state.tunnels.online(&host.host_id),
        "secure_only": host.secure_only || state.tunnels.secure_only(&host.host_id),
        "access": state.tunnels.access(&host.host_id).unwrap_or(host.access),
        "host_id": host.host_id,
        "name": host.name,
      });
      if let Some(address) = host.machine_address {
        entry["machine_address"] = json!(address);
      }
      entry
    }).collect::<Vec<_>>()
  })))
}

fn namespace_error(failure: NamespaceError) -> ApiError {
  match failure {
    NamespaceError::Invalid(message) => error(StatusCode::BAD_REQUEST, message),
    NamespaceError::Conflict(message) => error(StatusCode::CONFLICT, message),
    NamespaceError::NotFound(message) => error(StatusCode::NOT_FOUND, message),
    NamespaceError::Internal(detail) => internal(detail),
  }
}

async fn namespaces(State(state): State<HubState>) -> Result<Json<Value>, ApiError> {
  Ok(Json(
    json!({"namespaces": state.store.namespaces().map_err(namespace_error)?}),
  ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateNamespaceRequest {
  username: String,
}

async fn create_namespace(
  State(state): State<HubState>,
  request: Result<Json<CreateNamespaceRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
  let Json(request) = request.map_err(|_| error(StatusCode::BAD_REQUEST, "Expected a username object"))?;
  let namespace = state
    .store
    .create_namespace(&request.username)
    .map_err(namespace_error)?;
  Ok(Json(json!({"username": namespace.username})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BindMachineRequest {
  host_id: String,
}

async fn bind_machine(
  State(state): State<HubState>,
  Path((username, machine_name)): Path<(String, String)>,
  request: Result<Json<BindMachineRequest>, JsonRejection>,
) -> Result<Json<ResolvedMachine>, ApiError> {
  let Json(request) = request.map_err(|_| error(StatusCode::BAD_REQUEST, "Expected a host_id object"))?;
  let machine = state
    .store
    .bind_machine(&username, &machine_name, &request.host_id)
    .map_err(namespace_error)?;
  Ok(Json(resolved_machine(&state, machine)))
}

fn resolved_machine(state: &HubState, machine: MachineRecord) -> ResolvedMachine {
  ResolvedMachine {
    online: state.tunnels.online(&machine.host_id),
    host_id: machine.host_id,
    machine_address: machine.machine_address,
    name: machine.name,
  }
}

async fn resolve_machine(
  State(state): State<HubState>,
  Path((username, machine_name)): Path<(String, String)>,
  headers: HeaderMap,
) -> Response {
  if let Err(error) = state.auth.check_optional_origin(&headers) {
    return error.into_response();
  }
  match state.store.resolve_machine(&username, &machine_name) {
    Ok(Some(machine)) => Json(resolved_machine(&state, machine)).into_response(),
    Ok(None) => error(StatusCode::NOT_FOUND, "Unknown machine address").into_response(),
    Err(error) => namespace_error(error).into_response(),
  }
}

async fn enrollments(State(state): State<HubState>) -> Json<Value> {
  Json(json!({ "enrollments": state.tunnels.pending() }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApproveRequest {
  pairing_code: String,
}

async fn approve_host(
  State(state): State<HubState>,
  Json(request): Json<ApproveRequest>,
) -> Result<Json<Value>, ApiError> {
  let host = state
    .tunnels
    .approve(&request.pairing_code)
    .map_err(|message| error(StatusCode::BAD_REQUEST, message))?;
  Ok(Json(json!({ "host_id": host.host_id })))
}

async fn revoke_host(State(state): State<HubState>, Path(host_id): Path<String>) -> Result<StatusCode, ApiError> {
  state.tunnels.revoke(&host_id).map_err(internal)?;
  Ok(StatusCode::NO_CONTENT)
}

async fn tunnel(State(state): State<HubState>, headers: HeaderMap, websocket: WebSocketUpgrade) -> Response {
  // Only the native connector uses this endpoint; browser clients use passkeys
  // and the authenticated host routes, never anonymous WebSocket enrollment.
  if headers.contains_key(header::ORIGIN) {
    return error(StatusCode::FORBIDDEN, "Browser tunnel connections are not supported").into_response();
  }
  websocket
    .max_message_size(2 * 1024 * 1024)
    .max_frame_size(2 * 1024 * 1024)
    .on_upgrade(move |socket| async move { state.tunnels.handle_socket(socket).await })
}

async fn secure_tunnel(
  State(state): State<HubState>,
  Path(host_id): Path<String>,
  headers: HeaderMap,
  websocket: WebSocketUpgrade,
) -> Response {
  // The configured UI is a trusted code publisher. Browsers authenticate and
  // decrypt at their endpoint; this relay receives only opaque binary records.
  // Native clients omit Origin. Browser admission is exact-origin only and does
  // not confer host authorization, which remains enforced inside the channel.
  if let Err(error) = state.auth.check_optional_origin(&headers) {
    return error.into_response();
  }
  if !state.tunnels.online(&host_id) {
    return error(StatusCode::BAD_GATEWAY, "Host is offline").into_response();
  }
  let permit = match state.tunnels.reserve_secure_channel() {
    Ok(permit) => permit,
    Err(message) => return error(StatusCode::TOO_MANY_REQUESTS, message).into_response(),
  };
  websocket
    .max_message_size(crate::protocol::MAX_SECURE_RECORD)
    .max_frame_size(crate::protocol::MAX_SECURE_RECORD)
    .on_upgrade(move |socket| async move { state.tunnels.handle_secure_socket(&host_id, socket, permit).await })
}

async fn proxy(
  State(state): State<HubState>,
  Path((host_id, command)): Path<(String, String)>,
  method: Method,
  body: Bytes,
) -> Response {
  // The connector independently enforces its local maximum permission.
  if state.tunnels.secure_only(&host_id) {
    return error(
      StatusCode::FORBIDDEN,
      "This host requires the installed encrypted client",
    )
    .into_response();
  }
  // Advertising unavailable input also keeps the viewer's composer disabled.
  if method == Method::POST
    && command == "get_session_input_status"
    && state.tunnels.online(&host_id)
    && state.tunnels.access(&host_id).as_deref() != Some("control")
  {
    return Json(json!({
      "available": false,
      "message": "This host allows viewing only",
      "max_length": 0,
    }))
    .into_response();
  }
  let path = format!("/api/v1/{command}");
  match state.tunnels.proxy(&host_id, method, &path, body).await {
    Ok(response) => response,
    Err(failure) => error(failure.status, failure.message).into_response(),
  }
}

fn internal(_detail: String) -> ApiError {
  error(StatusCode::INTERNAL_SERVER_ERROR, "Hub storage operation failed")
}

#[cfg(test)]
mod tests;
