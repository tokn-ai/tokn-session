//! Dispatch authorized encrypted requests to the fixed loopback viewer API.
use crate::{
  connector::ConnectorConfig,
  protocol,
  secure::{Grant, GrantScope},
};
use serde_json::{Value, json};

pub async fn forward(
  config: &ConnectorConfig,
  client: &reqwest::Client,
  grant: &Grant,
  method: &str,
  path: &str,
  body: Vec<u8>,
) -> Result<reqwest::Response, String> {
  if !protocol::allowed_route(method, path, config.allow_control && grant.allow_control) {
    return Err("Route unavailable for this grant".into());
  }
  if body.len() > protocol::MAX_BODY {
    return Err("Request is too large".into());
  }
  let mut url = config.local_url.clone();
  let request = match &grant.scope {
    GrantScope::All {} => {
      if !(config.allow_control && grant.allow_control)
        && let Some(value) = protocol::read_only_model_response(path, "This grant allows viewing only")
      {
        return json_response(value);
      }
      if matches!(
        path,
        "/api/v1/get_session_input_status" | "/api/v1/get_session_question_status"
      ) && !(config.allow_control && grant.allow_control)
      {
        let body = serde_json::to_vec(&json!({
          "available": false, "message": "This grant allows viewing only", "max_length": 0, "question_ids": [],
            "images_available": false,
            "images_stageable": false, "max_images": 0, "max_image_bytes": 0,
            "max_total_image_bytes": 0, "image_mime_types": []
        }))
        .map_err(|_| "Could not encode input status")?;
        return Ok(reqwest::Response::from(
          axum::http::Response::builder()
            .header("content-type", "application/json")
            .body(body)
            .map_err(|_| "Could not encode input status")?,
        ));
      }
      url.set_path(path);
      let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| "Invalid method")?;
      client.request(method, url).body(body)
    }
    GrantScope::Sessions { session_keys } => {
      let command = path.strip_prefix("/api/v1/").ok_or("Route unavailable")?;
      if matches!(
        command,
        "select_session_model"
          | "submit_session_input"
          | "submit_session_question_answers"
          | "retry_session_index"
          | "upload_image_chunk"
          | "release_image_upload"
      ) {
        return Err("This session share is read-only".into());
      }
      let payload: Value = if body.is_empty() {
        json!({})
      } else {
        serde_json::from_slice(&body).map_err(|_| "Invalid request JSON")?
      };
      // Neither scope nor authenticated principal can come from the request body.
      // The inner payload remains an ordinary viewer command envelope.
      url.set_path("/api/v1/shared");
      let envelope = serde_json::to_vec(&json!({
        "session_keys": session_keys,
        "principal": grant.recipient_public_key,
        "command": command,
        "payload": payload,
      }))
      .map_err(|_| "Could not encode shared request")?;
      client.post(url).body(envelope)
    }
  };
  let mut request = request.header(reqwest::header::CONTENT_TYPE, "application/json");
  if let Some(token) = &config.local_token {
    request = request.bearer_auth(token);
  }
  let response = request
    .send()
    .await
    .map_err(|_| "Local API request failed; delivery may be uncertain and will not be retried")?;
  if response.status().is_redirection() {
    return Err("Local API redirects are forbidden".into());
  }
  Ok(response)
}

fn json_response(value: Value) -> Result<reqwest::Response, String> {
  Ok(reqwest::Response::from(
    axum::http::Response::builder()
      .header("content-type", "application/json")
      .body(serde_json::to_vec(&value).map_err(|_| "Could not encode model status")?)
      .map_err(|_| "Could not encode model status")?,
  ))
}
