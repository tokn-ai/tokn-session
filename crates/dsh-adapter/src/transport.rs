use crate::{ApiProxyConfig, error};
use futures_util::StreamExt;
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
      .redirect(reqwest::redirect::Policy::none())
      .build()
      .map_err(|_| InteractionError::unavailable("could not initialize DSH HTTP transport"))?;
    Ok(Self { client, base, config })
  }

  fn builder(&self, method: reqwest::Method, route: &str) -> reqwest::RequestBuilder {
    let request = self
      .client
      .request(method, self.base.join(route).expect("fixed DSH route"));
    match &self.config.bearer_token {
      Some(token) => request.bearer_auth(token),
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
      return Err(InteractionError::new(
        tokn_interaction_core::ErrorKind::Rejected,
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
        .ok_or_else(|| error(mutation, "DSH response has no result"))?,
    )
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
