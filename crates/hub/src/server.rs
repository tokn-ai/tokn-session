//! The public Hub boundary for accounts, machine discovery, and encrypted tunnels.
use crate::{
  account_host::{ClaimRequest, RenameRequest},
  accounts::{Accounts, MACHINES_REGISTER},
  auth::AuthState,
  protocol,
  store::{MachineRecord, NamespaceError, Store},
  tunnel::HubTunnels,
};
use axum::{
  Json, Router,
  extract::{DefaultBodyLimit, Path, Request, State, WebSocketUpgrade, rejection::JsonRejection},
  http::{HeaderMap, HeaderValue, StatusCode, header},
  middleware::{self, Next},
  response::{IntoResponse, Response},
  routing::{any, get, post},
};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokn_hub_client_core::address::ResolvedMachine;
use tokn_hub_client_core::presence::{
  CONNECTOR_VERSION_HEADER, HUB_VERSION_HEADER, PRESENCE_VERSIONS_HEADER, PresenceRequest, PresenceResult,
  valid_software_version,
};
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

#[derive(Clone)]
pub struct HubState {
  pub auth: AuthState,
  pub store: Store,
  pub tunnels: HubTunnels,
  accounts: Accounts,
  public_origin: String,
}

impl HubState {
  pub fn new(store: Store, public_url: &str) -> Result<Self, String> {
    let auth = AuthState::new(store.clone(), public_url)?;
    let accounts = Accounts::new(store.clone(), public_url)?;
    let public_origin =
      crate::onboarding::canonical_hub(&url::Url::parse(public_url).map_err(|_| "Invalid Hub public URL")?)?;
    Ok(Self {
      auth,
      tunnels: HubTunnels::new(store.clone()),
      accounts,
      public_origin,
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
    .layer(middleware::from_fn_with_state(state.clone(), authorize));
  let auth = state.auth.router();
  let accounts = state.accounts.router();
  Router::new()
    .merge(protected)
    .route("/hub/v1/tunnel", get(tunnel))
    .route("/hub/v1/secure/{host_id}", get(secure_tunnel))
    .route("/hub/v1/resolve/{username}/{machine_name}", get(resolve_machine))
    .route("/hub/v1/account/machines", get(account_machines))
    .route(
      "/hub/v1/account/machines/claim",
      post(claim_account_machine).layer(DefaultBodyLimit::max(4096)),
    )
    .route(
      "/hub/v1/account/machines/rename",
      post(rename_account_machine).layer(DefaultBodyLimit::max(4096)),
    )
    .route("/hub/v1/discovery/{namespace}/{secret_digest}", get(discover_machines))
    .route(
      "/hub/v1/presence",
      post(machine_presence).layer(DefaultBodyLimit::max(8 * 1024)),
    )
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
    .merge(accounts)
    .layer(DefaultBodyLimit::max(1024 * 1024))
    .layer(middleware::from_fn(response_headers))
}

async fn account_machines(State(state): State<HubState>, headers: HeaderMap) -> Response {
  if let Err(error) = state.auth.check_optional_origin(&headers) {
    return error.into_response();
  }
  let principal = match state.accounts.authenticate_machine_reader(&headers).await {
    Ok(principal) => principal,
    Err(error) => return error.into_response(),
  };
  let store = state.store.clone();
  match tokio::task::spawn_blocking(move || store.account_machines(&principal.user_id)).await {
    Ok(Ok(machines)) => Json(json!({
      "machines": machines.into_iter().map(|machine| resolved_machine(&state, machine)).collect::<Vec<_>>()
    }))
    .into_response(),
    _ => error(StatusCode::INTERNAL_SERVER_ERROR, "Could not load account machines").into_response(),
  }
}

async fn claim_account_machine(
  State(state): State<HubState>,
  headers: HeaderMap,
  Json(claim): Json<ClaimRequest>,
) -> Response {
  if let Err(error) = state.auth.check_optional_origin(&headers) {
    return error.into_response();
  }
  let principal = match state.accounts.authenticate(&headers, MACHINES_REGISTER).await {
    Ok(principal) => principal,
    Err(error) => return error.into_response(),
  };
  if !protocol::valid_host_uuid(&claim.host_id)
    || claim.name.trim().is_empty()
    || claim.name.len() > 128
    || claim.name.chars().any(char::is_control)
    || tokn_hub_client_core::address::parse_machine_address(&format!("{}:{}", principal.username, claim.machine_name))
      .is_err()
  {
    return error(StatusCode::BAD_REQUEST, "Invalid machine identity or name").into_response();
  }
  if let Err(message) = claim.verify(&state.public_origin, &principal.user_id) {
    return error(StatusCode::FORBIDDEN, message).into_response();
  }
  let record = crate::store::HostRecord {
    host_id: claim.host_id,
    public_key: claim.public_key,
    name: claim.name,
    access: if claim.allow_control { "control" } else { "view" }.into(),
  };
  let result = tokio::task::spawn_blocking(move || {
    state
      .store
      .claim_account_host(record, &principal.user_id, &claim.machine_name)
  })
  .await;
  match result {
    Ok(Ok(assignment)) => Json(assignment).into_response(),
    Ok(Err(message))
      if matches!(
        message.as_str(),
        crate::store::ADDRESS_TAKEN
          | crate::store::HOST_CLAIMED
          | "Host UUID registration is revoked or belongs to a different identity"
          | "Host UUID is registered to a different identity"
      ) =>
    {
      error(StatusCode::CONFLICT, message).into_response()
    }
    Ok(Err(message)) if message.starts_with("New host registration rate limit reached") => error(
      StatusCode::TOO_MANY_REQUESTS,
      "New host registration rate limit reached; retry in one minute",
    )
    .into_response(),
    _ => error(StatusCode::INTERNAL_SERVER_ERROR, "Could not register account machine").into_response(),
  }
}

async fn rename_account_machine(
  State(state): State<HubState>,
  headers: HeaderMap,
  Json(rename): Json<RenameRequest>,
) -> Response {
  if let Err(error) = state.auth.check_optional_origin(&headers) {
    return error.into_response();
  }
  let principal = match state.accounts.authenticate(&headers, MACHINES_REGISTER).await {
    Ok(principal) => principal,
    Err(error) => return error.into_response(),
  };
  if !protocol::valid_host_uuid(&rename.host_id)
    || tokn_hub_client_core::address::parse_machine_address(&rename.expected_machine_address).is_err()
    || tokn_hub_client_core::address::parse_machine_address(&format!("{}:{}", principal.username, rename.machine_name))
      .is_err()
  {
    return error(StatusCode::BAD_REQUEST, "Invalid machine identity or name").into_response();
  }
  if let Err(message) = rename.verify(&state.public_origin, &principal.user_id) {
    return error(StatusCode::FORBIDDEN, message).into_response();
  }
  let result = tokio::task::spawn_blocking(move || {
    state.store.rename_account_host(
      &principal.user_id,
      &rename.host_id,
      &rename.public_key,
      &rename.expected_machine_address,
      &rename.machine_name,
    )
  })
  .await;
  match result {
    Ok(Ok(claim)) => Json(claim).into_response(),
    Ok(Err(message)) if message == crate::store::MACHINE_NOT_OWNED => {
      error(StatusCode::FORBIDDEN, message).into_response()
    }
    Ok(Err(message))
      if matches!(
        message.as_str(),
        crate::store::ADDRESS_TAKEN | crate::store::STALE_MACHINE_ADDRESS
      ) =>
    {
      error(StatusCode::CONFLICT, message).into_response()
    }
    _ => error(StatusCode::INTERNAL_SERVER_ERROR, "Could not rename the machine").into_response(),
  }
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
  tokio::select! {
    biased;
    _ = cancellation.cancelled() => error(StatusCode::UNAUTHORIZED, "Hub session expired").into_response(),
    response = next.run(request) => response,
  }
}

async fn hosts(State(state): State<HubState>) -> Result<Json<Value>, ApiError> {
  let hosts = state.store.host_catalog().map_err(internal)?;
  Ok(Json(json!({
    "hosts": hosts.into_iter().map(|host| {
      json!({
        "online": state.tunnels.online(&host.host_id),
        "access": state.tunnels.access(&host.host_id).unwrap_or(host.access),
        "host_id": host.host_id,
        "name": host.name,
        "machine_address": host.machine_address,
      })
    }).collect::<Vec<_>>()
  })))
}

fn namespace_error(failure: NamespaceError) -> ApiError {
  match failure {
    NamespaceError::Invalid(message) => error(StatusCode::BAD_REQUEST, message),
    NamespaceError::Internal(detail) => internal(detail),
  }
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

async fn discover_machines(
  State(state): State<HubState>,
  Path((namespace, secret_digest)): Path<(String, String)>,
  headers: HeaderMap,
) -> Result<Json<tokn_hub_client_core::discovery::DiscoveryResult>, ApiError> {
  use tokn_hub_client_core::discovery::{DISCOVERY_ADDRESSES_HEADER, DiscoveryGroup, DiscoveryResult};
  state
    .auth
    .check_optional_origin(&headers)
    .map_err(|failure| error(failure.status, "Invalid discovery request origin"))?;
  let group = DiscoveryGroup {
    namespace,
    secret_digest,
  };
  group
    .validate()
    .map_err(|message| error(StatusCode::BAD_REQUEST, message))?;
  let mut machines = state
    .store
    .discover_machines(&group)
    .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Could not load discovery group"))?;
  let include_addresses = headers
    .get(DISCOVERY_ADDRESSES_HEADER)
    .is_some_and(|value| value == "1");
  for machine in &mut machines {
    machine.online = state.tunnels.online(&machine.host_id);
    if !include_addresses {
      machine.machine_address = None;
    }
  }
  Ok(Json(DiscoveryResult {
    namespace: group.namespace,
    secret_digest: group.secret_digest,
    machines,
  }))
}

async fn machine_presence(
  State(state): State<HubState>,
  headers: HeaderMap,
  request: Result<Json<PresenceRequest>, JsonRejection>,
) -> Result<Json<PresenceResult>, ApiError> {
  state
    .auth
    .check_optional_origin(&headers)
    .map_err(|failure| error(failure.status, "Invalid presence request origin"))?;
  let Json(request) = request.map_err(|failure| {
    let status = if failure.status() == StatusCode::PAYLOAD_TOO_LARGE {
      StatusCode::PAYLOAD_TOO_LARGE
    } else {
      StatusCode::BAD_REQUEST
    };
    error(status, "Expected a bounded presence request")
  })?;
  request
    .validate()
    .map_err(|message| error(StatusCode::BAD_REQUEST, message))?;
  // Earlier native clients require the original response shape.
  let include_version = headers.get(PRESENCE_VERSIONS_HEADER).is_some_and(|value| value == "1");
  Ok(Json(PresenceResult {
    machines: request
      .host_ids
      .into_iter()
      .map(|host_id| state.tunnels.machine_presence(host_id, include_version))
      .collect(),
  }))
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
  let connector_version = headers
    .get(CONNECTOR_VERSION_HEADER)
    .and_then(|value| value.to_str().ok())
    .filter(|version| valid_software_version(version))
    .map(str::to_owned);
  let mut response = websocket
    .max_message_size(2 * 1024 * 1024)
    .max_frame_size(2 * 1024 * 1024)
    .on_upgrade(move |socket| async move {
      state
        .tunnels
        .handle_socket_with_version(socket, connector_version)
        .await;
    });
  response
    .headers_mut()
    .insert(HUB_VERSION_HEADER, HeaderValue::from_static(env!("CARGO_PKG_VERSION")));
  response
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

fn internal(_detail: String) -> ApiError {
  error(StatusCode::INTERNAL_SERVER_ERROR, "Hub storage operation failed")
}

#[cfg(test)]
mod tests;
