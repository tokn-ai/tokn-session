//! Portable Hub client cryptography shared by native hosts, apps, and browsers.
//!
//! Transports and endpoint authorization remain outside this crate. A Noise
//! handshake authenticates keys but never authorizes session access by itself.
pub mod address;
pub mod pairing;
pub mod protocol;
pub mod secure;

#[cfg(target_arch = "wasm32")]
mod wasm;
