use crate::{ApiProxyConfig, error};
use futures_util::StreamExt;
use reqwest::{Client, Url};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::{
  Mutex,
  atomic::{AtomicBool, Ordering},
};
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
  launch_token: Option<String>,
  cookie: Mutex<Option<String>>,
  gateway: AtomicBool,
  pub(crate) client_id: Mutex<Option<String>>,
}

pub(crate) type Downlink = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

impl Transport {
  pub(crate) fn new(config: ApiProxyConfig) -> Result<Self, InteractionError> {
    let mut base = Url::parse(&config.endpoint).map_err(|_| InteractionError::invalid("invalid DSH endpoint URL"))?;
    let launch_token = if base.query().is_some() {
      let pairs: Vec<_> = base.query_pairs().collect();
      if pairs.len() != 1 || pairs[0].0 != "token" || pairs[0].1.is_empty() {
        return Err(InteractionError::invalid(
          "DSH endpoint supports only one launch-token query parameter",
        ));
      }
      Some(pairs[0].1.to_string())
    } else {
      None
    };
    base.set_query(None);
    if !matches!(base.scheme(), "http" | "https")
      || base.host_str().is_none()
      || !base.username().is_empty()
      || base.password().is_some()
      || base.fragment().is_some()
    {
      return Err(InteractionError::invalid(
        "DSH endpoint must be an HTTP(S) base URL without embedded credentials or fragment",
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
      .redirect(reqwest::redirect::Policy::none())
      .build()
      .map_err(|_| InteractionError::unavailable("could not initialize DSH HTTP transport"))?;
    Ok(Self {
      client,
      base,
      config,
      launch_token,
      cookie: Mutex::new(None),
      gateway: AtomicBool::new(false),
      client_id: Mutex::new(None),
    })
  }

  pub(crate) fn is_gateway(&self) -> bool {
    self.gateway.load(Ordering::Acquire)
  }

  /// Discover only through a distinct read-only route. A failed mutation never
  /// selects a different protocol or causes a retry.
  pub(crate) async fn gateway_stream(&self) -> Result<Option<Downlink>, InteractionError> {
    if let Some(token) = self.launch_token.as_ref().or(self.config.bearer_token.as_ref()) {
      let mut url = self.base.join("connect").expect("DSH connect route");
      url
        .query_pairs_mut()
        .append_pair("token", token)
        .append_pair("interactions", "answer");
      let response = self
        .client
        .get(url)
        .timeout(self.config.connect_timeout)
        .send()
        .await
        .map_err(|_| InteractionError::unavailable("DSH API credential exchange failed"))?;
      if response.status() == reqwest::StatusCode::NOT_FOUND && self.launch_token.is_none() {
        return Ok(None);
      }
      if response.status() != reqwest::StatusCode::FOUND {
        return Err(InteractionError::unavailable("DSH API launch token was refused"));
      }
      let cookies: Vec<_> = response
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .filter_map(|header| header.split(';').next())
        .filter(|cookie| !cookie.is_empty())
        .collect();
      if cookies.is_empty() {
        return Err(InteractionError::unavailable(
          "DSH credential exchange returned no API cookie",
        ));
      }
      *self.cookie.lock().expect("DSH cookie lock") = Some(cookies.join("; "));
    }
    let result = self.open_stream("api/remote.mux").await;
    match result {
      Ok(socket) => {
        self.gateway.store(true, Ordering::Release);
        Ok(Some(socket))
      }
      Err(tokio_tungstenite::tungstenite::Error::Http(response))
        if self.launch_token.is_none() && matches!(response.status().as_u16(), 404 | 405) =>
      {
        Ok(None)
      }
      Err(_) => Err(InteractionError::unavailable(
        "Could not establish DSH Gateway stream; current hosts require an API launch token",
      )),
    }
  }

  fn builder(&self, method: reqwest::Method, route: &str) -> reqwest::RequestBuilder {
    let request = self
      .client
      .request(method, self.base.join(route).expect("fixed DSH route"));
    if let Some(cookie) = self.cookie.lock().expect("DSH cookie lock").as_ref() {
      return request.header(reqwest::header::COOKIE, cookie);
    }
    match &self.config.bearer_token {
      Some(token) => request.bearer_auth(token),
      None => request,
    }
  }

  pub(crate) async fn stream(&self, route: &str) -> Result<Downlink, InteractionError> {
    self
      .open_stream(route)
      .await
      .map_err(|_| InteractionError::unavailable("could not connect to DSH event stream"))
  }

  async fn open_stream(&self, route: &str) -> Result<Downlink, tokio_tungstenite::tungstenite::Error> {
    let mut url = self.base.join(route).expect("fixed DSH route");
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme).expect("HTTP/WebSocket scheme");
    let mut request = url.as_str().into_client_request()?;
    if let Some(cookie) = self.cookie.lock().expect("DSH cookie lock").as_ref() {
      request.headers_mut().insert(
        "cookie",
        cookie
          .parse()
          .map_err(|_| tokio_tungstenite::tungstenite::Error::Io(std::io::Error::other("invalid API cookie")))?,
      );
    } else if let Some(token) = &self.config.bearer_token {
      let value = format!("Bearer {token}").parse().map_err(|_| {
        tokio_tungstenite::tungstenite::Error::Io(std::io::Error::other("invalid authorization header"))
      })?;
      request.headers_mut().insert("authorization", value);
    }
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(self.config.max_frame_bytes);
    config.max_frame_size = Some(self.config.max_frame_bytes);
    let (socket, _) = tokio::time::timeout(
      self.config.connect_timeout,
      tokio_tungstenite::connect_async_with_config(request, Some(config), false),
    )
    .await
    .map_err(|_| {
      tokio_tungstenite::tungstenite::Error::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "DSH stream timeout"))
    })??;
    Ok(socket)
  }

  pub(crate) async fn call<T: DeserializeOwned>(
    &self,
    rpc_id: &str,
    method: &str,
    mut payload: Value,
    mutation: bool,
  ) -> Result<T, InteractionError> {
    let (method, payload) = if self.is_gateway() {
      match method {
        "session.list" => ("session/list", json!({"args":{"_request":payload}})),
        "session.create" => ("session/create", json!({"args":{"request":payload}})),
        "session.prompt" => {
          payload["requestId"] = json!(rpc_id);
          ("session/prompt", json!({"args":{"request":payload}}))
        }
        "session.cancel" => ("session/cancel", json!({"args":{"request":payload}})),
        "session.state" => ("session/state", json!({"args":{"request":payload}})),
        "llm.models" => ("session/modelCatalog", json!({"args":{}})),
        "$events/result" => (method, json!({"args":payload})),
        _ => (method, payload),
      }
    } else {
      (method, payload)
    };
    let body = serde_json::to_vec(&api::Request::client(rpc_id, method, payload))
      .map_err(|_| InteractionError::invalid("invalid DSH request"))?;
    let native: api::Response = self.post(&format!("api/{method}"), body, mutation).await?;
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
      let kind = if matches!(method, "session.create" | "session/create")
        && !matches!(
          rejected.code.as_str(),
          "session-conflict"
            | "agent-preset-conflict"
            | "agent-preset-not-found"
            | "agent-preset-invalid"
            | "agent-busy"
            | "session/conflict"
            | "session/agent-busy"
            | "agent-preset/conflict"
            | "agent-preset/not-found"
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
    serde_json::from_value(
      native
        .result
        .value
        .or_else(|| (method == "$events/result").then_some(Value::Null))
        .ok_or_else(|| error(mutation, "DSH response has no result"))?,
    )
    .map_err(|_| error(mutation, "DSH response does not match the native contract"))
  }

  pub(crate) async fn respond(&self, rpc_id: &str, result: Value) -> Result<api::Receipt, InteractionError> {
    if self.is_gateway() {
      let client_id = self
        .client_id
        .lock()
        .expect("DSH client identity lock")
        .clone()
        .ok_or_else(|| InteractionError::unavailable("DSH Gateway has no active event generation"))?;
      let _: Value = self
        .call(
          &uuid::Uuid::new_v4().to_string(),
          "$events/result",
          json!({"clientId":client_id,"eventId":rpc_id,"outcome":result}),
          true,
        )
        .await?;
      return Ok(api::Receipt {
        accepted: true,
        reason: None,
        extra: Default::default(),
      });
    }
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
