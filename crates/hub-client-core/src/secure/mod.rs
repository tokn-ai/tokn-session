//! Endpoint-owned identities and authenticated Noise records.
//!
//! Host-verified authenticator pairing establishes the default device trust.
//! Legacy signed grants require independently provisioned owner and host keys.
//! The Hub must never choose a trust anchor. An authenticated Noise peer is not
//! yet authorized: hosts must check their paired-device registry or verify a
//! legacy grant and enforce its scope before accessing the local viewer API.
mod grant;
mod identity;
mod noise;

pub use grant::{GRANT_VERSION, Grant, GrantScope, MAX_GRANT_BYTES, SignedGrant};
pub use identity::{NoiseIdentity, OwnerIdentity, decode_public_key};
pub use noise::{
  HostAuthOperation, InnerMessage, MAX_AUTH_PAYLOAD, MAX_CHUNK, MAX_PLAINTEXT, MAX_RECORD, MAX_REQUEST_BODY,
  NoiseInitiator, NoiseResponder, SecureChannel,
};
