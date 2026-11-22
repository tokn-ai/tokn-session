use crate::{
  exchange::{self, Exchange},
  session::Session,
};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tokn_hub_client_core::secure::NoiseIdentity;
use url::Url;

/// A relay-only connection for the installed loopback viewer. Its lifetime
/// belongs to the selected machine and its pinned device identity.
pub struct PinnedRelay {
  endpoint: Url,
  identity: Arc<NoiseIdentity>,
  host_key: String,
  cancellation: CancellationToken,
  active: Mutex<Option<Arc<Session>>>,
}

impl PinnedRelay {
  pub fn new(endpoint: Url, identity: Arc<NoiseIdentity>, host_key: String, cancellation: CancellationToken) -> Self {
    Self {
      endpoint,
      identity,
      host_key,
      cancellation,
      active: Mutex::default(),
    }
  }

  pub async fn request(&self, method: &str, path: &str, body: &[u8]) -> Result<Exchange, String> {
    let active = session(
      &mut *self.active.lock().await,
      &self.endpoint,
      &self.identity,
      &self.host_key,
      &self.cancellation,
    )
    .await?;
    Exchange::open(&active, method, path, body).await
  }
}

pub(crate) async fn session(
  slot: &mut Option<Arc<Session>>,
  endpoint: &Url,
  identity: &NoiseIdentity,
  host_key: &str,
  cancellation: &CancellationToken,
) -> Result<Arc<Session>, String> {
  if cancellation.is_cancelled() {
    return Err("Machine disconnected".into());
  }
  if let Some(active) = slot.as_ref().filter(|active| !active.stopped().is_cancelled()) {
    return Ok(active.clone());
  }
  let active = tokio::select! {
    biased;
    _ = cancellation.cancelled() => return Err("Machine disconnected".into()),
    result = async {
      Session::authenticate(exchange::relay_transport(endpoint).await?, identity, host_key, cancellation.clone()).await
    } => result?,
  };
  *slot = Some(active.clone());
  Ok(active)
}
