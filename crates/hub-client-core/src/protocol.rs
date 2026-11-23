//! Versioned, bounded messages carried by the authenticated host tunnel.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
pub const VIEWER_API_VERSION: u64 = 3;
pub const MAX_BODY: usize = 1024 * 1024;
pub const CHUNK_SIZE: usize = 32 * 1024;
pub const MAX_FRAME: usize = 2 * 1024 * 1024;
pub const MAX_REQUESTS: usize = 32;
pub const RESPONSE_WINDOW: usize = 8;
pub const TUNNEL_PATH: &str = "/hub/v1/tunnel";
pub const MAX_SECURE_RECORD: usize = 65_535;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Frame {
  /// Advisory online capability, sent only on the authenticated host socket.
  /// The connector independently rejects plaintext regardless of this flag.
  SecureOnly {},
  Challenge {
    version: u32,
    nonce: String,
  },
  /// Public routing registration for a host that enforces device authorization
  /// and encryption itself. Possession of this identity grants no viewer access.
  Register {
    version: u32,
    host_id: String,
    public_key: String,
    name: String,
    allow_control: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    discovery_group: Option<crate::discovery::DiscoveryGroup>,
    signature: String,
  },
  /// Advisory registration failure. Never changes endpoint-owned trust.
  RegistrationRejected {
    message: String,
  },
  Ready {
    host_id: String,
    allow_control: bool,
  },
  /// Opaque, end-to-end authenticated channels. The Hub never parses records.
  SecureOpen {
    channel_id: u64,
  },
  SecureData {
    channel_id: u64,
    data: String,
  },
  SecureClose {
    channel_id: u64,
  },
}

pub fn encode(bytes: &[u8]) -> String {
  URL_SAFE_NO_PAD.encode(bytes)
}

pub fn decode(value: &str, maximum: usize) -> Result<Vec<u8>, String> {
  if value.len() > maximum.div_ceil(3) * 4 {
    return Err("Encoded tunnel payload is too large".into());
  }
  let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|_| "Invalid tunnel base64")?;
  if bytes.len() > maximum {
    return Err("Tunnel payload is too large".into());
  }
  Ok(bytes)
}

pub fn registration_proof(nonce: &str, host_id: &str, public_key: &str, name: &str, allow_control: bool) -> Vec<u8> {
  serde_json::to_vec(&(
    "tokn-hub-device-host-v1",
    nonce,
    host_id,
    public_key,
    name,
    allow_control,
  ))
  .expect("string tuple")
}

pub fn discovery_registration_proof(
  nonce: &str,
  host_id: &str,
  public_key: &str,
  name: &str,
  allow_control: bool,
  discovery_group: Option<&crate::discovery::DiscoveryGroup>,
) -> Vec<u8> {
  match discovery_group {
    None => registration_proof(nonce, host_id, public_key, name, allow_control),
    Some(group) => serde_json::to_vec(&(
      "tokn-hub-discovery-host-v1",
      nonce,
      host_id,
      public_key,
      name,
      allow_control,
      group,
    ))
    .expect("validated discovery registration tuple"),
  }
}

pub fn valid_host_uuid(host_id: &str) -> bool {
  uuid::Uuid::parse_str(host_id).is_ok_and(|uuid| uuid.get_version_num() == 4 && uuid.to_string() == host_id)
}

/// Exact paths prevent the gateway becoming a general loopback HTTP proxy.
/// Some POSTs update viewer bookkeeping, but only input/answer submissions execute
/// agent input and requires the connector's explicit control permission.
pub fn allowed_route(method: &str, path: &str, allow_control: bool) -> bool {
  match (method, path) {
    ("GET", "/api/v1/health" | "/api/v1/events") => true,
    (
      "POST",
      "/api/v1/submit_session_input"
      | "/api/v1/submit_session_question_answers"
      | "/api/v1/respond_session_interaction"
      | "/api/v1/set_session_control"
      | "/api/v1/cancel_session_turn"
      | "/api/v1/upload_image_chunk"
      | "/api/v1/release_image_upload",
    ) => allow_control,
    (
      "POST",
      "/api/v1/events"
      | "/api/v1/list_sessions"
      | "/api/v1/list_session_children"
      | "/api/v1/load_event_page"
      | "/api/v1/open_session"
      | "/api/v1/release_session"
      | "/api/v1/subscribe_background_updates"
      | "/api/v1/load_session_details"
      | "/api/v1/inspect_session_event"
      | "/api/v1/renew_session_subscriptions"
      | "/api/v1/renew_background_subscriptions"
      | "/api/v1/update_session_view"
      | "/api/v1/load_event_detail"
      | "/api/v1/load_session_image"
      | "/api/v1/load_trajectory_event_page"
      | "/api/v1/acknowledge_session_attention"
      | "/api/v1/get_session_index_progress"
      | "/api/v1/retry_session_index"
      | "/api/v1/get_relay_status"
      | "/api/v1/get_machine_settings"
      | "/api/v1/get_session_input_status"
      | "/api/v1/get_session_question_status"
      | "/api/v1/get_session_model_status"
      | "/api/v1/get_session_control"
      | "/api/v1/list_session_interactions"
      | "/api/v1/list_session_models",
    ) => true,
    _ => false,
  }
}

/// Runtime configuration/lifecycle is a separate host administration capability.
/// Keep it out of `allowed_route`: paired devices must never inherit process
/// administration from their agent-input permission.
pub fn runtime_management_route(method: &str, path: &str) -> bool {
  method == "POST"
    && matches!(
      path,
      "/api/v1/get_rpc_runtime_profiles"
        | "/api/v1/validate_rpc_runtime_profile"
        | "/api/v1/save_rpc_runtime_profile"
        | "/api/v1/delete_rpc_runtime_profile"
        | "/api/v1/list_rpc_runtimes"
        | "/api/v1/start_rpc_runtime"
        | "/api/v1/close_rpc_runtime"
    )
}

/// View-only hosts keep recorded model context but cannot discover a live
/// runtime's catalog or advertise controls that the client cannot use.
pub fn read_only_model_response(path: &str, reason: &str) -> Option<serde_json::Value> {
  if !matches!(path, "/api/v1/get_session_model_status" | "/api/v1/list_session_models") {
    return None;
  }
  let status = serde_json::json!({
    "state":{"model":null,"source":"unknown","revision":"read_only"},
    "can_select":false,"can_list":false,"reason":reason,
    "runtime_generation":null,"updates_pi_default":false,"supports_model_input":false
  });
  Some(if path.ends_with("/list_session_models") {
    serde_json::json!({"status":status,"models":[]})
  } else {
    status
  })
}

/// A viewing client cannot discover the selected live route or runtime IDs.
pub fn read_only_session_control_response(path: &str, body: &[u8], reason: &str) -> Option<serde_json::Value> {
  if !matches!(
    path,
    "/api/v1/get_session_control" | "/api/v1/list_session_interactions"
  ) {
    return None;
  }
  let payload: serde_json::Value = serde_json::from_slice(body).ok()?;
  let session_key = payload.get("request")?.get("session_key")?.as_str()?;
  if path == "/api/v1/list_session_interactions" {
    return Some(serde_json::json!({
      "session_key":session_key, "runtime_id":null, "instance_id":null, "interactions":[]
    }));
  }
  Some(serde_json::json!({
    "session_key":session_key,"provider":"codex","transport":"rpc",
    "available":false,"can_select":false,"can_cancel":false,"message":reason,
    "runtime_id":null,"instance_id":null
  }))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn legacy_authentication_and_plaintext_frames_are_rejected() {
    for frame in [
      serde_json::json!({"type":"authenticate","version":1,"public_key":"key","name":"Host","allow_control":false,"signature":"signature"}),
      serde_json::json!({"type":"pending","code":"123456","expires_in":60}),
      serde_json::json!({"type":"request","request_id":1,"method":"GET","path":"/api/v1/health","body":""}),
      serde_json::json!({"type":"response","request_id":1,"status":200,"content_type":"application/json"}),
      serde_json::json!({"type":"chunk","request_id":1,"data":"e30"}),
      serde_json::json!({"type":"end","request_id":1}),
      serde_json::json!({"type":"error","request_id":1,"message":"failed"}),
      serde_json::json!({"type":"cancel","request_id":1}),
      serde_json::json!({"type":"window","request_id":1,"credits":1}),
    ] {
      assert!(serde_json::from_value::<Frame>(frame.clone()).is_err(), "{frame}");
    }
  }

  #[test]
  fn session_control_uses_agent_permission_and_viewing_never_discloses_the_live_route() {
    let path = "/api/v1/get_session_control";
    assert!(allowed_route("POST", path, false));
    assert!(!runtime_management_route("POST", path));
    let response =
      read_only_session_control_response(path, br#"{"request":{"session_key":"one"}}"#, "Viewing only").unwrap();
    assert_eq!(response["session_key"], "one");
    assert_eq!(response["available"], false);
    assert_eq!(response["can_select"], false);
    assert_eq!(response["can_cancel"], false);
    assert!(response["runtime_id"].is_null());
    assert!(response["instance_id"].is_null());
    let interactions = "/api/v1/list_session_interactions";
    assert!(allowed_route("POST", interactions, false));
    assert_eq!(
      read_only_session_control_response(interactions, br#"{"request":{"session_key":"one"}}"#, "Viewing only"),
      Some(serde_json::json!({"session_key":"one", "runtime_id":null, "instance_id":null, "interactions":[]}))
    );
    for path in [
      "/api/v1/set_session_control",
      "/api/v1/cancel_session_turn",
      "/api/v1/respond_session_interaction",
    ] {
      assert!(allowed_route("POST", path, true));
      assert!(!allowed_route("POST", path, false));
      assert!(!allowed_route("GET", path, true));
      assert!(!allowed_route("POST", &format!("{path}/"), true));
      assert!(!runtime_management_route("POST", path));
      assert!(read_only_session_control_response(path, b"{}", "Viewing only").is_none());
    }
  }

  #[test]
  fn runtime_routes_require_an_independent_paired_host_permission() {
    for command in [
      "get_rpc_runtime_profiles",
      "validate_rpc_runtime_profile",
      "save_rpc_runtime_profile",
      "delete_rpc_runtime_profile",
      "list_rpc_runtimes",
      "start_rpc_runtime",
      "close_rpc_runtime",
    ] {
      let path = format!("/api/v1/{command}");
      assert!(runtime_management_route("POST", &path));
      for control in [false, true] {
        assert!(!allowed_route("POST", &path, control));
      }
      for method in ["GET", "PUT", "DELETE", "OPTIONS"] {
        assert!(!runtime_management_route(method, &path));
      }
      for suffix in ["/", "?admin=true", "/../list_sessions"] {
        assert!(!runtime_management_route("POST", &format!("{path}{suffix}")));
      }
    }
    for command in ["get_rpc_runtime_access", "set_rpc_runtime_access", "shared"] {
      assert!(!runtime_management_route("POST", &format!("/api/v1/{command}")));
    }
  }

  #[test]
  fn viewing_model_controls_never_advertise_or_disclose_a_catalog() {
    for path in ["/api/v1/get_session_model_status", "/api/v1/list_session_models"] {
      let response = read_only_model_response(path, "Viewing only").unwrap();
      let status = response.get("status").unwrap_or(&response);
      assert_eq!(status["can_select"], false);
      assert_eq!(status["can_list"], false);
      assert!(status["state"]["model"].is_null());
      assert!(status["runtime_generation"].is_null());
      if path.ends_with("/list_session_models") {
        assert_eq!(response["models"], serde_json::json!([]));
      }
    }
    assert!(read_only_model_response("/api/v1/select_session_model", "Viewing only").is_none());
    assert!(!allowed_route("POST", "/api/v1/select_session_model", true));
  }

  #[test]
  fn paths_are_exact_and_control_is_explicit() {
    assert!(allowed_route("POST", "/api/v1/list_sessions", false));
    for allow_control in [false, true] {
      for path in [
        "/api/v1/open_session",
        "/api/v1/release_session",
        "/api/v1/subscribe_background_updates",
        "/api/v1/load_session_details",
        "/api/v1/inspect_session_event",
        "/api/v1/load_session_image",
        "/api/v1/renew_session_subscriptions",
        "/api/v1/renew_background_subscriptions",
        "/api/v1/get_machine_settings",
        "/api/v1/get_session_model_status",
        "/api/v1/list_session_models",
      ] {
        assert!(allowed_route("POST", path, allow_control));
        for method in ["GET", "PUT", "DELETE"] {
          assert!(!allowed_route(method, path, allow_control));
        }
        for suffix in ["/", "?x=1"] {
          assert!(!allowed_route("POST", &format!("{path}{suffix}"), allow_control));
        }
      }
    }
    for path in ["/api/v1/upload_image_chunk", "/api/v1/release_image_upload"] {
      assert!(allowed_route("POST", path, true));
      assert!(!allowed_route("POST", path, false));
      for method in ["GET", "PUT", "DELETE"] {
        assert!(!allowed_route(method, path, true));
      }
      for suffix in ["/", "?x=1"] {
        assert!(!allowed_route("POST", &format!("{path}{suffix}"), true));
      }
    }
    assert!(allowed_route("POST", "/api/v1/submit_session_input", true));
    assert!(!allowed_route("POST", "/api/v1/submit_session_input", false));
    assert!(allowed_route("POST", "/api/v1/submit_session_question_answers", true));
    assert!(!allowed_route("POST", "/api/v1/submit_session_question_answers", false));
    assert!(allowed_route("POST", "/api/v1/get_session_question_status", false));
    for path in [
      "/",
      "http://127.0.0.1/secrets",
      "//example.com",
      "/api/v1/../secret",
      "/api/v1/events?x=1",
    ] {
      assert!(!allowed_route("GET", path, true));
    }
  }

  #[test]
  fn decoding_limits_actual_bytes() {
    assert_eq!(decode(&encode(b"abc"), 3).unwrap(), b"abc");
    assert!(decode(&encode(b"abc"), 2).is_err());
  }
}
