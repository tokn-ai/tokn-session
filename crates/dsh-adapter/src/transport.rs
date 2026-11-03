use crate::{ApiProxyConfig, error};
use futures_util::StreamExt;
use reqwest::header::{COOKIE, HeaderValue, LOCATION, SET_COOKIE};
use reqwest::{Client, Url};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_tungstenite::{
  MaybeTlsStream, WebSocketStream,
  tungstenite::{client::IntoClientRequest, protocol::WebSocketConfig},
};
use tokn_dsh_rpc_protocol::api;
use tokn_interaction_core::InteractionError;

pub(crate) struct Transport {
  client: Client,
  base: Url,
  config: ApiProxyConfig,
  generation: Generation,
  cookie: Option<HeaderValue>,
}

#[cfg(test)]
mod tests {
  use super::*;
  use axum::{
    Json, Router,
    extract::{Query, WebSocketUpgrade},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
  };
  use std::collections::BTreeMap;

  const TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
  const COOKIE_VALUE: &str = "dsh-auth-fixture=v2.fixture.signature";

  struct Peer {
    endpoint: String,
    task: tokio::task::JoinHandle<()>,
  }

  impl Drop for Peer {
    fn drop(&mut self) {
      self.task.abort();
    }
  }

  async fn peer(location: &'static str, cookie: &'static str) -> Peer {
    let router = Router::new()
      .route("/connect", get(move |Query(query): Query<BTreeMap<String, String>>| async move {
        assert_eq!(query.get("token").map(String::as_str), Some(TOKEN));
        assert_eq!(query.get("interactions").map(String::as_str), Some("answer"));
        assert_eq!(query.len(), 2);
        (StatusCode::FOUND, [(LOCATION, location), (SET_COOKIE, cookie)]).into_response()
      }))
      .route("/api/session/modelCatalog", post(|headers: HeaderMap, Json(body): Json<Value>| async move {
        assert_eq!(headers.get(COOKIE).unwrap(), COOKIE_VALUE);
        assert!(headers.get("authorization").is_none());
        assert_eq!(body["method"], "session/modelCatalog");
        assert_eq!(body["payload"], serde_json::json!({"args":{}}));
        Json(serde_json::json!({"type":"server-response","rpcId":body["rpcId"],"result":{"ok":true,"value":{"checked":true}}}))
      }))
      .route("/api/remote.mux", get(|headers: HeaderMap, upgrade: WebSocketUpgrade| async move {
        assert_eq!(headers.get(COOKIE).unwrap(), COOKIE_VALUE);
        upgrade.on_upgrade(|mut stream| async move { let _ = stream.recv().await; }).into_response()
      }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
      axum::serve(listener, router).await.unwrap();
    });
    Peer { endpoint, task }
  }

  #[tokio::test]
  async fn owned_launch_token_is_exchanged_once_for_cookie_authenticated_http_and_websocket() {
    let peer = peer(
      "./api/remote.mux",
      "dsh-auth-fixture=v2.fixture.signature; HttpOnly; SameSite=Strict",
    )
    .await;
    let transport = Transport::authenticated(ApiProxyConfig::new(peer.endpoint.clone()), TOKEN.into())
      .await
      .unwrap();
    assert_eq!(transport.generation(), Generation::Remote);
    assert!(transport.base.query().is_none());
    assert!(transport.cookie.as_ref().unwrap().is_sensitive());
    let value: Value = transport
      .call("probe", "llm.models", serde_json::json!({}), false)
      .await
      .unwrap();
    assert_eq!(value, serde_json::json!({"checked":true}));
    let _stream = transport.stream("api/remote.mux").await.unwrap();
  }

  #[tokio::test]
  async fn authentication_rejects_redirects_or_invalid_cookie_without_exposing_credentials() {
    for (location, cookie) in [
      ("https://example.invalid/", "dsh-auth-fixture=v2.fixture.signature"),
      ("./api/remote.mux", "unrelated=value"),
    ] {
      let peer = peer(location, cookie).await;
      let error = Transport::authenticated(ApiProxyConfig::new(peer.endpoint.clone()), TOKEN.into())
        .await
        .err()
        .unwrap();
      assert!(!error.message.contains(TOKEN));
      assert!(!error.message.contains(cookie));
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Generation {
  ApiProxy,
  Remote,
}

pub(crate) type Downlink = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

impl Transport {
  pub(crate) fn new(config: ApiProxyConfig) -> Result<Self, InteractionError> {
    let mut base = Url::parse(&config.endpoint).map_err(|_| InteractionError::invalid("invalid DSH endpoint URL"))?;
    if !matches!(base.scheme(), "http" | "https")
      || base.host_str().is_none()
      || !base.username().is_empty()
      || base.password().is_some()
      || base.query().is_some()
      || base.fragment().is_some()
    {
      return Err(InteractionError::invalid(
        "DSH endpoint must be an HTTP(S) base URL without credentials, query, or fragment",
      ));
    }
    if !base.path().ends_with('/') {
      base.set_path(&format!("{}/", base.path()));
    }
    if config.request_timeout.is_zero()
      || config.connect_timeout.is_zero()
      || config.heartbeat_interval.is_zero()
      || config.heartbeat_timeout.is_zero()
      || config.max_body_bytes == 0
      || config.max_frame_bytes == 0
    {
      return Err(InteractionError::invalid(
        "DSH timeouts and byte limits must be positive",
      ));
    }
    let client = Client::builder()
      .connect_timeout(config.connect_timeout)
      .redirect(reqwest::redirect::Policy::none());
    // Owned readiness always binds literal loopback. Never send its launch
    // credential or cookie through an environment-configured HTTP proxy.
    let client = if matches!(base.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")) {
      client.no_proxy()
    } else {
      client
    };
    let client = client
      .build()
      .map_err(|_| InteractionError::unavailable("could not initialize DSH HTTP transport"))?;
    Ok(Self {
      client,
      base,
      config,
      generation: Generation::ApiProxy,
      cookie: None,
    })
  }

  /// Exchange only the readiness credential emitted by the owned child. Neither
  /// the query token nor the signed operator cookie leaves this transport.
  pub(crate) async fn authenticated(config: ApiProxyConfig, launch_token: String) -> Result<Self, InteractionError> {
    let mut transport = Self::new(config)?;
    let mut connect = transport.base.join("connect").expect("fixed DSH connect route");
    connect
      .query_pairs_mut()
      .append_pair("token", &launch_token)
      .append_pair("interactions", "answer");
    let response = tokio::time::timeout(transport.config.connect_timeout, transport.client.get(connect).send())
      .await
      .map_err(|_| InteractionError::unavailable("DSH API authentication timed out"))?
      .map_err(|_| InteractionError::unavailable("Could not authenticate the owned DSH API"))?;
    if response.status() != reqwest::StatusCode::FOUND
      || response.headers().get(LOCATION).and_then(|value| value.to_str().ok()) != Some("./api/remote.mux")
    {
      return Err(InteractionError::unavailable(
        "DSH returned an unsupported API authentication response",
      ));
    }
    let cookies = response.headers().get_all(SET_COOKIE).iter().collect::<Vec<_>>();
    if cookies.len() != 1 {
      return Err(InteractionError::unavailable(
        "DSH did not return one API operator credential",
      ));
    }
    let cookie = cookies[0]
      .to_str()
      .ok()
      .and_then(|value| value.split(';').next())
      .filter(|value| value.len() <= 8192 && value.starts_with("dsh-auth-"))
      .ok_or_else(|| InteractionError::unavailable("DSH returned an invalid API operator credential"))?;
    let mut cookie = HeaderValue::from_str(cookie)
      .map_err(|_| InteractionError::unavailable("DSH returned an invalid API operator credential"))?;
    cookie.set_sensitive(true);
    transport.cookie = Some(cookie);
    transport.generation = Generation::Remote;
    Ok(transport)
  }

  pub(crate) fn generation(&self) -> Generation {
    self.generation
  }

  fn builder(&self, method: reqwest::Method, route: &str) -> reqwest::RequestBuilder {
    let request = self
      .client
      .request(method, self.base.join(route).expect("fixed DSH route"));
    let request = match &self.config.bearer_token {
      Some(token) => request.bearer_auth(token),
      None => request,
    };
    match &self.cookie {
      Some(cookie) => request.header(COOKIE, cookie.clone()),
      None => request,
    }
  }

  pub(crate) async fn stream(&self, route: &str) -> Result<Downlink, InteractionError> {
    let mut url = self.base.join(route).expect("fixed DSH route");
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme).expect("HTTP/WebSocket scheme");
    let mut request = url
      .as_str()
      .into_client_request()
      .map_err(|_| InteractionError::invalid("invalid DSH WebSocket request"))?;
    if let Some(token) = &self.config.bearer_token {
      let value = format!("Bearer {token}")
        .parse()
        .map_err(|_| InteractionError::invalid("invalid DSH authorization header"))?;
      request.headers_mut().insert("authorization", value);
    }
    if let Some(cookie) = &self.cookie {
      request.headers_mut().insert(COOKIE, cookie.clone());
    }
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(self.config.max_frame_bytes);
    config.max_frame_size = Some(self.config.max_frame_bytes);
    let (socket, _) = tokio::time::timeout(
      self.config.connect_timeout,
      tokio_tungstenite::connect_async_with_config(request, Some(config), false),
    )
    .await
    .map_err(|_| InteractionError::unavailable("DSH event stream connection timed out"))?
    .map_err(|_| InteractionError::unavailable("could not connect to DSH event stream"))?;
    Ok(socket)
  }

  pub(crate) async fn call<T: DeserializeOwned>(
    &self,
    rpc_id: &str,
    method: &str,
    payload: Value,
    mutation: bool,
  ) -> Result<T, InteractionError> {
    let (endpoint, payload) = if self.generation == Generation::Remote {
      crate::remote_api::request(method, rpc_id, payload)?
    } else {
      (method, payload)
    };
    let body = serde_json::to_vec(&api::Request::client(rpc_id, endpoint, payload))
      .map_err(|_| InteractionError::invalid("invalid DSH request"))?;
    let native: api::Response = self.post(&format!("api/{endpoint}"), body, mutation).await?;
    if native.native_type != "server-response" || native.rpc_id != rpc_id {
      return Err(error(mutation, "DSH response identity does not match its request"));
    }
    if !native.result.ok {
      let rejected = native
        .result
        .error
        .ok_or_else(|| error(mutation, "DSH returned a malformed error response"))?;
      // Creation can fail after native publication. Only native errors with a
      // definite identity/configuration refusal authorize an editable failure.
      let kind = if method == "session.create"
        && !matches!(
          rejected.code.as_str(),
          "session-conflict"
            | "agent-preset-conflict"
            | "agent-preset-not-found"
            | "agent-preset-invalid"
            | "agent-busy"
            | "session/conflict"
            | "agent-preset/conflict"
            | "agent-preset/not-found"
            | "agent-preset/invalid"
            | "session/agent-busy"
            | "gateway/arguments-invalid"
        ) {
        tokn_interaction_core::ErrorKind::Uncertain
      } else {
        tokn_interaction_core::ErrorKind::Rejected
      };
      return Err(InteractionError::new(
        kind,
        format!("{}: {}", rejected.code, rejected.message),
      ));
    }
    if native.result.error.is_some() {
      return Err(error(mutation, "DSH returned an inconsistent result"));
    }
    serde_json::from_value(match native.result.value {
      Some(value) => value,
      None if self.generation == Generation::Remote && method == "$events/result" => Value::Null,
      None => return Err(error(mutation, "DSH response has no result")),
    })
    .map_err(|_| error(mutation, "DSH response does not match the native contract"))
  }

  pub(crate) async fn respond(&self, rpc_id: &str, result: Value) -> Result<api::Receipt, InteractionError> {
    let body = serde_json::to_vec(&serde_json::json!({"type":"client-response", "rpcId":rpc_id,"result":result}))
      .map_err(|_| InteractionError::invalid("invalid DSH response"))?;
    self.post("api/respond", body, true).await
  }

  async fn post<T: DeserializeOwned>(&self, route: &str, body: Vec<u8>, mutation: bool) -> Result<T, InteractionError> {
    if body.len() > self.config.max_body_bytes {
      return Err(InteractionError::invalid("DSH request exceeds the byte limit"));
    }
    let operation = async {
      let response = self
        .builder(reqwest::Method::POST, route)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|_| error(mutation, "DSH HTTP request failed; it was not retried"))?;
      if !response.status().is_success() {
        return Err(error(
          mutation,
          format!("DSH HTTP request failed with {}", response.status()),
        ));
      }
      if response
        .content_length()
        .is_some_and(|length| length > self.config.max_body_bytes as u64)
      {
        return Err(error(mutation, "DSH response exceeds the byte limit"));
      }
      let mut bytes = Vec::new();
      let mut stream = response.bytes_stream();
      while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| error(mutation, "DSH response was interrupted"))?;
        if bytes.len().saturating_add(chunk.len()) > self.config.max_body_bytes {
          return Err(error(mutation, "DSH response exceeds the byte limit"));
        }
        bytes.extend_from_slice(&chunk);
      }
      serde_json::from_slice(&bytes).map_err(|_| error(mutation, "DSH response is not valid JSON"))
    };
    tokio::time::timeout(self.config.request_timeout, operation)
      .await
      .map_err(|_| error(mutation, "DSH request timed out; it was not retried"))?
  }
}
