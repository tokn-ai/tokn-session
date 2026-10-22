use reqwest::{Client, Method, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::time::Duration;
use tokn_interaction_core::{ErrorKind, InteractionError};

pub(crate) const MAX_BODY: usize = 8 * 1024 * 1024;

/// Credentials remain host-side and deliberately do not implement Serialize/Debug.
#[derive(Clone)]
pub struct Credentials {
  pub username: String,
  pub password: String,
}

#[derive(Clone)]
pub(crate) struct Transport {
  endpoint: Url,
  directory: String,
  workspace_id: Option<String>,
  credentials: Option<Credentials>,
  client: Client,
  events: Client,
}

impl Transport {
  pub fn new(
    endpoint: &str,
    directory: String,
    workspace_id: Option<String>,
    credentials: Option<Credentials>,
  ) -> Result<Self, InteractionError> {
    let endpoint = Url::parse(endpoint).map_err(|_| InteractionError::invalid("Invalid OpenCode endpoint URL"))?;
    if !matches!(endpoint.scheme(), "http" | "https")
      || endpoint.host_str().is_none()
      || !endpoint.username().is_empty()
      || endpoint.password().is_some()
      || endpoint.query().is_some()
      || endpoint.fragment().is_some()
      || endpoint.path() != "/"
    {
      return Err(InteractionError::invalid(
        "OpenCode endpoint must be an HTTP(S) server origin without embedded credentials",
      ));
    }
    if !std::path::Path::new(&directory).is_absolute() || directory.contains('\0') {
      return Err(InteractionError::invalid("OpenCode directory must be an absolute path"));
    }
    let builder = || {
      Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
    };
    let client = builder()
      .timeout(Duration::from_secs(20))
      .build()
      .map_err(|_| InteractionError::unavailable("Could not initialize OpenCode HTTP transport"))?;
    let events = builder()
      .read_timeout(Duration::from_secs(45))
      .build()
      .map_err(|_| InteractionError::unavailable("Could not initialize OpenCode event transport"))?;
    Ok(Self {
      endpoint,
      directory,
      workspace_id,
      credentials,
      client,
      events,
    })
  }

  fn request(&self, client: &Client, method: Method, path: &[&str]) -> reqwest::RequestBuilder {
    let mut url = self.endpoint.clone();
    url.path_segments_mut().expect("validated HTTP origin").extend(path);
    url
      .query_pairs_mut()
      .append_pair("location[directory]", &self.directory);
    if let Some(workspace) = &self.workspace_id {
      url.query_pairs_mut().append_pair("location[workspace]", workspace);
    }
    let request = client.request(method, url).header("Accept", "application/json");
    match &self.credentials {
      Some(credentials) => request.basic_auth(&credentials.username, Some(&credentials.password)),
      None => request,
    }
  }

  pub async fn json<T: DeserializeOwned>(&self, path: &[&str]) -> Result<T, InteractionError> {
    let value = self.send(Method::GET, path, None, false).await?;
    serde_json::from_value(value)
      .map_err(|_| InteractionError::unavailable("OpenCode returned an incompatible V2 response"))
  }

  pub async fn legacy_messages(&self, session: &str) -> Result<Vec<Value>, InteractionError> {
    // Legacy V1 and V2 share session metadata but not conversation projections.
    let request = self
      .request(&self.client, Method::GET, &["session", session, "message"])
      .query(&[("limit", "1"), ("directory", self.directory.as_str())]);
    let response = request.send().await.map_err(|_| transport_error(false))?;
    // Native V1 history reads initialize the legacy config parser. V2-only
    // configuration is valid for V2, but cannot establish V1 history absence.
    if response.status() == StatusCode::BAD_REQUEST {
      let body = read_json(response, false).await?;
      return Err(legacy_history_error(&body));
    }
    let value = read_response(response, false).await?;
    serde_json::from_value(value)
      .map_err(|_| InteractionError::unavailable("Could not verify OpenCode V1/V2 session compatibility"))
  }

  pub async fn post(&self, path: &[&str], body: Value) -> Result<Value, InteractionError> {
    self.send(Method::POST, path, Some(body), true).await
  }

  async fn send(
    &self,
    method: Method,
    path: &[&str],
    body: Option<Value>,
    mutation: bool,
  ) -> Result<Value, InteractionError> {
    let mut request = self.request(&self.client, method, path);
    if let Some(body) = body {
      let bytes = serde_json::to_vec(&body).map_err(|_| InteractionError::invalid("Invalid OpenCode request body"))?;
      if bytes.len() > MAX_BODY {
        return Err(InteractionError::invalid("OpenCode request exceeds the byte limit"));
      }
      request = request.header("Content-Type", "application/json").body(bytes);
    }
    let response = request.send().await.map_err(|_| transport_error(mutation))?;
    read_response(response, mutation).await
  }

  pub async fn events(&self) -> Result<reqwest::Response, InteractionError> {
    let response = self
      .request(&self.events, Method::GET, &["api", "event"])
      .header("Accept", "text/event-stream")
      .send()
      .await
      .map_err(|_| transport_error(false))?;
    if !response.status().is_success() {
      return Err(http_error(response.status(), false));
    }
    if !response
      .headers()
      .get("content-type")
      .and_then(|value| value.to_str().ok())
      .is_some_and(|value| value.starts_with("text/event-stream"))
    {
      return Err(InteractionError::unavailable(
        "OpenCode does not expose a native V2 event stream",
      ));
    }
    Ok(response)
  }
}

fn transport_error(mutation: bool) -> InteractionError {
  if mutation {
    InteractionError::uncertain(
      "OpenCode connection ended before admission was confirmed; do not replay with a new request ID",
    )
  } else {
    InteractionError::unavailable("OpenCode RPC endpoint could not be reached before the timeout")
  }
}

fn http_error(status: StatusCode, mutation: bool) -> InteractionError {
  let kind = match status {
    StatusCode::CONFLICT => ErrorKind::Conflict,
    StatusCode::NOT_FOUND => ErrorKind::StaleTarget,
    status if status.is_client_error() => ErrorKind::Rejected,
    _ if mutation => ErrorKind::Uncertain,
    _ => ErrorKind::Unavailable,
  };
  InteractionError::new(kind, format!("OpenCode RPC returned HTTP {}", status.as_u16()))
}

fn legacy_history_error(body: &Value) -> InteractionError {
  if body["name"] == "ConfigInvalidError" {
    InteractionError::unsupported(
      "OpenCode's legacy-history check could not read this location's configuration; control requires a configuration compatible with both native V1 and V2",
    )
  } else {
    http_error(StatusCode::BAD_REQUEST, false)
  }
}

async fn read_response(response: reqwest::Response, mutation: bool) -> Result<Value, InteractionError> {
  if !response.status().is_success() {
    return Err(http_error(response.status(), mutation));
  }
  if response.status() == StatusCode::NO_CONTENT {
    return Ok(Value::Null);
  }
  read_json(response, mutation).await
}

async fn read_json(mut response: reqwest::Response, mutation: bool) -> Result<Value, InteractionError> {
  if !response
    .headers()
    .get("content-type")
    .and_then(|value| value.to_str().ok())
    .is_some_and(|value| value.starts_with("application/json"))
  {
    return Err(if mutation {
      InteractionError::uncertain("OpenCode returned an incompatible admission response")
    } else {
      InteractionError::unavailable("OpenCode endpoint returned non-JSON content; native V2 API is required")
    });
  }
  let mut body = Vec::new();
  while let Some(chunk) = response.chunk().await.map_err(|_| transport_error(mutation))? {
    if body.len().saturating_add(chunk.len()) > MAX_BODY {
      return Err(InteractionError::new(
        if mutation {
          ErrorKind::Uncertain
        } else {
          ErrorKind::Unavailable
        },
        "OpenCode response exceeds the byte limit",
      ));
    }
    body.extend_from_slice(&chunk);
  }
  serde_json::from_slice(&body).map_err(|_| {
    InteractionError::new(
      if mutation {
        ErrorKind::Uncertain
      } else {
        ErrorKind::Unavailable
      },
      "OpenCode returned malformed JSON",
    )
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn real_native_config_rejection_explains_unavailable_history_verification() {
    let fixture: Value =
      serde_json::from_str(include_str!("../tests/fixtures/native-legacy-config-rejection.json")).unwrap();
    assert_eq!(fixture["upstream_revision"], "017a5977d2107092007623e507fc5c6eb337d3b2");
    let error = legacy_history_error(&fixture["records"][0]["response"]);
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert!(error.message.contains("both native V1 and V2"));
    assert!(!error.message.contains("opencode.json"));
    assert_eq!(
      legacy_history_error(&serde_json::json!({"name":"OtherError"})).kind,
      ErrorKind::Rejected
    );
  }
}
