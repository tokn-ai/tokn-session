//! Exact public name lookup. Directory metadata is not a Noise trust anchor.
use crate::{ResolvedMachine, canonical_hub};
use std::time::Duration;
use tokn_hub_client_core::address::parse_machine_address;

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
  let client = reqwest::Client::builder()
    .no_proxy()
    .redirect(reqwest::redirect::Policy::none())
    .retry(reqwest::retry::never())
    .connect_timeout(Duration::from_secs(5))
    .build()
    .map_err(|_| "Could not create Hub directory connection")?;
  tokio::time::timeout(timeout, async {
    let mut response = client
      .get(endpoint)
      .send()
      .await
      .map_err(|_| "Could not resolve machine address through the Hub")?;
    let status = response.status();
    if response
      .content_length()
      .is_some_and(|length| length > MAX_RESPONSE as u64)
    {
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
      if body.len() + chunk.len() > MAX_RESPONSE {
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
    let resolved: ResolvedMachine =
      serde_json::from_slice(&body).map_err(|_| "Hub directory returned invalid machine metadata")?;
    resolved.validate_for(machine_address)?;
    Ok(resolved)
  })
  .await
  .map_err(|_| "Hub directory lookup timed out")?
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn directory_deadline_bounds_a_server_that_never_responds() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let result = resolve_with_timeout(&hub, "alice:workstation", Duration::from_millis(20)).await;
    assert!(result.unwrap_err().contains("timed out"));
  }
}
