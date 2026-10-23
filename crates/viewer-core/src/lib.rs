//! Shared viewer domain and runtime. Both Tauri and HTTP adapters consume
//! this crate; core owns snapshots and semantic delivery, while standalone
//! Relay supplies provider feeds.
pub mod background;
pub mod delivery;
pub mod images;
mod index_queries;
mod indexer;
mod input;
pub(crate) mod managed_pi;
pub mod model;
pub mod models;
pub mod projects;
mod questions;
pub mod relay;
mod repository;
pub mod rpc_profiles;
pub mod rpc_runtime;
pub mod runtime;
pub mod runtimes;
pub mod service;
pub mod service_client;
mod service_history;
mod service_metadata;
pub mod service_protocol;
pub mod service_server;
mod service_source;
pub mod settings;
pub mod updates;
mod watcher;
pub mod workspace;
pub use service::ViewerService;
use tokn_session_relay::{RelayConfig, RelayRecord};
