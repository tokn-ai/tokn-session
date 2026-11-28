//! Codex executable discovery and app-server interaction.

pub mod executable;

#[cfg(feature = "app-server")]
mod app_server;
#[cfg(feature = "app-server")]
pub use app_server::*;
