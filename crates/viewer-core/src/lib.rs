//! Shared viewer domain and runtime. Both Tauri and HTTP adapters consume
//! this crate; Relay provides live-feed hints while core owns snapshots.
mod index_queries;
mod indexer;
mod input;
pub mod model;
mod questions;
pub mod relay;
mod repository;
pub mod runtime;
pub mod service;
pub mod service_client;
mod service_history;
mod service_metadata;
pub mod service_protocol;
pub mod service_server;
mod service_source;
mod watcher;
pub use service::ViewerService;
use tokn_session_relay::{RelayConfig, RelayRecord};
