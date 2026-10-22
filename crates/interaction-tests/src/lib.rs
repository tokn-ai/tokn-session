//! Test-only infrastructure. Real agents talk to local deterministic models;
//! ordinary tests replay captured native RPC without installing an agent.
mod model;
mod proxy;
mod replay;
mod trace;

pub use model::{ModelGate, ModelServer, ModelStep};
pub use proxy::RecordingProxy;
pub use replay::ReplayServer;
pub use trace::{Fixture, Trace, WireRecord};

pub type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[cfg(test)]
mod tests;
