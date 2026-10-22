//! Codex app-server wire contracts, separate from persisted rollout protocols.
//!
//! Native app-server omits the JSON-RPC `jsonrpc` field. Unknown envelope fields,
//! methods, and notification payloads remain available for inspection.
mod rpc;
mod types;

pub use rpc::*;
pub use types::*;
