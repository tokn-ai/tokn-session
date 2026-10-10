//! Host-local projection for an already verified session-sharing grant.
//!
//! This endpoint has the same local API authentication as unrestricted reads.
//! The connector alone maps a cryptographically verified grant into this shape;
//! clients cannot supply the scope or principal through the remote route list.
use super::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SharedRequest {
  session_keys: Vec<String>,
  principal: String,
  command: String,
  payload: Value,
}

pub(super) async fn command(State(state): State<ApiState>, Json(mut share): Json<SharedRequest>) -> Response {
  if share.principal.is_empty() || share.principal.len() > 128 {
    return error(StatusCode::BAD_REQUEST, "Invalid sharing principal").into_response();
  }
  let service = match state.service.scoped_to_sessions(&share.session_keys) {
    Ok(service) => service,
    Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid session share").into_response(),
  };
  if share.command == "events" {
    return events(state, share.session_keys).await;
  }
  let subscription_id = (share.command == "load_session_updates")
    .then(|| {
      share
        .payload
        .get("request")
        .and_then(|request| request.get("subscription_id"))
        .cloned()
    })
    .flatten();
  if let Err(message) = prepare(&mut share) {
    return error(StatusCode::FORBIDDEN, message).into_response();
  }
  let permit = match state.requests.clone().try_acquire_owned() {
    Ok(permit) => permit,
    Err(_) => return error(StatusCode::TOO_MANY_REQUESTS, "Viewer is busy; retry shortly").into_response(),
  };
  let result = tokio::task::spawn_blocking(move || {
    let _permit = permit;
    match share.command.as_str() {
      "health" => Ok(json!({ "version": 1 })),
      "get_session_input_status" => Ok(json!({
        "available": false, "message": "This session share is read-only", "max_length": 0
      })),
      // Operational state is host-wide and must not disclose other sessions,
      // provider roots, errors, counters, or the owner's read markers.
      "get_relay_status" => Ok(json!({
        "settings": { "mode": "local", "endpoint": "", "include_native": false },
        "active_endpoint": null, "phase": "local", "native": false, "error": null
      })),
      "get_session_index_progress" => Ok(json!({
        "revision": "0", "is_refreshing": false, "activity": "idle",
        "catalog": { "scope": "full", "active_provider": null, "processed_providers": 0,
          "total_providers": 0, "pending_providers": [], "error_providers": [] },
        "body": { "active_provider": null, "pending_jobs": 0, "failed_jobs": 0,
          "completed_in_run": 0, "stale_in_run": 0, "batch_size": 0, "providers": [] },
        "worker_error": null, "retry_at_ms": null
      })),
      _ => dispatch(&service, &share.command, share.payload),
    }
  })
  .await;
  match result {
    Ok(Ok(mut value)) => {
      if let Some(id) = subscription_id {
        value["subscription_id"] = id;
      }
      Json(value).into_response()
    }
    // An index failure can contain paths/provider details outside this share.
    // Do not relay raw diagnostics from shared service internals to guests.
    Ok(Err((status, _))) => error(status, "Shared session request failed or is no longer available").into_response(),
    Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Shared session request failed").into_response(),
  }
}

fn prepare(share: &mut SharedRequest) -> Result<(), &'static str> {
  let check = |key: &str| {
    if share.session_keys.iter().any(|allowed| allowed == key) {
      Ok(())
    } else {
      Err("Session is not included in this share")
    }
  };
  let request = share.payload.get_mut("request");
  match share.command.as_str() {
    "health" | "list_sessions" | "get_relay_status" | "get_session_index_progress" => Ok(()),
    "list_session_children"
    | "load_event_page"
    | "load_event_detail"
    | "load_trajectory_event_page"
    | "acknowledge_session_attention"
    | "get_session_input_status" => {
      let field = if share.command == "list_session_children" {
        "parent_session_key"
      } else {
        "session_key"
      };
      let key = request
        .and_then(|value| value.get(field))
        .and_then(Value::as_str)
        .ok_or("Missing session key")?;
      check(key)
    }
    "load_session_updates" => {
      let request = request
        .and_then(Value::as_object_mut)
        .ok_or("Missing subscription request")?;
      check(
        request
          .get("session_key")
          .and_then(Value::as_str)
          .ok_or("Missing session key")?,
      )?;
      let id = request
        .get("subscription_id")
        .and_then(Value::as_str)
        .ok_or("Missing subscription identity")?;
      if id.is_empty() || id.len() > 128 {
        return Err("Invalid subscription identity");
      }
      let digest = Sha256::digest(format!("subscription\0{}\0{id}", share.principal).as_bytes());
      let mut scoped_id = String::from("share_");
      for byte in digest {
        use std::fmt::Write;
        let _ = write!(scoped_id, "{byte:02x}");
      }
      request.insert("subscription_id".into(), Value::String(scoped_id));
      Ok(())
    }
    "update_session_view" => {
      let request = request.and_then(Value::as_object_mut).ok_or("Missing view request")?;
      if let Some(key) = request.get("session_key").filter(|value| !value.is_null()) {
        check(key.as_str().ok_or("Invalid session key")?)?;
      }
      if let Some(keys) = request.get("candidate_session_keys") {
        for key in keys.as_array().ok_or("Invalid session candidates")? {
          check(key.as_str().ok_or("Invalid session candidate")?)?;
        }
      }
      let view_id = request
        .get("view_id")
        .and_then(Value::as_str)
        .ok_or("Missing viewer identity")?;
      if view_id.is_empty() || view_id.len() > 128 {
        return Err("Invalid viewer identity");
      }
      let digest = Sha256::digest(format!("{}\0{view_id}", share.principal).as_bytes());
      let mut scoped_id = String::from("share_");
      for byte in digest {
        use std::fmt::Write;
        let _ = write!(scoped_id, "{byte:02x}");
      }
      request.insert("view_id".into(), Value::String(scoped_id));
      Ok(())
    }
    _ => Err("This command is unavailable for a session share"),
  }
}

async fn events(state: ApiState, keys: Vec<String>) -> Response {
  let permit = match state.subscribers.clone().try_acquire_owned() {
    Ok(permit) => permit,
    Err(_) => return error(StatusCode::TOO_MANY_REQUESTS, "Too many viewer subscriptions").into_response(),
  };
  // Fixed-cadence invalidations keep shared sessions live without disclosing
  // host-wide event names, timestamps, counts, paths, or hidden session keys.
  let stream = async_stream::stream! {
    let _permit = permit;
    yield Ok::<_, Infallible>(Event::default().event("ready").data("{}"));
    let change = json!({ "changed": true, "attention_session_keys": [], "updated_session_keys": keys }).to_string();
    loop {
      tokio::select! {
        _ = state.shutdown.cancelled() => break,
        _ = tokio::time::sleep(Duration::from_secs(5)) => {},
      }
      yield Ok(Event::default().event("session-index-changed").data(change.clone()));
    }
  };
  Sse::new(stream).into_response()
}

#[cfg(test)]
mod tests {
  use super::*;
  use http_body_util::BodyExt;
  use tower::ServiceExt;

  fn request(command: &str, payload: Value) -> SharedRequest {
    SharedRequest {
      session_keys: vec!["allowed".into()],
      principal: "recipient".into(),
      command: command.into(),
      payload,
    }
  }

  #[test]
  fn subscriptions_are_scoped_to_both_session_and_principal() {
    let payload = json!({"request":{"session_key":"allowed","subscription_id":"same"}});
    let mut first = request("load_session_updates", payload.clone());
    let mut second = request("load_session_updates", payload);
    second.principal = "another".into();
    prepare(&mut first).unwrap();
    prepare(&mut second).unwrap();
    assert_ne!(
      first.payload["request"]["subscription_id"],
      second.payload["request"]["subscription_id"]
    );
    assert!(
      prepare(&mut request(
        "load_session_updates",
        json!({"request":{"session_key":"hidden","subscription_id":"same"}})
      ))
      .is_err()
    );
  }

  #[test]
  fn scope_covers_every_key_bearing_command_and_disallows_control() {
    for command in [
      "load_event_page",
      "load_event_detail",
      "load_trajectory_event_page",
      "acknowledge_session_attention",
      "get_session_input_status",
    ] {
      assert!(prepare(&mut request(command, json!({"request":{"session_key":"allowed"}}))).is_ok());
      assert!(prepare(&mut request(command, json!({"request":{"session_key":"hidden"}}))).is_err());
    }
    assert!(
      prepare(&mut request(
        "list_session_children",
        json!({"request":{"parent_session_key":"hidden"}})
      ))
      .is_err()
    );
    for command in [
      "submit_session_input",
      "retry_session_index",
      "configure_relay",
      "shared",
    ] {
      assert!(prepare(&mut request(command, json!({}))).is_err());
    }
  }

  #[test]
  fn leases_are_recipient_scoped_and_cannot_preload_hidden_sessions() {
    let payload = json!({"request":{"view_id":"same", "session_key":"allowed", "candidate_session_keys":["allowed"]}});
    let mut first = request("update_session_view", payload.clone());
    let mut second = request("update_session_view", payload);
    second.principal = "another-recipient".into();
    prepare(&mut first).unwrap();
    prepare(&mut second).unwrap();
    assert_ne!(
      first.payload["request"]["view_id"],
      second.payload["request"]["view_id"]
    );
    assert!(
      prepare(&mut request(
        "update_session_view",
        json!({"request":{"view_id":"x", "candidate_session_keys":["hidden"]}})
      ))
      .is_err()
    );
  }

  #[tokio::test]
  async fn shared_endpoint_keeps_local_auth_and_never_emits_global_events() {
    let data = tempfile::tempdir().unwrap();
    let service = ViewerService::native(data.path().join("index.sqlite")).unwrap();
    let identity = br#"{"version":1,"provider":"codex","session_id":"shared","source_path":"/fixtures/shared.jsonl"}"#;
    let key = format!(
      "session.v1.{}",
      identity.iter().map(|byte| format!("{byte:02x}")).collect::<String>()
    );
    let (events, _) = broadcast::channel(16);
    let app = router(
      service,
      events.clone(),
      Some("local-secret".into()),
      vec![],
      CancellationToken::new(),
    );
    let request = |command: &str, token: bool, payload: Value| {
      let mut request = Request::builder()
        .method("POST")
        .uri("/api/v1/shared")
        .header("content-type", "application/json");
      if token {
        request = request.header("authorization", "Bearer local-secret");
      }
      request
        .body(axum::body::Body::from(
          json!({
            "session_keys": [key.clone()], "principal": "recipient", "command": command, "payload": payload,
          })
          .to_string(),
        ))
        .unwrap()
    };
    assert_eq!(
      app
        .clone()
        .oneshot(request("health", false, json!({})))
        .await
        .unwrap()
        .status(),
      StatusCode::UNAUTHORIZED
    );
    let denied = app
      .clone()
      .oneshot(request(
        "load_event_page",
        true,
        json!({"request":{"session_key":"hidden"}}),
      ))
      .await
      .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let list = app
      .clone()
      .oneshot(request("list_sessions", true, json!({})))
      .await
      .unwrap();
    let value: Value = serde_json::from_slice(&list.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
      value,
      json!({"sessions":[],"next_cursor":null,"pending_providers":[],"source_errors":[]})
    );
    let mut stream = app
      .oneshot(request("events", true, json!({})))
      .await
      .unwrap()
      .into_body();
    let ready = stream.frame().await.unwrap().unwrap().into_data().unwrap();
    assert!(std::str::from_utf8(&ready).unwrap().contains("event: ready"));
    // There is deliberately no subscription to the global event broadcaster.
    let _ = events.send(ViewerEvent {
      event: "relay-status".into(),
      payload: json!({"secret":"hidden-path"}),
    });
    assert!(
      tokio::time::timeout(Duration::from_millis(50), stream.frame())
        .await
        .is_err()
    );
  }
}
