//! Exact public name lookup. Directory metadata is not a Noise trust anchor.
use crate::{DiscoveryGroup, DiscoveryResult, PresenceResult, ResolvedMachine, canonical_hub};
use std::time::Duration;
use tokn_hub_client_core::{
  address::parse_machine_address,
  discovery::MAX_DISCOVERY_RESPONSE,
  presence::{MAX_PRESENCE_RESPONSE, PRESENCE_VERSIONS_HEADER, PresenceRequest},
};
use url::Url;

const MAX_RESPONSE: usize = 4096;
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) async fn resolve(hub_url: &str, machine_address: &str) -> Result<ResolvedMachine, String> {
  resolve_with_timeout(hub_url, machine_address, LOOKUP_TIMEOUT).await
}

async fn resolve_with_timeout(
  hub_url: &str,
  machine_address: &str,
  timeout: Duration,
) -> Result<ResolvedMachine, String> {
  let (username, machine_name) = parse_machine_address(machine_address)?;
  let mut endpoint = canonical_hub(hub_url)?;
  endpoint.set_path(&format!("/hub/v1/resolve/{username}/{machine_name}"));
  let body = fetch(endpoint, MAX_RESPONSE, timeout).await?;
  let resolved: ResolvedMachine =
    serde_json::from_slice(&body).map_err(|_| "Hub directory returned invalid machine metadata")?;
  resolved.validate_for(machine_address)?;
  Ok(resolved)
}

pub(crate) async fn discover(hub_url: &str, group: &DiscoveryGroup) -> Result<DiscoveryResult, String> {
  group.validate()?;
  let mut endpoint = canonical_hub(hub_url)?;
  endpoint.set_path(&format!(
    "/hub/v1/discovery/{}/{}",
    group.namespace, group.secret_digest
  ));
  let body = fetch(endpoint, MAX_DISCOVERY_RESPONSE, LOOKUP_TIMEOUT).await?;
  let discovered: DiscoveryResult =
    serde_json::from_slice(&body).map_err(|_| "Hub directory returned invalid discovery metadata")?;
  discovered.validate_for(group)?;
  Ok(discovered)
}

async fn fetch(endpoint: Url, maximum: usize, timeout: Duration) -> Result<Vec<u8>, String> {
  fetch_request(directory_client()?.get(endpoint), maximum, timeout).await
}

pub(crate) async fn presence(hub_url: &str, host_ids: &[String]) -> Result<PresenceResult, String> {
  let request = PresenceRequest {
    host_ids: host_ids.to_vec(),
  };
  request.validate()?;
  let mut endpoint = canonical_hub(hub_url)?;
  if host_ids.is_empty() {
    return Ok(PresenceResult { machines: Vec::new() });
  }
  endpoint.set_path("/hub/v1/presence");
  let body = serde_json::to_vec(&request).map_err(|_| "Could not encode Hub presence request")?;
  let request = directory_client()?
    .post(endpoint)
    .header(reqwest::header::CONTENT_TYPE, "application/json")
    .header(reqwest::header::ACCEPT, "application/json")
    .header(PRESENCE_VERSIONS_HEADER, "1")
    .body(body);
  let body = fetch_request(request, MAX_PRESENCE_RESPONSE, LOOKUP_TIMEOUT).await?;
  let result: PresenceResult = serde_json::from_slice(&body).map_err(|_| "Hub returned invalid presence metadata")?;
  result.validate_for(host_ids)?;
  Ok(result)
}

fn directory_client() -> Result<reqwest::Client, String> {
  reqwest::Client::builder()
    .no_proxy()
    .redirect(reqwest::redirect::Policy::none())
    .retry(reqwest::retry::never())
    .connect_timeout(Duration::from_secs(5))
    .build()
    .map_err(|_| "Could not create Hub directory connection".into())
}

async fn fetch_request(request: reqwest::RequestBuilder, maximum: usize, timeout: Duration) -> Result<Vec<u8>, String> {
  tokio::time::timeout(timeout, async {
    let mut response = request
      .send()
      .await
      .map_err(|_| "Could not load machine directory through the Hub")?;
    let status = response.status();
    if response.content_length().is_some_and(|length| length > maximum as u64) {
      return Err("Hub directory response exceeds its size limit".into());
    }
    let json_type = response
      .headers()
      .get(reqwest::header::CONTENT_TYPE)
      .and_then(|value| value.to_str().ok())
      .is_some_and(|value| {
        value
          .split(';')
          .next()
          .is_some_and(|mime| mime.trim() == "application/json")
      });
    if !json_type {
      return Err("Hub directory returned an unsupported response type".into());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
      .chunk()
      .await
      .map_err(|_| "Could not read Hub directory response")?
    {
      if body.len() + chunk.len() > maximum {
        return Err("Hub directory response exceeds its size limit".into());
      }
      body.extend_from_slice(&chunk);
    }
    if status != reqwest::StatusCode::OK {
      let message = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| {
          value
            .get("error")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
        })
        .filter(|message| !message.is_empty() && message.len() <= 512 && !message.chars().any(char::is_control));
      return Err(message.unwrap_or_else(|| format!("Hub directory rejected lookup ({status})")));
    }
    Ok(body)
  })
  .await
  .map_err(|_| "Hub directory lookup timed out")?
}

#[cfg(test)]
mod tests {
  use super::*;
  use axum::{Json, Router, http::header::CONTENT_TYPE, routing::post};
  use tokn_hub_client_core::presence::MachinePresence;

  const HOST_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
  const OLDER_HOST_ID: &str = "550e8400-e29b-41d4-a716-446655440001";

  #[tokio::test]
  async fn presence_posts_only_requested_ids_and_preserves_optional_connector_versions() {
    let app = Router::new().route(
      "/hub/v1/presence",
      post(
        |headers: axum::http::HeaderMap, Json(request): Json<PresenceRequest>| async move {
          assert_eq!(headers[CONTENT_TYPE], "application/json");
          assert_eq!(headers[PRESENCE_VERSIONS_HEADER], "1");
          assert_eq!(request.host_ids, [HOST_ID, OLDER_HOST_ID]);
          Json(PresenceResult {
            machines: vec![
              MachinePresence {
                host_id: HOST_ID.into(),
                online: true,
                connector_version: Some("0.1.1".into()),
              },
              MachinePresence {
                host_id: OLDER_HOST_ID.into(),
                online: true,
                connector_version: None,
              },
            ],
          })
        },
      ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let result = presence(&hub, &[HOST_ID.into(), OLDER_HOST_ID.into()]).await;
    server.abort();
    let result = result.unwrap();
    assert_eq!(result.machines.len(), 2);
    assert_eq!(result.machines[0].host_id, HOST_ID);
    assert!(result.machines[0].online);
    assert_eq!(result.machines[0].connector_version.as_deref(), Some("0.1.1"));
    assert_eq!(result.machines[1].host_id, OLDER_HOST_ID);
    assert!(result.machines[1].online);
    assert_eq!(result.machines[1].connector_version, None);
  }

  #[tokio::test]
  async fn presence_rejects_missing_duplicate_or_unrequested_machine_rows() {
    for machines in [
      serde_json::json!([]),
      serde_json::json!([{"host_id":HOST_ID,"online":true},{"host_id":HOST_ID,"online":false}]),
      serde_json::json!([{"host_id":"550e8400-e29b-41d4-a716-446655440001","online":true}]),
    ] {
      let app = Router::new().route(
        "/hub/v1/presence",
        post(move || {
          let machines = machines.clone();
          async move { Json(serde_json::json!({"machines":machines})) }
        }),
      );
      let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
      let hub = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
      let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
      let result = presence(&hub, &[HOST_ID.into()]).await;
      server.abort();
      assert!(result.is_err());
    }
  }

  #[tokio::test]
  async fn presence_rejects_invalid_input_before_network_and_skips_empty_lookups() {
    assert!(presence("https://hub.example", &["invalid".into()]).await.is_err());
    assert!(presence("http://hub.example", &[]).await.is_err());
    assert!(presence("https://hub.example", &[]).await.unwrap().machines.is_empty());
  }

  #[tokio::test]
  async fn directory_deadline_bounds_a_server_that_never_responds() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let result = resolve_with_timeout(&hub, "alice:workstation", Duration::from_millis(20)).await;
    assert!(result.unwrap_err().contains("timed out"));
  }
}
