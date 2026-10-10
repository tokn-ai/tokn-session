//! Use the trusted Hub origin for WebAuthn, then return only the assertion to
//! the app's one-shot loopback callback. Session traffic never uses this server.
use axum::{
  Form, Router,
  extract::{DefaultBodyLimit, State},
  http::{HeaderMap, StatusCode, header},
  response::{Html, IntoResponse, Response},
  routing::post,
};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;
use tokn_hub_client_core::protocol;

#[derive(Clone, Default)]
pub struct PasskeyCallbacks {
  pending: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

impl PasskeyCallbacks {
  pub async fn cancel(&self, id: &str) {
    if let Some(token) = self.pending.lock().await.remove(id) {
      token.cancel();
    }
  }
  pub async fn cancel_all(&self) {
    for (_, token) in self.pending.lock().await.drain() {
      token.cancel();
    }
  }
}

#[derive(Clone)]
struct Callback {
  sender: Arc<Mutex<Option<oneshot::Sender<Result<Value, String>>>>>,
  authority: String,
  origin: String,
}

pub async fn credential(
  app: AppHandle,
  callbacks: PasskeyCallbacks,
  hub_url: &str,
  options: Value,
  register: bool,
  auth_id: Option<String>,
) -> Result<Value, String> {
  let hub = tokn_hub_remote::canonical_hub(hub_url)?;
  validate_options(&options)?;
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
    .await
    .map_err(|error| format!("Create passkey callback: {error}"))?;
  let address = listener.local_addr().map_err(|error| error.to_string())?;
  let nonce = protocol::encode(&rand::random::<[u8; 32]>());
  let path = format!("/passkey/{nonce}");
  let callback_url = format!("http://{address}{path}");
  let (sender, result) = oneshot::channel();
  let router = Router::new()
    .route(&path, post(callback))
    .layer(DefaultBodyLimit::max(64 * 1024))
    .with_state(Callback {
      sender: Arc::new(Mutex::new(Some(sender))),
      authority: address.to_string(),
      origin: hub.origin().ascii_serialization(),
    });
  let cancel = CancellationToken::new();
  let callback_id = auth_id.unwrap_or_else(|| nonce.clone());
  {
    let mut pending = callbacks.pending.lock().await;
    if pending.len() >= 4 {
      return Err("Too many pending app passkey ceremonies".into());
    }
    if pending.contains_key(&callback_id) {
      return Err("Passkey ceremony is already open".into());
    }
    pending.insert(callback_id.clone(), cancel.clone());
  }
  let server_cancel = cancel.clone();
  let server = tokio::spawn(async move {
    axum::serve(listener, router)
      .with_graceful_shutdown(server_cancel.cancelled_owned())
      .await
  });
  let encoded = protocol::encode(
    &serde_json::to_vec(&json!({
      "operation":if register {"register"} else {"login"},"options":options,"callback_url":callback_url,
    }))
    .map_err(|error| error.to_string())?,
  );
  let mut page = hub;
  page.set_path("/passkey");
  page.set_fragment(Some(&format!("request={encoded}")));
  let outcome = match app.opener().open_url(page.as_str(), None::<&str>) {
    Err(error) => Err(format!("Open passkey browser: {error}")),
    Ok(()) => tokio::select! {
      _ = cancel.cancelled()=>Err("Passkey ceremony cancelled".into()),
      result = tokio::time::timeout(Duration::from_secs(120),result)=>match result {
        Ok(Ok(result))=>result,
        Ok(Err(_))=>Err("Passkey browser closed without a response".into()),
        Err(_)=>Err("Passkey ceremony timed out".into()),
      },
    },
  };
  cancel.cancel();
  callbacks.pending.lock().await.remove(&callback_id);
  // Allow the callback response to reach the browser before closing its socket.
  let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
  outcome
}

fn validate_options(options: &Value) -> Result<(), String> {
  if !options.get("publicKey").is_some_and(Value::is_object)
    || serde_json::to_vec(options).map_err(|error| error.to_string())?.len() > 32 * 1024
  {
    return Err("Invalid passkey options".into());
  }
  Ok(())
}

async fn callback(
  State(state): State<Callback>,
  headers: HeaderMap,
  Form(fields): Form<HashMap<String, String>>,
) -> Response {
  let exact = |name| {
    let mut values = headers.get_all(name).iter();
    let first = values.next().and_then(|value| value.to_str().ok());
    if values.next().is_some() { None } else { first }
  };
  if exact(header::HOST) != Some(state.authority.as_str()) || exact(header::ORIGIN) != Some(state.origin.as_str()) {
    return (StatusCode::FORBIDDEN, "Invalid passkey callback origin").into_response();
  }
  let result = if let Some(credential) = fields.get("credential") {
    if credential.len() > 32 * 1024 {
      return (StatusCode::PAYLOAD_TOO_LARGE, "Passkey response too large").into_response();
    }
    match serde_json::from_str::<Value>(credential) {
      Ok(value) if value.is_object() => Ok(value),
      _ => return (StatusCode::BAD_REQUEST, "Invalid passkey response").into_response(),
    }
  } else if let Some(error) = fields.get("error") {
    Err(error.chars().take(512).collect())
  } else {
    return (StatusCode::BAD_REQUEST, "Missing passkey response").into_response();
  };
  let Some(sender) = state.sender.lock().await.take() else {
    return (StatusCode::CONFLICT, "Passkey response already received").into_response();
  };
  let _ = sender.send(result);
  let mut response = Html("<!doctype html><meta name=\"viewport\" content=\"width=device-width\"><title>Tokn Sessions</title><p>Return to Tokn Sessions. You can close this tab.</p>").into_response();
  response
    .headers_mut()
    .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
  response
    .headers_mut()
    .insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
  response.headers_mut().insert(
    header::CONTENT_SECURITY_POLICY,
    "default-src 'none'; frame-ancestors 'none'; base-uri 'none'"
      .parse()
      .unwrap(),
  );
  response
}

#[cfg(test)]
mod tests {
  use super::*;

  fn headers(authority: &str, origin: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, authority.parse().unwrap());
    headers.insert(header::ORIGIN, origin.parse().unwrap());
    headers
  }

  fn fields(credential: &str) -> Form<HashMap<String, String>> {
    Form(HashMap::from([("credential".into(), credential.into())]))
  }

  #[tokio::test]
  async fn callback_requires_exact_origin_and_host_and_consumes_only_one_valid_response() {
    let (sender, mut result) = oneshot::channel();
    let state = Callback {
      sender: Arc::new(Mutex::new(Some(sender))),
      authority: "127.0.0.1:12345".into(),
      origin: "https://hub.example".into(),
    };
    for invalid in [
      headers("127.0.0.1:12345", "null"),
      headers("127.0.0.1:12345", "https://other.example"),
      headers("localhost:12345", "https://hub.example"),
    ] {
      let response = callback(State(state.clone()), invalid, fields("{}")).await;
      assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let mut duplicate = headers(&state.authority, &state.origin);
    duplicate.append(header::ORIGIN, state.origin.parse().unwrap());
    assert_eq!(
      callback(State(state.clone()), duplicate, fields("{}")).await.status(),
      StatusCode::FORBIDDEN
    );
    assert_eq!(
      callback(
        State(state.clone()),
        headers(&state.authority, &state.origin),
        fields("[]")
      )
      .await
      .status(),
      StatusCode::BAD_REQUEST
    );
    assert!(matches!(result.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
    let response = callback(
      State(state.clone()),
      headers(&state.authority, &state.origin),
      fields(r#"{"id":"assertion"}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(result.await.unwrap().unwrap(), json!({"id":"assertion"}));
    assert_eq!(
      callback(
        State(state.clone()),
        headers(&state.authority, &state.origin),
        fields("{}")
      )
      .await
      .status(),
      StatusCode::CONFLICT
    );
  }

  #[tokio::test]
  async fn callback_bounds_credentials_and_returns_browser_cancellation() {
    let (sender, mut result) = oneshot::channel();
    let state = Callback {
      sender: Arc::new(Mutex::new(Some(sender))),
      authority: "127.0.0.1:12345".into(),
      origin: "https://hub.example".into(),
    };
    let response = callback(
      State(state.clone()),
      headers(&state.authority, &state.origin),
      fields(&"x".repeat(33 * 1024)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(matches!(result.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
    let response = callback(
      State(state.clone()),
      headers(&state.authority, &state.origin),
      Form(HashMap::from([("error".into(), "cancelled".into())])),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(result.await.unwrap(), Err("cancelled".into()));
  }

  #[test]
  fn rejects_unbounded_or_missing_passkey_options() {
    assert!(validate_options(&json!({})).is_err());
    assert!(validate_options(&json!({"publicKey":{"challenge":"x".repeat(33*1024)}})).is_err());
    assert!(validate_options(&json!({"publicKey":{"challenge":"abc"}})).is_ok());
  }
}
