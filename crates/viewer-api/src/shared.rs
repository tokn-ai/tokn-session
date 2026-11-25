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
  // Two grants for the same recipient can overlap without having the same
  // visibility. Their subscriptions, pushes and renewal IDs stay independent.
  share.principal = grant_namespace(&share.principal, &share.session_keys);
  if share.command == "events" {
    let prefix = serde_json::from_value::<StreamSubscription>(share.payload)
      .map_err(|_| error(StatusCode::BAD_REQUEST, "Missing stream subscription"))
      .and_then(StreamSubscription::into_prefix);
    return match prefix {
      Ok(prefix) => events(state, share.session_keys, share.principal, prefix).await,
      Err(error) => error.into_response(),
    };
  }
  let subscription_id = matches!(
    share.command.as_str(),
    "open_session" | "release_session" | "subscribe_background_updates" | "open_session_list"
  )
  .then(|| {
    share
      .payload
      .get("request")
      .and_then(|request| request.get("subscription_id"))
      .cloned()
  })
  .flatten();
  let renewal_prefix = match share.command.as_str() {
    "renew_session_subscriptions" => Some(format!("share_timeline_{}:", share.principal)),
    "renew_session_lists" => Some(format!("share_list_{}:", share.principal)),
    _ => None,
  };
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
      "health" => Ok(json!({"version": VIEWER_PROTOCOL_VERSION})),
      "get_session_input_status" => Ok(json!({
        "available": false, "message": "This session share is read-only", "max_length": 0,
        "images_available": false, "images_stageable": false, "max_images": 0, "max_image_bytes": 0,
        "max_total_image_bytes": 0, "image_mime_types": []
      })),
      "get_session_control" => Ok(json!({
        "session_key": share.payload["request"]["session_key"], "provider":"codex",
        "transport":"rpc", "available":false, "can_select":false, "can_cancel":false,
        "message":"This session share is read-only", "runtime_id":null, "instance_id":null
      })),
      "list_session_interactions" => Ok(json!({
        "session_key":share.payload["request"]["session_key"],
        "runtime_id":null, "instance_id":null, "interactions":[]
      })),
      "get_session_model_status" | "list_session_models" => {
        let status = json!({
          "state": {"model":null,"source":"unknown","revision":"read_only"},
          "can_select":false,"can_list":false,"reason":"This session share is read-only",
          "runtime_generation":null,"updates_pi_default":false,"supports_model_input":false
        });
        Ok(if share.command == "list_session_models" {
          json!({"status":status,"models":[]})
        } else {
          status
        })
      }
      "get_session_question_status" => Ok(json!({
        "available": false, "message": "This session share is read-only", "max_length": 0, "question_ids": []
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
      if let Some(prefix) = renewal_prefix {
        for field in ["renewed_ids", "missing_ids"] {
          if let Some(ids) = value.get_mut(field).and_then(Value::as_array_mut) {
            for id in ids {
              let Some(client_id) = id.as_str().and_then(|s| s.strip_prefix(&prefix)) else {
                return error(
                  StatusCode::INTERNAL_SERVER_ERROR,
                  "Invalid subscription acknowledgement",
                )
                .into_response();
              };
              *id = json!(client_id);
            }
          }
        }
      }
      if let Some(id) = subscription_id {
        value["subscription_id"] = id;
      }
      Json(value).into_response()
    }
    // An index failure can contain paths/provider details outside this share.
    // Do not relay raw diagnostics from shared service internals to guests.
    Ok(Err((StatusCode::TOO_MANY_REQUESTS, detail))) if detail.0["error"] == SESSION_LIST_CAPACITY_ERROR => {
      error(StatusCode::TOO_MANY_REQUESTS, SESSION_LIST_CAPACITY_ERROR).into_response()
    }
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
    "open_session_list" | "release_session_list" => {
      let request = request.ok_or("Missing session list request")?;
      if let Some(parent) = request.get("parent_session_key").filter(|value| !value.is_null()) {
        check(parent.as_str().ok_or("Invalid parent session key")?)?;
      }
      let id = request["subscription_id"]
        .as_str()
        .ok_or("Missing subscription identity")?;
      request["subscription_id"] = json!(scoped_id("list", &share.principal, id)?);
      Ok(())
    }
    "list_session_children"
    | "load_event_page"
    | "inspect_session_event"
    | "load_event_detail"
    | "load_trajectory_event_page"
    | "acknowledge_session_attention"
    | "get_session_input_status"
    | "get_session_question_status"
    | "get_session_model_status"
    | "get_session_control"
    | "list_session_interactions"
    | "list_session_models"
    | "load_session_image" => {
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
    "load_session_details" => {
      let request = request.ok_or("Missing tool detail request")?;
      if request["kind"] != "tool" {
        return Err("Invalid tool detail request");
      }
      check(
        request["request"]["session_key"]
          .as_str()
          .ok_or("Missing session key")?,
      )
    }
    "open_session" | "release_session" => {
      let request = request.ok_or("Missing subscription request")?;
      check(request["session_key"].as_str().ok_or("Missing session key")?)?;
      let scope_identity = |request: &mut Value| -> Result<(), &'static str> {
        let id = request["subscription_id"]
          .as_str()
          .ok_or("Missing subscription identity")?;
        request["subscription_id"] = json!(subscription_id(&share.principal, id)?);
        Ok(())
      };
      scope_identity(request)?;
      if let Some(resume) = request.get_mut("resume").filter(|value| !value.is_null()) {
        scope_identity(resume)?;
      }
      Ok(())
    }
    "subscribe_background_updates" => {
      let request = request
        .and_then(Value::as_object_mut)
        .ok_or("Missing background subscription request")?;
      match request.get("scope").and_then(Value::as_str) {
        Some("all") | None => {
          // All means all sessions in this grant, never the host's catalog.
          request.insert("scope".into(), json!("sidebar"));
          request.insert("session_keys".into(), json!(share.session_keys));
        }
        Some("sidebar") => {
          if let Some(keys) = request.get("session_keys") {
            for key in keys.as_array().ok_or("Invalid session keys")? {
              check(key.as_str().ok_or("Invalid session key")?)?;
            }
          }
        }
        _ => return Err("Invalid background subscription scope"),
      }
      let id = request
        .get("subscription_id")
        .and_then(Value::as_str)
        .ok_or("Missing subscription identity")?;
      request.insert("subscription_id".into(), json!(background_id(&share.principal, id)?));
      Ok(())
    }
    "renew_background_subscriptions" | "renew_session_subscriptions" | "renew_session_lists" => {
      let (family, max_ids) = match share.command.as_str() {
        "renew_session_lists" => ("list", 64),
        "renew_background_subscriptions" => ("bg", 24),
        _ => ("timeline", 24),
      };
      let ids = share
        .payload
        .get_mut("ids")
        .and_then(Value::as_array_mut)
        .ok_or("Missing subscription identities")?;
      if ids.len() > max_ids {
        return Err("Too many subscriptions");
      }
      for id in ids {
        *id = json!(scoped_id(
          family,
          &share.principal,
          id.as_str().ok_or("Invalid subscription identity")?
        )?);
      }
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

fn grant_namespace(principal: &str, keys: &[String]) -> String {
  let mut keys: Vec<_> = keys.iter().map(String::as_str).collect();
  keys.sort_unstable();
  keys.dedup();
  let mut digest = Sha256::new();
  for part in std::iter::once(principal).chain(keys) {
    digest.update((part.len() as u64).to_be_bytes());
    digest.update(part.as_bytes());
  }
  format!("{:x}", digest.finalize())
}

fn scoped_id(namespace: &str, principal: &str, id: &str) -> Result<String, &'static str> {
  if id.is_empty() || id.len() > 128 {
    return Err("Invalid subscription identity");
  }
  Ok(format!("share_{namespace}_{principal}:{id}"))
}

fn subscription_id(principal: &str, id: &str) -> Result<String, &'static str> {
  scoped_id("timeline", principal, id)
}

fn background_id(principal: &str, id: &str) -> Result<String, &'static str> {
  scoped_id("bg", principal, id)
}

// Check both the session summaries and tombstones against the grant.
fn list_event_in_scope(payload: &Value, keys: &[String]) -> bool {
  let allowed = |key: &Value| {
    key
      .as_str()
      .is_some_and(|key| keys.iter().any(|grant_key| grant_key == key))
  };
  payload["sessions"]
    .as_array()
    .is_some_and(|sessions| sessions.iter().all(|entry| allowed(&entry["session"]["session_key"])))
    && payload["removed_keys"]
      .as_array()
      .is_some_and(|keys| keys.iter().all(allowed))
}

fn shared_event(mut event: ViewerEvent, keys: &[String], principal: &str, prefix: &str) -> Option<ViewerEvent> {
  let family = match event.event.as_str() {
    "session-updated" | "session-subscription-invalidated" => "timeline",
    "background-session-updated" => "bg",
    "session-list-updated" => "list",
    _ => return None,
  };
  // The core reserves the share_ namespace for grant-scoped services.
  let id = event.payload["subscription_id"]
    .as_str()?
    .strip_prefix(&format!("share_{family}_{principal}:"))?;
  if family == "list" {
    if !list_event_in_scope(&event.payload, keys) {
      return None;
    }
  } else {
    let key = event.payload["session_key"].as_str()?;
    if !keys.iter().any(|allowed| allowed == key) {
      return None;
    }
  }
  event.payload["subscription_id"] = json!(id);
  if family == "bg" {
    event.payload["has_unread"] = json!(false);
    event.payload["unread_final_count"] = json!(0);
    event.payload["has_running_descendant"] = json!(false);
  }
  stream_event(event, Some(prefix))
}

async fn events(state: ApiState, keys: Vec<String>, principal: String, prefix: String) -> Response {
  let permit = match state.subscribers.clone().try_acquire_owned() {
    Ok(permit) => permit,
    Err(_) => return error(StatusCode::TOO_MANY_REQUESTS, "Too many viewer subscriptions").into_response(),
  };
  let mut receiver = state.events.subscribe();
  let list_prefix = format!("share_list_{principal}:{prefix}:");
  let stream = async_stream::stream! {
    let _permit = permit;
    yield Ok::<_, Infallible>(Event::default().event("ready").data("{}"));
    loop {
      tokio::select! {
        _ = state.shutdown.cancelled() => break,
        received = receiver.recv() => {
          let Ok(event) = received else { break; };
          for event in event.for_subscriptions(|id| id.starts_with(&list_prefix)) {
            if let Some(event) = shared_event(event, &keys, &principal, &prefix) {
              yield Ok(Event::default().event(event.event).data(event.payload.to_string()));
            }
          }
        },
      }
    }
  };
  Sse::new(stream)
    .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
    .into_response()
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
    let payload = json!({"request":{"session_key":"allowed","subscription_id":"same",
      "resume":{"subscription_id":"previous","generation":"baseline","revision":"2","state_hash":"a".repeat(64)}}});
    let mut first = request("open_session", payload.clone());
    let mut second = request("open_session", payload);
    second.principal = "another".into();
    prepare(&mut first).unwrap();
    prepare(&mut second).unwrap();
    assert_ne!(
      first.payload["request"]["resume"]["subscription_id"],
      second.payload["request"]["resume"]["subscription_id"]
    );
    let mut renewal = request("renew_session_subscriptions", json!({"ids":["previous"]}));
    prepare(&mut renewal).unwrap();
    assert_eq!(
      first.payload["request"]["resume"]["subscription_id"],
      renewal.payload["ids"][0]
    );
    let keys = vec!["one".to_owned(), "two".to_owned()];
    assert_eq!(
      grant_namespace("guest", &keys),
      grant_namespace("guest", &["two".into(), "one".into()])
    );
    assert_ne!(grant_namespace("guest", &keys), grant_namespace("guest", &keys[..1]));
    assert_ne!(
      first.payload["request"]["subscription_id"],
      second.payload["request"]["subscription_id"]
    );
    assert!(
      prepare(&mut request(
        "open_session",
        json!({"request":{"session_key":"hidden","subscription_id":"same"}})
      ))
      .is_err()
    );
  }

  #[test]
  fn background_subscriptions_and_renewals_keep_principal_and_session_scope() {
    let payload = json!({"request":{"scope":"sidebar","session_keys":["allowed"],"subscription_id":"same"}});
    let mut first = request("subscribe_background_updates", payload.clone());
    let mut second = request("subscribe_background_updates", payload);
    second.principal = "another".into();
    prepare(&mut first).unwrap();
    prepare(&mut second).unwrap();
    assert_ne!(
      first.payload["request"]["subscription_id"],
      second.payload["request"]["subscription_id"]
    );
    let mut renewal = request("renew_background_subscriptions", json!({"ids":["same"]}));
    prepare(&mut renewal).unwrap();
    assert_eq!(renewal.payload["ids"][0], first.payload["request"]["subscription_id"]);
    assert!(
      prepare(&mut request(
        "subscribe_background_updates",
        json!({
          "request":{"scope":"sidebar","session_keys":["hidden"],"subscription_id":"same"}
        })
      ))
      .is_err()
    );
    let mut all = request(
      "subscribe_background_updates",
      json!({"request":{"scope":"all","subscription_id":"same"}}),
    );
    prepare(&mut all).unwrap();
    assert_eq!(all.payload["request"]["scope"], "sidebar");
    assert_eq!(all.payload["request"]["session_keys"], json!(["allowed"]));
  }

  #[test]
  fn shared_events_require_the_exact_grant_session_family_and_viewer() {
    let keys = vec!["allowed".to_owned()];
    let timeline = subscription_id("grant", "screen:timeline").unwrap();
    let background = background_id("grant", "screen:sidebar").unwrap();
    let event = |name: &str, id: &str, key: &str| {
      ViewerEvent::new(
        name,
        json!({"subscription_id":id,"session_key":key,"has_unread":true,"unread_final_count":3,
        "has_running_descendant":true,"snapshot":false}),
      )
    };
    let bg = shared_event(
      event("background-session-updated", &background, "allowed"),
      &keys,
      "grant",
      "screen",
    )
    .unwrap();
    assert_eq!(bg.payload["subscription_id"], "screen:sidebar");
    assert_eq!(bg.payload["has_unread"], false);
    assert_eq!(bg.payload["unread_final_count"], 0);
    assert_eq!(bg.payload["has_running_descendant"], false);
    let rich = event("session-updated", &timeline, "allowed");
    assert!(shared_event(rich.clone(), &keys, "other", "screen").is_none());
    assert!(shared_event(rich.clone(), &keys, "grant", "other-screen").is_none());
    assert_eq!(
      shared_event(rich.clone(), &keys, "grant", "screen").unwrap().payload["subscription_id"],
      "screen:timeline"
    );
    let mut reset = rich;
    reset.payload["snapshot"] = json!(true);
    let notice = shared_event(reset, &keys, "grant", "screen").unwrap();
    assert_eq!(notice.event, "session-subscriptions-lost");
    assert_eq!(notice.payload, json!({"subscription_ids":["screen:timeline"]}));
    let invalidated = event("session-subscription-invalidated", &timeline, "allowed");
    assert!(stream_event(invalidated.clone(), None).is_none());
    assert!(stream_event(invalidated.clone(), Some("screen")).is_none());
    assert!(shared_event(invalidated.clone(), &keys, "grant", "other-screen").is_none());
    let invalidated = shared_event(invalidated, &keys, "grant", "screen").unwrap();
    assert_eq!(invalidated.event, notice.event);
    assert_eq!(invalidated.payload, notice.payload);
    for (name, key, id) in [
      ("session-notification", "allowed", background.as_str()),
      ("session-updated", "hidden", timeline.as_str()),
      ("session-updated", "allowed", background.as_str()),
      ("background-session-updated", "allowed", timeline.as_str()),
    ] {
      assert!(shared_event(event(name, id, key), &keys, "grant", "screen").is_none());
    }
  }

  #[test]
  fn list_subscriptions_route_each_session_and_removal_only_within_its_grant() {
    let keys = vec!["allowed".to_owned(), "second".to_owned()];
    let client_id = "screen:root";
    let id = scoped_id("list", "grant", client_id).unwrap();
    let payload = json!({
      "subscription_id":id,"sessions":[{"revision":"a".repeat(64),"session":{"session_key":"allowed","title":"changed"}}],"removed_keys":["second"],
    });
    let event = |payload| ViewerEvent::new("session-list-updated", payload);
    let accepted = shared_event(event(payload.clone()), &keys, "grant", "screen").unwrap();
    assert_eq!(accepted.payload["subscription_id"], client_id);
    assert_eq!(accepted.payload["sessions"], payload["sessions"]);
    assert!(shared_event(event(payload.clone()), &keys, "other-grant", "screen").is_none());
    assert!(shared_event(event(payload.clone()), &keys, "grant", "another-screen").is_none());
    for field in ["/sessions/0/session/session_key", "/removed_keys/0"] {
      let mut hidden = payload.clone();
      *hidden.pointer_mut(field).unwrap() = json!("private");
      assert!(shared_event(event(hidden), &keys, "grant", "screen").is_none());
    }
    for command in ["open_session_list", "release_session_list"] {
      let mut registered = request(command, json!({"request":{"subscription_id":client_id}}));
      let expected = scoped_id("list", &registered.principal, client_id).unwrap();
      prepare(&mut registered).unwrap();
      assert_eq!(registered.payload["request"]["subscription_id"], expected);
    }
    assert!(
      prepare(&mut request(
        "open_session_list",
        json!({
          "request":{"subscription_id":client_id,"parent_session_key":"private"},
        })
      ))
      .is_err()
    );
  }

  #[test]
  fn scope_covers_every_key_bearing_command_and_disallows_control() {
    for command in [
      "load_event_page",
      "inspect_session_event",
      "load_event_detail",
      "load_trajectory_event_page",
      "acknowledge_session_attention",
      "get_session_input_status",
      "get_session_question_status",
      "get_session_model_status",
      "get_session_control",
      "list_session_interactions",
      "list_session_models",
      "load_session_image",
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
      "select_session_model",
      "submit_session_input",
      "submit_session_question_answers",
      "respond_session_interaction",
      "set_session_control",
      "cancel_session_turn",
      "upload_image_chunk",
      "release_image_upload",
      "retry_session_index",
      "configure_relay",
      "get_machine_settings",
      "get_rpc_runtime_profiles",
      "validate_rpc_runtime_profile",
      "save_rpc_runtime_profile",
      "delete_rpc_runtime_profile",
      "list_rpc_runtimes",
      "start_rpc_runtime",
      "close_rpc_runtime",
      "get_rpc_runtime_access",
      "set_rpc_runtime_access",
      "list_registered_projects",
      "get_session_creation_options",
      "list_session_creation_models",
      "register_project",
      "create_project_session",
      "get_managed_session",
      "stop_managed_session",
      "resume_managed_session",
      "respond_managed_session_dialog",
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

  #[tokio::test(start_paused = true)]
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
    let status = app
      .clone()
      .oneshot(request(
        "get_session_input_status",
        true,
        json!({"request":{"session_key":key.clone()}}),
      ))
      .await
      .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let status: Value = serde_json::from_slice(&status.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(status["images_available"], false);
    for field in ["max_images", "max_image_bytes", "max_total_image_bytes"] {
      assert_eq!(status[field], 0);
    }
    assert_eq!(status["image_mime_types"], json!([]));
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
    let list_request = json!({"request":{"subscription_id":"screen:root"}});
    let list = app
      .clone()
      .oneshot(request("open_session_list", true, list_request.clone()))
      .await
      .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let baseline: Value = serde_json::from_slice(&list.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(baseline["subscription_id"], "screen:root");
    assert_eq!(baseline.get("base_cursor"), Some(&Value::Null));
    assert!(
      baseline["synced_through"]
        .as_str()
        .is_some_and(|cursor| !cursor.is_empty())
    );
    assert_eq!(baseline["sessions"], json!([]));
    let resumed = app
      .clone()
      .oneshot(request(
        "open_session_list",
        true,
        json!({"request":{
          "subscription_id":"screen:root","since":baseline["synced_through"]
        }}),
      ))
      .await
      .unwrap();
    assert_eq!(resumed.status(), StatusCode::OK);
    let resumed: Value = serde_json::from_slice(&resumed.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(resumed["base_cursor"], baseline["synced_through"]);
    assert_eq!(resumed["synced_through"], baseline["synced_through"]);
    assert_eq!(resumed["sessions"], json!([]));
    let renew_lists = json!({"ids":["screen:root","other"]});
    for (command, payload, expected) in [
      (
        "renew_session_lists",
        renew_lists.clone(),
        json!({"renewed_ids":["screen:root"],"missing_ids":["other"]}),
      ),
      ("release_session_list", list_request, json!(true)),
      (
        "renew_session_lists",
        renew_lists,
        json!({"renewed_ids":[],"missing_ids":["screen:root","other"]}),
      ),
    ] {
      let result = app.clone().oneshot(request(command, true, payload)).await.unwrap();
      assert_eq!(result.status(), StatusCode::OK);
      assert_eq!(
        serde_json::from_slice::<Value>(&result.into_body().collect().await.unwrap().to_bytes()).unwrap(),
        expected
      );
    }
    let subscribed = app
      .clone()
      .oneshot(request(
        "subscribe_background_updates",
        true,
        json!({
          "request":{"scope":"all","subscription_id":"screen:mine"}
        }),
      ))
      .await
      .unwrap();
    assert_eq!(subscribed.status(), StatusCode::OK);
    let value: Value = serde_json::from_slice(&subscribed.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(value["subscription_id"], "screen:mine");
    assert_eq!(value["revision"], "0");
    assert_eq!(value["scope"], "sidebar");
    assert!(value.get("items").is_none());
    let mut stream = app
      .oneshot(request("events", true, json!({"subscription_prefix":"screen"})))
      .await
      .unwrap()
      .into_body();
    let ready = stream.frame().await.unwrap().unwrap().into_data().unwrap();
    assert!(std::str::from_utf8(&ready).unwrap().contains("event: ready"));
    // An idle guest sees only a transport keepalive even after the former
    // five-second refresh interval has passed multiple times.
    let idle = tokio::time::timeout(Duration::from_secs(16), stream.frame())
      .await
      .unwrap()
      .unwrap()
      .unwrap()
      .into_data()
      .unwrap();
    assert!(idle.starts_with(b":"));
    // The broadcaster is filtered before any host-wide data reaches guests.
    let _ = events.send(ViewerEvent::new("relay-status", json!({"secret":"hidden-path"})));
    assert!(
      tokio::time::timeout(Duration::from_millis(50), stream.frame())
        .await
        .is_err()
    );
    events
      .send(ViewerEvent::new(
        "background-session-updated",
        json!({"subscription_id":background_id(&grant_namespace("recipient", std::slice::from_ref(&key)), "screen:mine").unwrap(),
        "session_key":key,"latest_assistant_message":{"preview":"Checking schema"},
        "has_unread":true,"unread_final_count":3,"has_running_descendant":true}),
      ))
      .unwrap();
    let next = tokio::time::timeout(Duration::from_secs(1), stream.frame())
      .await
      .unwrap()
      .unwrap()
      .unwrap()
      .into_data()
      .unwrap();
    let next = std::str::from_utf8(&next).unwrap();
    assert!(next.contains("event: background-session-updated"));
    assert!(next.contains("\"subscription_id\":\"screen:mine\""));
    assert!(next.contains("Checking schema"));
    assert!(next.contains("\"has_unread\":false"));
    assert!(!next.contains("share_bg_"));
  }

  #[tokio::test]
  async fn grouped_list_updates_emit_only_the_matching_owner_or_grant_routes() {
    use tokn_viewer_core::session_list::SessionListUpdate;

    async fn frame(body: &mut axum::body::Body) -> String {
      let frame = tokio::time::timeout(Duration::from_secs(1), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
      String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap()
    }
    let data = tempfile::tempdir().unwrap();
    let service = ViewerService::native(data.path().join("index.sqlite")).unwrap();
    let (events, _) = broadcast::channel(4);
    let app = router(service, events.clone(), None, vec![], CancellationToken::new());
    let key = format!(
      "session.v1.{}",
      br#"{"version":1,"provider":"pi","session_id":"fixture","source_path":"/fixtures/fixture.jsonl"}"#
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
    );
    let principal = grant_namespace("guest", std::slice::from_ref(&key));
    let post = |path: &str, data: Value| {
      Request::post(path)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(data.to_string()))
        .unwrap()
    };
    let mut owner = app
      .clone()
      .oneshot(post("/api/v1/events", json!({"subscription_prefix":"screen"})))
      .await
      .unwrap()
      .into_body();
    let mut guest = app.oneshot(post("/api/v1/shared", json!({
      "command":"events", "session_keys":[key.clone()], "principal":"guest", "payload":{"subscription_prefix":"screen"}
    }))).await.unwrap().into_body();
    for stream in [&mut owner, &mut guest] {
      assert!(frame(stream).await.starts_with("event: ready\n"));
    }
    let change = SessionListUpdate {
      subscription_id: String::new(),
      base_cursor: Some("before".into()),
      synced_through: "next".into(),
      sessions: vec![],
      removed_keys: vec![key.clone()],
      next_cursor: None,
      pending_providers: vec![],
      source_errors: vec![],
    };
    let granted = |id: &str| scoped_id("list", &principal, id).unwrap();
    let send = |ids, change: &SessionListUpdate| {
      events
        .send(ViewerEvent::session_list_batch(ids, change, false).unwrap())
        .unwrap();
    };
    // A valid grant route must still check the payload's allowed keys.
    send(
      vec![granted("screen:secret")],
      &SessionListUpdate {
        removed_keys: vec!["private".into()],
        ..change.clone()
      },
    );
    send(
      vec![
        "screen:first".into(),
        granted("screen:guest-one"),
        "screen-extra:foreign".into(),
        scoped_id("list", "foreign", "screen:foreign-grant").unwrap(),
        granted("screen-extra:foreign"),
        "screen:second".into(),
        granted("screen:guest-two"),
      ],
      &change,
    );
    for (stream, expected) in [
      (&mut owner, ["screen:first", "screen:second"]),
      (&mut guest, ["screen:guest-one", "screen:guest-two"]),
    ] {
      let mut ids = Vec::new();
      for _ in 0..expected.len() {
        let message = frame(stream).await;
        let data = message
          .strip_prefix("event: session-list-updated\ndata: ")
          .unwrap()
          .trim();
        let payload: Value = serde_json::from_str(data).unwrap();
        let id = payload["subscription_id"].as_str().unwrap().to_owned();
        let mut visible = serde_json::to_value(&change).unwrap();
        visible["subscription_id"] = json!(id);
        assert_eq!(payload, visible);
        ids.push(id);
      }
      ids.sort();
      assert_eq!(ids, expected);
      assert!(
        tokio::time::timeout(Duration::from_millis(50), stream.frame())
          .await
          .is_err()
      );
    }
  }

  #[tokio::test]
  async fn full_live_list_store_returns_the_static_list_fallback_signal_to_guests() {
    let data = tempfile::tempdir().unwrap();
    let service = ViewerService::native(data.path().join("index.sqlite")).unwrap();
    for offset in 0..64 {
      let request =
        serde_json::from_value(json!({"subscription_id":format!("owner:{offset}"),"offset":offset})).unwrap();
      service.open_session_list(request).unwrap();
    }
    // All distinct view slots are now occupied. Another viewer of the
    // existing first page shares its state without consuming another slot.
    let another = serde_json::from_value(json!({"subscription_id":"second:owner"})).unwrap();
    assert!(service.open_session_list(another).is_ok());
    let (events, _) = broadcast::channel(4);
    let app = router(service, events, None, vec![], CancellationToken::new());
    let identity = br#"{"version":1,"provider":"pi","session_id":"fixture","source_path":"/fixtures/one.jsonl"}"#;
    let key = format!(
      "session.v1.{}",
      identity.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    let result = app.oneshot(Request::post("/api/v1/shared")
      .header("content-type","application/json")
      .body(axum::body::Body::from(json!({
        "session_keys":[key],"principal":"reader","command":"open_session_list","payload":{"request":{"subscription_id":"screen:list"}}
      }).to_string())).unwrap()).await.unwrap();
    assert_eq!(result.status(), StatusCode::TOO_MANY_REQUESTS);
    let error: Value = serde_json::from_slice(&result.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(error, json!({"error":SESSION_LIST_CAPACITY_ERROR}));
  }
}
