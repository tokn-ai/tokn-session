//! Shared worker retirement ordering, independent of native provider protocols.
use super::{Command, DeliveryError, ManagedHostContext, SessionLease, WorkerState};
use tokio::sync::{OwnedMutexGuard, mpsc, oneshot, watch};

/// Keeps process ownership alive until cleanup and the final durable snapshot
/// complete. Drivers reject their in-flight native workflows before `complete`
/// acquires the admission gate, so callers holding that gate can always wake.
pub struct ManagedWorkerLifecycle {
  host: ManagedHostContext,
  lease: SessionLease,
  native_lease: Option<crate::session_ownership::SessionLease>,
  worker_state: watch::Sender<WorkerState>,
}

impl ManagedWorkerLifecycle {
  pub fn new(
    host: ManagedHostContext,
    lease: SessionLease,
    native_lease: Option<crate::session_ownership::SessionLease>,
    worker_state: watch::Sender<WorkerState>,
  ) -> Self {
    Self {
      host,
      lease,
      native_lease,
      worker_state,
    }
  }

  pub fn close_admission(&self, commands: &mut mpsc::Receiver<Command>, error: Option<String>, message: &str) {
    commands.close();
    self.worker_state.send_replace(WorkerState::Closing(error));
    while let Ok(command) = commands.try_recv() {
      match command {
        Command::Model(_, reply) => {
          let _ = reply.send(Err(DeliveryError::not_sent(message)));
        }
        Command::Prompt { reply, .. } => {
          let _ = reply.send(Err(DeliveryError::not_sent(message)));
        }
        Command::Abort(reply) | Command::Dialog(_, reply) => {
          let _ = reply.send(Err(message.to_owned()));
        }
      }
    }
  }

  pub async fn complete(
    self,
    error: Option<String>,
    cleanup_confirmed: bool,
    retirement: Option<OwnedMutexGuard<()>>,
    stop_reply: Option<oneshot::Sender<Result<(), String>>>,
  ) {
    let retirement = match retirement {
      Some(retirement) => Some(retirement),
      None => self.host.retirement_gate().await,
    };
    let finished = self.host.finish(error.clone()).await;
    if cleanup_confirmed {
      self.host.release_native_ownership();
    }
    // An observer may immediately attempt a resume after the stopped event.
    // Release the native and receipt leases before publishing that observation.
    drop(self.native_lease);
    drop(self.lease);
    self.worker_state.send_replace(WorkerState::Stopped(error));
    self.host.publish_finished(finished);
    drop(retirement);
    if let Some(reply) = stop_reply {
      let _ = reply.send(Ok(()));
    }
  }
}
