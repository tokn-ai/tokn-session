use super::{
  NoiseIdentity, SignedGrant,
  identity::{NOISE_PROTOCOL, decode_noise_public_key},
};
use crate::protocol::{decode, encode};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

pub const MAX_RECORD: usize = 65_535;
pub const MAX_PLAINTEXT: usize = MAX_RECORD - 16;
pub const MAX_CHUNK: usize = 32 * 1024;
pub const MAX_REQUEST_BODY: usize = crate::protocol::MAX_BODY;
pub const MAX_AUTH_PAYLOAD: usize = 32 * 1024;
pub const MAX_DIRECT_SDP: usize = 32 * 1024;
pub const MAX_ICE_SERVERS: usize = 8;
const PROLOGUE: &[u8] = b"tokn-hub-e2ee-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostAuthOperation {
  RegisterStart,
  RegisterFinish,
  LoginStart,
  LoginFinish,
}

/// One HTTP exchange per Noise channel. Requests are authenticated before body
/// assembly; aggregate body size, ordering, response credit, and route/scope
/// checks belong to the endpoint's exchange state machine. No application data
/// is accepted in the replayable first IK handshake message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InnerMessage {
  /// Host-owned passkey authentication, carried only inside a fresh Noise channel.
  AuthRequest {
    operation: HostAuthOperation,
    payload: serde_json::Value,
  },
  AuthResponse {
    payload: serde_json::Value,
  },
  /// Direct negotiation is authenticated and encrypted before any peer is created.
  DirectConfigRequest {},
  DirectConfig {
    ice_servers: Vec<String>,
  },
  DirectOffer {
    sdp: String,
  },
  DirectAnswer {
    sdp: String,
  },
  Request {
    method: String,
    path: String,
    grant: SignedGrant,
  },
  /// Full-host access authenticated against the host's paired-device registry.
  DeviceRequest {
    method: String,
    path: String,
  },
  RequestBody {
    data: String,
  },
  RequestEnd {},
  Response {
    status: u16,
    content_type: Option<String>,
  },
  Chunk {
    data: String,
  },
  End {},
  Error {
    message: String,
  },
  Window {
    credits: usize,
  },
}

impl InnerMessage {
  pub fn validate(&self) -> Result<(), String> {
    match self {
      Self::AuthRequest { payload, .. } | Self::AuthResponse { payload } => {
        if !payload.is_object()
          || serde_json::to_vec(payload)
            .map_err(|_| "Invalid host authentication payload")?
            .len()
            > MAX_AUTH_PAYLOAD
        {
          return Err("Host authentication requires a bounded object payload".into());
        }
      }
      Self::DirectConfig { ice_servers } => {
        validate_ice_servers(ice_servers)?;
      }
      Self::DirectOffer { sdp } | Self::DirectAnswer { sdp } => {
        if sdp.is_empty()
          || sdp.len() > MAX_DIRECT_SDP
          || !sdp.starts_with("v=0\r\n")
          || sdp
            .chars()
            .any(|value| value.is_control() && !matches!(value, '\r' | '\n' | '\t'))
        {
          return Err("Direct negotiation requires bounded SDP".into());
        }
      }
      Self::Request { method, path, .. } | Self::DeviceRequest { method, path } => {
        if !matches!(method.as_str(), "GET" | "POST") {
          return Err("Unsupported secure request method".into());
        }
        if path.len() > 512 || !path.starts_with("/api/v1/") || path.chars().any(char::is_control) {
          return Err("Invalid secure request path".into());
        }
        if let Self::Request { grant, .. } = self {
          grant.validate()?;
        }
      }
      Self::RequestBody { data } | Self::Chunk { data } => {
        let bytes = decode(data, MAX_CHUNK)?;
        if bytes.is_empty() || encode(&bytes) != *data {
          return Err("Encrypted chunks must be nonempty canonical base64".into());
        }
      }
      Self::Response { status, content_type } => {
        if !(100..=599).contains(status) {
          return Err("Invalid secure response status".into());
        }
        if content_type
          .as_ref()
          .is_some_and(|value| value.len() > 128 || value.chars().any(char::is_control))
        {
          return Err("Invalid secure response content type".into());
        }
      }
      Self::Error { message } => {
        if message.is_empty() || message.len() > 1024 || message.chars().any(char::is_control) {
          return Err("Invalid secure error message".into());
        }
      }
      Self::Window { credits } => {
        if !(1..=crate::protocol::RESPONSE_WINDOW).contains(credits) {
          return Err("Invalid encrypted response window".into());
        }
      }
      Self::DirectConfigRequest {} | Self::RequestEnd {} | Self::End {} => {}
    }
    Ok(())
  }
}

/// Only explicitly configured STUN discovery servers are supported. TURN would
/// add another relay and credentials; encrypted Hub fallback already exists.
pub fn validate_ice_servers(servers: &[String]) -> Result<(), String> {
  if servers.len() > MAX_ICE_SERVERS {
    return Err("Configure at most eight STUN URLs without credentials".into());
  }
  for server in servers {
    let authority = server
      .strip_prefix("stun:")
      .ok_or("Direct discovery requires a STUN URL")?;
    if server.len() > 512
      || authority.is_empty()
      || authority.contains(['@', '/', '\\', '?', '#'])
      || authority.ends_with(':')
      || authority
        .chars()
        .any(|value| value.is_whitespace() || value.is_control())
    {
      return Err("STUN URLs must contain a host and optional port without credentials or paths".into());
    }
    // STUN uses an opaque URI, so validate its authority with the existing
    // portable URL parser rather than accepting malformed host/port strings.
    let parsed = url::Url::parse(&format!("http://{authority}")).map_err(|_| "Invalid STUN host or port")?;
    if parsed.host_str().is_none() || parsed.port() == Some(0) || parsed.path() != "/" {
      return Err("Invalid STUN host or port".into());
    }
  }
  Ok(())
}

/// An IK initiator must already know the authentic host public key. Obtain it
/// through host-verified pairing or a legacy owner-signed invitation checked
/// with an independent owner key.
pub struct NoiseInitiator {
  state: snow::HandshakeState,
  pinned_host: String,
}

impl NoiseInitiator {
  pub fn new(identity: &NoiseIdentity, pinned_host: &str) -> Result<Self, String> {
    let host = decode_noise_public_key(pinned_host)?;
    let state = builder()?
      .local_private_key(identity.private_key())
      .map_err(noise_error)?
      .remote_public_key(&host)
      .map_err(noise_error)?
      .build_initiator()
      .map_err(noise_error)?;
    Ok(Self {
      state,
      pinned_host: pinned_host.into(),
    })
  }

  pub fn start(&mut self) -> Result<Vec<u8>, String> {
    let mut record = vec![0; MAX_RECORD];
    let length = self.state.write_message(&[], &mut record).map_err(noise_error)?;
    record.truncate(length);
    Ok(record)
  }

  pub fn finish(mut self, reply: &[u8]) -> Result<SecureChannel, String> {
    read_empty_handshake(&mut self.state, reply)?;
    if !self.state.is_handshake_finished() {
      return Err("Incomplete Noise handshake".into());
    }
    let remote = remote_key(&self.state)?;
    if remote != self.pinned_host {
      return Err("Noise host identity did not match its pin".into());
    }
    SecureChannel::from_handshake(self.state, remote)
  }
}

/// Completing the responder handshake authenticates the client's static key;
/// it does not authorize access. Check the host's device registry or a legacy
/// grant bound to that key next.
pub struct NoiseResponder(snow::HandshakeState);

impl NoiseResponder {
  pub fn new(identity: &NoiseIdentity) -> Result<Self, String> {
    Ok(Self(
      builder()?
        .local_private_key(identity.private_key())
        .map_err(noise_error)?
        .build_responder()
        .map_err(noise_error)?,
    ))
  }

  pub fn accept(mut self, first: &[u8]) -> Result<(Vec<u8>, SecureChannel), String> {
    read_empty_handshake(&mut self.0, first)?;
    let remote = remote_key(&self.0)?;
    let mut reply = vec![0; MAX_RECORD];
    let length = self.0.write_message(&[], &mut reply).map_err(noise_error)?;
    reply.truncate(length);
    if !self.0.is_handshake_finished() {
      return Err("Incomplete Noise handshake".into());
    }
    Ok((reply, SecureChannel::from_handshake(self.0, remote)?))
  }
}

/// Ordered, authenticated Noise transport. Keep one channel per connection;
/// never clone/reuse nonce state or retry ciphertext on a new connection. A
/// malformed, replayed, reordered, or forged record permanently closes it.
pub struct SecureChannel {
  state: snow::TransportState,
  remote_public_key: String,
  channel_binding: String,
  failed: bool,
}

impl SecureChannel {
  fn from_handshake(state: snow::HandshakeState, remote_public_key: String) -> Result<Self, String> {
    let channel_binding = encode(state.get_handshake_hash());
    Ok(Self {
      state: state.into_transport_mode().map_err(noise_error)?,
      remote_public_key,
      channel_binding,
      failed: false,
    })
  }

  pub fn remote_public_key(&self) -> &str {
    &self.remote_public_key
  }

  /// Bind endpoint authentication ceremonies to this completed Noise handshake.
  pub fn channel_binding(&self) -> &str {
    &self.channel_binding
  }

  pub fn encrypt(&mut self, message: &InnerMessage) -> Result<Vec<u8>, String> {
    if self.failed {
      return Err("Secure channel is closed".into());
    }
    message.validate()?;
    let plaintext = Zeroizing::new(serde_json::to_vec(message).map_err(|e| e.to_string())?);
    self.encrypt_bytes(&plaintext)
  }

  /// Authenticate one bounded byte record using this channel's next send nonce.
  pub fn encrypt_bytes(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, String> {
    if self.failed {
      return Err("Secure channel is closed".into());
    }
    if plaintext.len() > MAX_PLAINTEXT {
      return Err("Secure message exceeds the Noise record limit".into());
    }
    let mut encrypted = vec![0; plaintext.len() + 16];
    match self.state.write_message(&plaintext, &mut encrypted) {
      Ok(length) => {
        encrypted.truncate(length);
        Ok(encrypted)
      }
      Err(error) => {
        self.failed = true;
        Err(noise_error(error))
      }
    }
  }

  pub fn decrypt(&mut self, encrypted: &[u8]) -> Result<InnerMessage, String> {
    if self.failed {
      return Err("Secure channel is closed".into());
    }
    let result = self.decrypt_bytes(encrypted).and_then(|plaintext| {
      let plaintext = Zeroizing::new(plaintext);
      let message: InnerMessage =
        serde_json::from_slice(&plaintext).map_err(|_| "Invalid encrypted protocol message")?;
      message.validate()?;
      Ok(message)
    });
    if result.is_err() {
      self.failed = true;
    }
    result
  }

  /// Authenticate one bounded byte record. Any malformed record closes the channel.
  pub fn decrypt_bytes(&mut self, encrypted: &[u8]) -> Result<Vec<u8>, String> {
    if self.failed {
      return Err("Secure channel is closed".into());
    }
    let result = self.decrypt_record(encrypted);
    if result.is_err() {
      self.failed = true;
    }
    result
  }

  fn decrypt_record(&mut self, encrypted: &[u8]) -> Result<Vec<u8>, String> {
    if !(16..=MAX_RECORD).contains(&encrypted.len()) {
      return Err("Invalid Noise record length".into());
    }
    let mut plaintext = vec![0; encrypted.len()];
    let length = self
      .state
      .read_message(encrypted, &mut plaintext)
      .map_err(noise_error)?;
    plaintext.truncate(length);
    Ok(plaintext)
  }
}

fn builder<'a>() -> Result<snow::Builder<'a>, String> {
  snow::Builder::new(NOISE_PROTOCOL.parse().map_err(noise_error)?)
    .prologue(PROLOGUE)
    .map_err(noise_error)
}

fn read_empty_handshake(state: &mut snow::HandshakeState, message: &[u8]) -> Result<(), String> {
  if message.is_empty() || message.len() > MAX_RECORD {
    return Err("Invalid Noise handshake length".into());
  }
  let mut payload = vec![0; MAX_RECORD];
  if state.read_message(message, &mut payload).map_err(noise_error)? != 0 {
    return Err("Noise handshake must not contain application data".into());
  }
  Ok(())
}

fn remote_key(state: &snow::HandshakeState) -> Result<String, String> {
  let key = encode(state.get_remote_static().ok_or("Noise peer identity is missing")?);
  decode_noise_public_key(&key)?;
  Ok(key)
}

fn noise_error(error: snow::Error) -> String {
  format!("Noise authentication failed: {error}")
}

#[cfg(test)]
mod tests {
  use super::*;

  fn pair() -> (SecureChannel, SecureChannel) {
    let client = NoiseIdentity::generate().unwrap();
    let host = NoiseIdentity::generate().unwrap();
    let mut initiator = NoiseInitiator::new(&client, &host.public_key()).unwrap();
    let first = initiator.start().unwrap();
    let (reply, responder) = NoiseResponder::new(&host).unwrap().accept(&first).unwrap();
    let initiator = initiator.finish(&reply).unwrap();
    assert_eq!(initiator.remote_public_key(), host.public_key());
    assert_eq!(responder.remote_public_key(), client.public_key());
    (initiator, responder)
  }

  #[test]
  fn noise_authenticates_both_peers_and_bidirectional_records() {
    let (mut client, mut host) = pair();
    let request = client.encrypt(&InnerMessage::RequestEnd {}).unwrap();
    assert_eq!(host.decrypt(&request).unwrap(), InnerMessage::RequestEnd {});
    let response = host
      .encrypt(&InnerMessage::Chunk {
        data: encode(b"private session contents"),
      })
      .unwrap();
    assert!(!response.windows(7).any(|bytes| bytes == b"private"));
    assert_eq!(
      client.decrypt(&response).unwrap(),
      InnerMessage::Chunk {
        data: encode(b"private session contents")
      }
    );
  }

  #[test]
  fn handshake_bindings_are_shared_by_peers_and_unique_per_channel() {
    let client = NoiseIdentity::generate().unwrap();
    let host = NoiseIdentity::generate().unwrap();
    let handshake = || {
      let mut initiator = NoiseInitiator::new(&client, &host.public_key()).unwrap();
      let (reply, responder) = NoiseResponder::new(&host)
        .unwrap()
        .accept(&initiator.start().unwrap())
        .unwrap();
      (initiator.finish(&reply).unwrap(), responder)
    };
    let (first_client, first_host) = handshake();
    let (second_client, second_host) = handshake();
    assert_eq!(first_client.channel_binding(), first_host.channel_binding());
    assert_eq!(second_client.channel_binding(), second_host.channel_binding());
    assert_ne!(first_client.channel_binding(), second_client.channel_binding());
    assert_eq!(decode(first_client.channel_binding(), 32).unwrap().len(), 32);
  }

  #[test]
  fn byte_records_preserve_bounds_order_and_fail_closed_protocol_decoding() {
    let (mut client, mut host) = pair();
    let plaintext = vec![42; MAX_PLAINTEXT];
    let record = client.encrypt_bytes(&plaintext).unwrap();
    assert_eq!(record.len(), MAX_RECORD);
    assert_eq!(host.decrypt_bytes(&record).unwrap(), plaintext);
    assert!(client.encrypt_bytes(&vec![42; MAX_PLAINTEXT + 1]).is_err());
    let malformed = client
      .encrypt_bytes(br#"{"type":"end","unsigned_permission":true}"#)
      .unwrap();
    assert!(host.decrypt(&malformed).is_err());
    let next = client.encrypt(&InnerMessage::End {}).unwrap();
    assert!(host.decrypt_bytes(&next).is_err());
    assert!(host.encrypt_bytes(b"closed").is_err());
  }

  #[test]
  fn host_auth_messages_are_typed_bounded_and_authenticated() {
    let (mut client, mut host) = pair();
    for operation in [
      HostAuthOperation::RegisterStart,
      HostAuthOperation::RegisterFinish,
      HostAuthOperation::LoginStart,
      HostAuthOperation::LoginFinish,
    ] {
      let message = InnerMessage::AuthRequest {
        operation,
        payload: serde_json::json!({ "credential": {} }),
      };
      assert_eq!(host.decrypt(&client.encrypt(&message).unwrap()).unwrap(), message);
    }
    let response = InnerMessage::AuthResponse {
      payload: serde_json::json!({ "authorized": true, "registered": false }),
    };
    assert_eq!(client.decrypt(&host.encrypt(&response).unwrap()).unwrap(), response);
    assert!(
      client
        .encrypt(&InnerMessage::AuthRequest {
          operation: HostAuthOperation::LoginStart,
          payload: serde_json::json!([]),
        })
        .is_err()
    );
    assert!(
      host
        .encrypt(&InnerMessage::AuthResponse {
          payload: serde_json::json!({ "oversized": "x".repeat(MAX_AUTH_PAYLOAD) }),
        })
        .is_err()
    );
    assert!(
      serde_json::from_str::<InnerMessage>(r#"{"type":"auth_request","operation":"approve_anything","payload":{}}"#)
        .is_err()
    );
  }

  #[test]
  fn direct_signaling_is_encrypted_bounded_and_rejects_relay_credentials() {
    let (mut client, mut host) = pair();
    let offer = InnerMessage::DirectOffer {
      sdp: "v=0\r\na=ice-ufrag:private\r\n".into(),
    };
    let record = client.encrypt(&offer).unwrap();
    assert!(!record.windows(7).any(|bytes| bytes == b"private"));
    assert_eq!(host.decrypt(&record).unwrap(), offer);
    let config = InnerMessage::DirectConfig {
      ice_servers: vec!["stun:stun.example.test:3478".into()],
    };
    assert_eq!(client.decrypt(&host.encrypt(&config).unwrap()).unwrap(), config);
    for sdp in [
      "".into(),
      "v=0\r\n\0".into(),
      format!("v=0\r\n{}", "x".repeat(MAX_DIRECT_SDP)),
    ] {
      assert!(InnerMessage::DirectAnswer { sdp }.validate().is_err());
    }
    for servers in [
      vec!["turn:relay.example.test:3478".into()],
      vec!["stuns:stun.example.test:5349".into()],
      vec![format!("stun:{}", "x".repeat(512))],
      vec!["stun:user@stun.example.test".into()],
      vec!["stun:example.test\n".into()],
      vec!["stun://example.test".into()],
      vec!["stun:example.test:0".into()],
      vec!["stun:example.test:65536".into()],
      vec!["stun:example.test/path".into()],
      vec!["stun:example.test".into(); MAX_ICE_SERVERS + 1],
    ] {
      assert!(InnerMessage::DirectConfig { ice_servers: servers }.validate().is_err());
    }
    assert!(validate_ice_servers(&["stun:[::1]:3478".into()]).is_ok());
    assert!(
      serde_json::from_str::<InnerMessage>(r#"{"type":"direct_offer","sdp":"v=0\r\n","grant_device":true}"#).is_err()
    );
  }

  #[test]
  fn a_wrong_pinned_host_cannot_complete_the_handshake() {
    let client = NoiseIdentity::generate().unwrap();
    let real_host = NoiseIdentity::generate().unwrap();
    let impostor = NoiseIdentity::generate().unwrap();
    let mut initiator = NoiseInitiator::new(&client, &real_host.public_key()).unwrap();
    assert!(
      NoiseResponder::new(&impostor)
        .unwrap()
        .accept(&initiator.start().unwrap())
        .is_err()
    );
  }

  #[test]
  fn tamper_replay_and_reordering_permanently_close_channels() {
    let (mut client, mut host) = pair();
    let first = client.encrypt(&InnerMessage::RequestEnd {}).unwrap();
    host.decrypt(&first).unwrap();
    assert!(host.decrypt(&first).is_err());
    let second = client.encrypt(&InnerMessage::End {}).unwrap();
    assert!(host.decrypt(&second).is_err());

    let (mut client, mut host) = pair();
    let first = client.encrypt(&InnerMessage::RequestEnd {}).unwrap();
    let second = client.encrypt(&InnerMessage::End {}).unwrap();
    assert!(host.decrypt(&second).is_err());
    assert!(host.decrypt(&first).is_err());

    let (mut client, mut host) = pair();
    let mut tampered = client.encrypt(&InnerMessage::End {}).unwrap();
    tampered[0] ^= 1;
    assert!(host.decrypt(&tampered).is_err());
    assert!(host.encrypt(&InnerMessage::End {}).is_err());
  }

  #[test]
  fn replayed_first_handshake_cannot_replay_an_application_request() {
    let client = NoiseIdentity::generate().unwrap();
    let host = NoiseIdentity::generate().unwrap();
    let mut initiator = NoiseInitiator::new(&client, &host.public_key()).unwrap();
    let first = initiator.start().unwrap();
    let (reply, mut receiver) = NoiseResponder::new(&host).unwrap().accept(&first).unwrap();
    let mut sender = initiator.finish(&reply).unwrap();
    let request = sender.encrypt(&InnerMessage::RequestEnd {}).unwrap();
    receiver.decrypt(&request).unwrap();
    let (_, mut fresh_receiver) = NoiseResponder::new(&host).unwrap().accept(&first).unwrap();
    assert!(fresh_receiver.decrypt(&request).is_err());
  }

  #[test]
  fn replayable_handshake_payloads_are_rejected_before_authorization() {
    let client = NoiseIdentity::generate().unwrap();
    let host = NoiseIdentity::generate().unwrap();
    let host_public = decode_noise_public_key(&host.public_key()).unwrap();
    let mut initiator = builder()
      .unwrap()
      .local_private_key(client.private_key())
      .unwrap()
      .remote_public_key(&host_public)
      .unwrap()
      .build_initiator()
      .unwrap();
    let mut first = vec![0; MAX_RECORD];
    let length = initiator.write_message(b"application request", &mut first).unwrap();
    assert!(NoiseResponder::new(&host).unwrap().accept(&first[..length]).is_err());
  }

  #[test]
  fn record_limits_and_unknown_inner_fields_fail_closed() {
    let (mut client, mut host) = pair();
    let max_chunk = InnerMessage::Chunk {
      data: encode(&vec![42; MAX_CHUNK]),
    };
    let encrypted = client.encrypt(&max_chunk).unwrap();
    assert!(encrypted.len() <= MAX_RECORD);
    assert_eq!(host.decrypt(&encrypted).unwrap(), max_chunk);
    assert!(
      client
        .encrypt(&InnerMessage::Chunk {
          data: encode(&vec![42; MAX_CHUNK + 1])
        })
        .is_err()
    );
    assert!(host.decrypt(&vec![0; MAX_RECORD + 1]).is_err());
    assert!(serde_json::from_str::<InnerMessage>(r#"{"type":"end","extra":true}"#).is_err());
  }
}
