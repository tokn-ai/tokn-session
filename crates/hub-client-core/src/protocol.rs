//! Versioned, bounded messages carried by the authenticated host tunnel.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
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
  Authenticate {
    version: u32,
    public_key: String,
    name: String,
    allow_control: bool,
    signature: String,
  },
  /// Public routing registration for a host that enforces device authorization
  /// and encryption itself. Possession of this identity grants no viewer access.
  Register {
    version: u32,
    host_id: String,
    public_key: String,
    name: String,
    allow_control: bool,
    signature: String,
  },
  /// Advisory registration failure. Never changes endpoint-owned trust.
  RegistrationRejected {
    message: String,
  },
  Pending {
    code: String,
    expires_in: u64,
  },
  Ready {
    host_id: String,
    allow_control: bool,
  },
  Request {
    request_id: u64,
    method: String,
    path: String,
    body: String,
  },
  Response {
    request_id: u64,
    status: u16,
    content_type: Option<String>,
  },
  Chunk {
    request_id: u64,
    data: String,
  },
  End {
    request_id: u64,
  },
  Error {
    request_id: u64,
    message: String,
  },
  Cancel {
    request_id: u64,
  },
  Window {
    request_id: u64,
    credits: usize,
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

/// Bind the proof to all enrollment fields, with a protocol-specific domain.
pub fn proof(nonce: &str, public_key: &str, name: &str, allow_control: bool) -> Vec<u8> {
  serde_json::to_vec(&("tokn-hub-host-v1", nonce, public_key, name, allow_control)).expect("string tuple")
}

pub fn host_id(public_key: &[u8; 32]) -> String {
  format!("host_{}", encode(public_key))
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

pub fn valid_host_uuid(host_id: &str) -> bool {
  uuid::Uuid::parse_str(host_id).is_ok_and(|uuid| uuid.get_version_num() == 4 && uuid.to_string() == host_id)
}

/// Exact paths prevent the gateway becoming a general loopback HTTP proxy.
/// Some POSTs update viewer bookkeeping, but only submit_session_input executes
/// agent input and requires the connector's explicit control permission.
pub fn allowed_route(method: &str, path: &str, allow_control: bool) -> bool {
  match (method, path) {
    ("GET", "/api/v1/health" | "/api/v1/events") => true,
    ("POST", "/api/v1/submit_session_input") => allow_control,
    (
      "POST",
      "/api/v1/list_sessions"
      | "/api/v1/list_session_children"
      | "/api/v1/load_event_page"
      | "/api/v1/load_session_updates"
      | "/api/v1/update_session_view"
      | "/api/v1/load_event_detail"
      | "/api/v1/load_trajectory_event_page"
      | "/api/v1/acknowledge_session_attention"
      | "/api/v1/get_session_index_progress"
      | "/api/v1/retry_session_index"
      | "/api/v1/get_relay_status"
      | "/api/v1/get_session_input_status",
    ) => true,
    _ => false,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn paths_are_exact_and_control_is_explicit() {
    assert!(allowed_route("POST", "/api/v1/list_sessions", false));
    for allow_control in [false, true] {
      assert!(allowed_route("POST", "/api/v1/load_session_updates", allow_control));
      for method in ["GET", "PUT", "DELETE"] {
        assert!(!allowed_route(method, "/api/v1/load_session_updates", allow_control));
      }
      for path in ["/api/v1/load_session_updates/", "/api/v1/load_session_updates?x=1"] {
        assert!(!allowed_route("POST", path, allow_control));
      }
    }
    assert!(allowed_route("POST", "/api/v1/submit_session_input", true));
    assert!(!allowed_route("POST", "/api/v1/submit_session_input", false));
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
