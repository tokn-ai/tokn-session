//! Host-local authenticator pairing through an untrusted Hub.
//!
//! This is an application protocol using RustCrypto's SPAKE2 implementation,
//! not an implementation of RFC 9382's wire format. The dependency and this
//! composition have not received an independent cryptographic audit. Never
//! replace this exchange with a cleartext OTP or a low-entropy Noise PSK.
//!
//! Each attempt uses one declared TOTP step and fresh asymmetric SPAKE2 state.
//! Role-separated HKDF/HMAC confirmations bind both Noise identities, the exact
//! target host, the time step, and both complete initial records. The caller
//! must enforce persistent host-local attempt limits before `respond`, commit
//! authorization and OTP consumption before sending the final acknowledgment,
//! and save a client-side pin only after verifying that acknowledgment.
use crate::{
  protocol::{decode, encode},
  secure::{NoiseIdentity, decode_public_key},
};
use data_encoding::BASE32_NOPAD;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use zeroize::Zeroizing;

const MAGIC: &[u8] = b"tokn-hub-pairing-v1\0";
const DOMAIN: &[u8] = b"tokn-hub-authenticator-pairing-v1";
pub const MAX_PAIRING_RECORD: usize = 4096;
pub const TOTP_PERIOD: u64 = 30;
const AUTH_FAILED: &str = "Authenticator pairing failed; wait for a new code and check device clocks";

/// A locally held RFC 6238 seed. Secrets have no Debug/Serialize implementation.
pub struct TotpSecret(Zeroizing<Vec<u8>>);

impl TotpSecret {
  pub fn generate() -> Self {
    let mut bytes = Zeroizing::new(vec![0; 20]);
    OsRng.fill_bytes(&mut bytes);
    Self(bytes)
  }

  /// Import an authenticator's Base32 seed, allowing spaces and lowercase.
  pub fn from_base32(value: &str) -> Result<Self, String> {
    if value.len() > 256 {
      return Err("Authenticator secret is too long".into());
    }
    let normalized = Zeroizing::new(
      value
        .chars()
        .filter(|value| !value.is_ascii_whitespace())
        .map(|value| value.to_ascii_uppercase())
        .collect::<String>(),
    );
    let raw = normalized.trim_end_matches('=');
    let bytes = Zeroizing::new(
      BASE32_NOPAD
        .decode(raw.as_bytes())
        .map_err(|_| "Authenticator secret must be Base32")?,
    );
    if !(16..=64).contains(&bytes.len()) {
      return Err("Authenticator secret must contain 16 to 64 bytes".into());
    }
    Ok(Self(bytes))
  }

  /// Only expose for local setup, user-managed synchronization, or private storage.
  pub fn to_base32(&self) -> String {
    BASE32_NOPAD.encode(&self.0)
  }

  /// This URI contains the secret. Never send it through the Hub.
  pub fn provisioning_uri(&self, label: &str) -> Result<String, String> {
    if label.is_empty() || label.len() > 128 || label.chars().any(char::is_control) {
      return Err("Authenticator label must contain 1 to 128 non-control characters".into());
    }
    let mut uri = url::Url::parse("otpauth://totp/").map_err(|e| e.to_string())?;
    uri.set_path(&format!("Tokn Hub:{label}"));
    uri
      .query_pairs_mut()
      .append_pair("secret", &self.to_base32())
      .append_pair("issuer", "Tokn Hub")
      .append_pair("algorithm", "SHA1")
      .append_pair("digits", "6")
      .append_pair("period", "30");
    Ok(uri.into())
  }

  pub fn code_at(&self, unix_seconds: u64) -> String {
    self.code_for_step(unix_seconds / TOTP_PERIOD)
  }

  fn code_for_step(&self, step: u64) -> String {
    format!("{:06}", self.hotp(step) % 1_000_000)
  }

  fn hotp(&self, step: u64) -> u32 {
    let mut hmac = Hmac::<Sha1>::new_from_slice(&self.0).expect("HMAC accepts any key length");
    hmac.update(&step.to_be_bytes());
    let output = hmac.finalize().into_bytes();
    let offset = (output[19] & 0x0f) as usize;
    u32::from_be_bytes(
      output[offset..offset + 4]
        .try_into()
        .expect("SHA1 truncation is in bounds"),
    ) & 0x7fff_ffff
  }
}

/// Public identities authenticated by the completed exchange, never by the Hub.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AuthenticatedPairing {
  pub host_id: String,
  pub host_public_key: String,
  pub client_public_key: String,
  pub step: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientHello {
  host_id: String,
  client_public_key: String,
  step: u64,
  pake: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostHello {
  host_public_key: String,
  pake: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostReply {
  hello: HostHello,
  confirmation: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirmation {
  confirmation: String,
}

pub struct ClientPairing {
  state: Spake2<Ed25519Group>,
  hello: ClientHello,
  hello_wire: Vec<u8>,
}

pub struct ClientAwaitingAck {
  keys: ConfirmationKeys,
  identities: AuthenticatedPairing,
}

pub struct HostPairing {
  keys: ConfirmationKeys,
  identities: AuthenticatedPairing,
}

/// Classification only. Untrusted records must still pass bounded decoding.
pub fn is_pairing_record(bytes: &[u8]) -> bool {
  bytes.starts_with(MAGIC)
}

/// Read the declared time step before reserving an attempt in persistent state.
pub fn peek_step(bytes: &[u8]) -> Result<u64, String> {
  let hello: ClientHello = read_record(bytes)?;
  validate_hello(&hello)?;
  Ok(hello.step)
}

impl ClientPairing {
  pub fn start(host_id: &str, identity: &NoiseIdentity, code: &str, now: u64) -> Result<(Self, Vec<u8>), String> {
    validate_host_id(host_id)?;
    if code.len() != 6 || !code.bytes().all(|value| value.is_ascii_digit()) {
      return Err("Enter the six digits shown by your authenticator".into());
    }
    let mut hello = ClientHello {
      host_id: host_id.into(),
      client_public_key: identity.public_key(),
      step: now / TOTP_PERIOD,
      pake: String::new(),
    };
    let (client_id, host_id) = identities(&hello);
    let (state, message) = Spake2::<Ed25519Group>::start_a(&Password::new(code.as_bytes()), &client_id, &host_id);
    hello.pake = encode(&message);
    let hello_wire = write_record(&hello)?;
    Ok((
      Self {
        state,
        hello,
        hello_wire: hello_wire.clone(),
      },
      hello_wire,
    ))
  }

  pub fn confirm(self, reply: &[u8]) -> Result<(ClientAwaitingAck, Vec<u8>), String> {
    let reply: HostReply = read_record(reply)?;
    decode_public_key(&reply.hello.host_public_key)?;
    let shared = Zeroizing::new(
      self
        .state
        .finish(&decode_pake(&reply.hello.pake)?)
        .map_err(|_| AUTH_FAILED)?,
    );
    let keys = ConfirmationKeys::derive(&shared, &self.hello_wire, &write_record(&reply.hello)?)?;
    keys.verify(b"host-proof", &reply.confirmation)?;
    let confirmation = write_record(&Confirmation {
      confirmation: keys.sign(b"client-proof"),
    })?;
    let identities = AuthenticatedPairing {
      host_id: self.hello.host_id,
      host_public_key: reply.hello.host_public_key,
      client_public_key: self.hello.client_public_key,
      step: self.hello.step,
    };
    Ok((ClientAwaitingAck { keys, identities }, confirmation))
  }
}

impl ClientAwaitingAck {
  /// Only this successful result may become a persisted host pin.
  pub fn finish(self, ack: &[u8]) -> Result<AuthenticatedPairing, String> {
    let ack: Confirmation = read_record(ack)?;
    self.keys.verify(b"host-accepted", &ack.confirmation)?;
    Ok(self.identities)
  }
}

impl HostPairing {
  /// The caller must reserve a rate-limited attempt before invoking this method.
  pub fn respond(
    secret: &TotpSecret,
    host_id: &str,
    identity: &NoiseIdentity,
    hello_wire: &[u8],
    now: u64,
  ) -> Result<(Self, Vec<u8>), String> {
    let hello: ClientHello = read_record(hello_wire)?;
    validate_hello(&hello)?;
    if hello.host_id != host_id {
      return Err("Pairing target does not match this host".into());
    }
    validate_step(hello.step, now)?;
    let code = Zeroizing::new(secret.code_for_step(hello.step));
    let (client_id, host_id) = identities(&hello);
    let (state, message) = Spake2::<Ed25519Group>::start_b(&Password::new(code.as_bytes()), &client_id, &host_id);
    let shared = Zeroizing::new(state.finish(&decode_pake(&hello.pake)?).map_err(|_| AUTH_FAILED)?);
    let reply_hello = HostHello {
      host_public_key: identity.public_key(),
      pake: encode(&message),
    };
    let keys = ConfirmationKeys::derive(&shared, hello_wire, &write_record(&reply_hello)?)?;
    let confirmation = keys.sign(b"host-proof");
    let identities = AuthenticatedPairing {
      host_id: hello.host_id,
      host_public_key: reply_hello.host_public_key.clone(),
      client_public_key: hello.client_public_key,
      step: hello.step,
    };
    let reply = write_record(&HostReply {
      hello: reply_hello,
      confirmation,
    })?;
    Ok((Self { keys, identities }, reply))
  }

  /// Commit the returned device and consumed step before releasing the ack.
  pub fn finish(self, confirmation: &[u8], now: u64) -> Result<(AuthenticatedPairing, Vec<u8>), String> {
    validate_step(self.identities.step, now)?;
    let confirmation: Confirmation = read_record(confirmation)?;
    self.keys.verify(b"client-proof", &confirmation.confirmation)?;
    let ack = write_record(&Confirmation {
      confirmation: self.keys.sign(b"host-accepted"),
    })?;
    Ok((self.identities, ack))
  }
}

struct ConfirmationKeys {
  secret: Zeroizing<[u8; 32]>,
  transcript: [u8; 32],
}

impl ConfirmationKeys {
  fn derive(shared: &[u8], client: &[u8], host: &[u8]) -> Result<Self, String> {
    let mut hash = Sha256::new();
    hash.update(DOMAIN);
    hash.update((client.len() as u64).to_be_bytes());
    hash.update(client);
    hash.update((host.len() as u64).to_be_bytes());
    hash.update(host);
    let transcript: [u8; 32] = hash.finalize().into();
    let mut secret = Zeroizing::new([0; 32]);
    Hkdf::<Sha256>::new(Some(&transcript), shared)
      .expand(DOMAIN, secret.as_mut())
      .map_err(|_| "Could not derive pairing confirmation keys")?;
    Ok(Self { secret, transcript })
  }

  fn mac(&self, purpose: &[u8]) -> Hmac<Sha256> {
    let mut key = Zeroizing::new([0; 32]);
    Hkdf::<Sha256>::new(Some(DOMAIN), self.secret.as_ref())
      .expand(purpose, key.as_mut())
      .expect("32-byte HKDF output is within bounds");
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_ref()).expect("HMAC accepts any key length");
    mac.update(DOMAIN);
    mac.update(purpose);
    mac.update(&self.transcript);
    mac
  }

  fn sign(&self, purpose: &[u8]) -> String {
    encode(&self.mac(purpose).finalize().into_bytes())
  }

  fn verify(&self, purpose: &[u8], received: &str) -> Result<(), String> {
    let received = decode(received, 32).map_err(|_| AUTH_FAILED)?;
    self
      .mac(purpose)
      .verify_slice(&received)
      .map_err(|_| AUTH_FAILED.into())
  }
}

fn identities(hello: &ClientHello) -> (Identity, Identity) {
  let client = format!("tokn-hub-pairing-v1/client/{}", hello.client_public_key);
  let host = format!("tokn-hub-pairing-v1/host/{}/{}", hello.host_id, hello.step);
  (Identity::new(client.as_bytes()), Identity::new(host.as_bytes()))
}

fn validate_hello(hello: &ClientHello) -> Result<(), String> {
  validate_host_id(&hello.host_id)?;
  decode_public_key(&hello.client_public_key)?;
  decode_pake(&hello.pake)?;
  Ok(())
}

fn validate_host_id(host_id: &str) -> Result<(), String> {
  if host_id.is_empty()
    || host_id.len() > 128
    || !host_id
      .bytes()
      .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
  {
    return Err("Invalid pairing host identity".into());
  }
  Ok(())
}

fn validate_step(step: u64, now: u64) -> Result<(), String> {
  if step.abs_diff(now / TOTP_PERIOD) > 1 {
    return Err("Authenticator pairing expired; wait for a new code and check device clocks".into());
  }
  Ok(())
}

fn decode_pake(value: &str) -> Result<Vec<u8>, String> {
  let bytes = decode(value, 33)?;
  if bytes.len() != 33 || encode(&bytes) != value {
    return Err("Invalid pairing message".into());
  }
  Ok(bytes)
}

fn write_record(value: &impl Serialize) -> Result<Vec<u8>, String> {
  let mut bytes = MAGIC.to_vec();
  bytes.extend(serde_json::to_vec(value).map_err(|e| format!("Could not encode pairing record: {e}"))?);
  if bytes.len() > MAX_PAIRING_RECORD {
    return Err("Pairing record is too large".into());
  }
  Ok(bytes)
}

fn read_record<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
  if bytes.len() > MAX_PAIRING_RECORD || !is_pairing_record(bytes) {
    return Err("Invalid pairing record".into());
  }
  serde_json::from_slice(&bytes[MAGIC.len()..]).map_err(|_| "Invalid pairing record".into())
}

#[cfg(test)]
mod tests;
