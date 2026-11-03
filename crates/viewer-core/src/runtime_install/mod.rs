//! Explicit, selected-host agent software installation. Ordinary runtime
//! discovery only reads this inventory; it never fetches or installs software.
mod download;
mod filesystem;
mod package;
#[cfg(test)]
mod tests;

use filesystem::{InstallRecord, exclusive_lock, provider_name, read_record, recover, replace};
pub use filesystem::{RuntimeLaunchSpec, RuntimeUseGuard, acquire_use, launch_spec};
use serde::{Deserialize, Serialize};
use std::{
  collections::BTreeMap,
  path::{Path, PathBuf},
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  },
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tokn_interaction_core::Provider;

const PROVIDERS: [Provider; 4] = [Provider::Pi, Provider::Codex, Provider::OpenCode, Provider::Dsh];
const MAX_INSTALL_REQUESTS: usize = 1024;

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeInstallInventory {
  pub agents: Vec<AgentInstallation>,
}
#[derive(Clone, Debug, Serialize)]
pub struct AgentInstallation {
  pub provider: Provider,
  pub version: Option<String>,
  pub generation: Option<String>,
  pub requires_node: bool,
  pub available: bool,
  pub unavailable_reason: Option<String>,
  pub in_use: bool,
  pub job: Option<RuntimeInstallJob>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeInstallRequest {
  pub request_id: String,
  pub provider: Provider,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeInstallCancelRequest {
  pub job_id: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeInstallRemoveRequest {
  pub provider: Provider,
  pub generation: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct RuntimeInstallJob {
  pub job_id: String,
  pub request_id: String,
  pub provider: Provider,
  pub state: String,
  pub downloaded_bytes: u64,
  pub total_bytes: Option<u64>,
  pub error: Option<String>,
}
impl RuntimeInstallJob {
  fn terminal(&self) -> bool {
    matches!(self.state.as_str(), "completed" | "failed" | "cancelled")
  }
}
struct JobEntry {
  job: RuntimeInstallJob,
  cancel: CancellationToken,
}
struct Inner {
  root: PathBuf,
  _temporary: Option<tempfile::TempDir>,
  jobs: Mutex<BTreeMap<String, JobEntry>>,
  tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
  changed: broadcast::Sender<RuntimeInstallJob>,
  closed: AtomicBool,
  latest: Mutex<BTreeMap<String, String>>,
}
#[derive(Clone)]
pub struct RuntimeInstallManager(Arc<Inner>);
impl RuntimeInstallManager {
  pub fn new(root: PathBuf) -> Self {
    Self::construct(root, None)
  }
  pub fn temporary() -> Self {
    let directory = tempfile::tempdir().expect("Temporary runtime installation directory");
    Self::construct(directory.path().join("runtimes"), Some(directory))
  }
  fn construct(root: PathBuf, temporary: Option<tempfile::TempDir>) -> Self {
    if root.exists() {
      for provider in PROVIDERS {
        if let Ok(_lease) = exclusive_lock(&root, provider) {
          let _ = recover(&root, provider);
        }
      }
    }
    Self(Arc::new(Inner {
      root,
      _temporary: temporary,
      jobs: Mutex::new(BTreeMap::new()),
      tasks: Mutex::new(Vec::new()),
      changed: broadcast::channel(64).0,
      closed: AtomicBool::new(false),
      latest: Mutex::new(BTreeMap::new()),
    }))
  }
  pub fn root(&self) -> &Path {
    &self.0.root
  }
  pub fn subscribe(&self) -> broadcast::Receiver<RuntimeInstallJob> {
    self.0.changed.subscribe()
  }
  pub async fn inventory(&self) -> Result<RuntimeInstallInventory, String> {
    let this = self.clone();
    tokio::task::spawn_blocking(move || this.inventory_sync())
      .await
      .map_err(|_| "Could not inspect agent installations")?
  }
  fn inventory_sync(&self) -> Result<RuntimeInstallInventory, String> {
    let jobs = self.0.jobs.lock().unwrap_or_else(|e| e.into_inner());
    let agents = PROVIDERS
      .into_iter()
      .map(|provider| {
        let requires_node = matches!(provider, Provider::Pi | Provider::Dsh);
        let status = if requires_node {
          package::node_tools().map(|_| ())
        } else {
          download::asset_name(provider).map(|_| ())
        };
        let record = read_record(&self.0.root, provider);
        let prerequisite_error = status.err();
        let lock_status = if self.0.root.exists() {
          filesystem::in_use(&self.0.root, provider)
        } else {
          Ok(false)
        };
        let installed_error = record
          .as_ref()
          .ok()
          .and_then(|value| value.as_ref())
          .and_then(|record| filesystem::validate_staged(&self.0.root.join(provider_name(provider)), record).err());
        let error = prerequisite_error
          .clone()
          .or_else(|| record.as_ref().err().cloned())
          .or(installed_error)
          .or_else(|| lock_status.as_ref().err().cloned());
        let record = record.ok().flatten();
        let in_use = lock_status.unwrap_or(false);
        AgentInstallation {
          provider,
          version: record.as_ref().map(|v| v.version.clone()),
          generation: record.map(|v| v.generation),
          requires_node,
          available: prerequisite_error.is_none(),
          unavailable_reason: error,
          in_use,
          job: self
            .0
            .latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(provider_name(provider))
            .and_then(|id| jobs.get(id))
            .map(|entry| entry.job.clone()),
        }
      })
      .collect();
    Ok(RuntimeInstallInventory { agents })
  }
  pub async fn start(&self, request: RuntimeInstallRequest) -> Result<RuntimeInstallJob, String> {
    uuid::Uuid::parse_str(&request.request_id).map_err(|_| "Installation request ID must be a UUID")?;
    if self.0.closed.load(Ordering::Acquire) {
      return Err("Agent installation service is stopping".into());
    }
    let mut jobs = self.0.jobs.lock().unwrap_or_else(|e| e.into_inner());
    if self.0.closed.load(Ordering::Acquire) {
      return Err("Agent installation service is stopping".into());
    }
    if let Some(previous) = jobs.values().find(|entry| entry.job.request_id == request.request_id) {
      return if previous.job.provider == request.provider {
        Ok(previous.job.clone())
      } else {
        Err("Installation request ID belongs to another agent".into())
      };
    }
    if jobs
      .values()
      .any(|entry| entry.job.provider == request.provider && !entry.job.terminal())
    {
      return Err("An installation is already running for this agent".into());
    }
    if jobs.len() >= MAX_INSTALL_REQUESTS {
      // Preserve request receipts rather than silently replaying an old install.
      return Err("Installation request limit reached; restart this host service before installing again".into());
    }
    let lease = exclusive_lock(&self.0.root, request.provider)?;
    recover(&self.0.root, request.provider)?;
    let job = RuntimeInstallJob {
      job_id: uuid::Uuid::new_v4().to_string(),
      request_id: request.request_id,
      provider: request.provider,
      state: "resolving".into(),
      downloaded_bytes: 0,
      total_bytes: None,
      error: None,
    };
    let cancel = CancellationToken::new();
    jobs.insert(
      job.job_id.clone(),
      JobEntry {
        job: job.clone(),
        cancel: cancel.clone(),
      },
    );
    self
      .0
      .latest
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .insert(provider_name(request.provider).into(), job.job_id.clone());
    let this = self.clone();
    let id = job.job_id.clone();
    let task = tokio::spawn(async move {
      let result = this.install(&id, request.provider, &cancel).await;
      // Keep the exclusive lease until every subprocess has been reaped and all
      // staging/replacement work has completed, including cancellation.
      drop(lease);
      match result {
        Ok(()) => this.progress(&id, "completed", None, None),
        Err(error) => this.finish_error(&id, &cancel, error),
      }
    });
    self.0.tasks.lock().unwrap_or_else(|e| e.into_inner()).push(task);
    let _ = self.0.changed.send(job.clone());
    Ok(job)
  }
  pub async fn cancel(&self, request: RuntimeInstallCancelRequest) -> Result<RuntimeInstallJob, String> {
    let jobs = self.0.jobs.lock().unwrap_or_else(|e| e.into_inner());
    let entry = jobs
      .get(&request.job_id)
      .ok_or("Installation job was not found on this host")?;
    entry.cancel.cancel();
    Ok(entry.job.clone())
  }
  pub async fn remove(&self, request: RuntimeInstallRemoveRequest) -> Result<RuntimeInstallInventory, String> {
    if self.0.closed.load(Ordering::Acquire) {
      return Err("Agent installation service is stopping".into());
    }
    let this = self.clone();
    tokio::task::spawn_blocking(move || {
      let _lease = exclusive_lock(&this.0.root, request.provider)?;
      recover(&this.0.root, request.provider)?;
      let current = read_record(&this.0.root, request.provider)?.ok_or("This agent is not installed by Tokn")?;
      if current.generation != request.generation {
        return Err("The agent installation changed; refresh before removing it".to_string());
      }
      let directory = this.0.root.join(provider_name(request.provider));
      std::fs::remove_dir_all(directory).map_err(|_| "Could not remove this agent installation")?;
      this
        .0
        .latest
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(provider_name(request.provider));
      drop(_lease);
      let _ = this.0.changed.send(RuntimeInstallJob {
        job_id: uuid::Uuid::new_v4().to_string(),
        request_id: uuid::Uuid::new_v4().to_string(),
        provider: request.provider,
        state: "completed".into(),
        downloaded_bytes: 0,
        total_bytes: None,
        error: None,
      });
      this.inventory_sync()
    })
    .await
    .map_err(|_| "Could not remove agent installation")?
  }
  pub async fn shutdown(&self) {
    // Fence admission and collect handles under the same job mutex used by start.
    {
      let jobs = self.0.jobs.lock().unwrap_or_else(|e| e.into_inner());
      self.0.closed.store(true, Ordering::Release);
      for entry in jobs.values() {
        entry.cancel.cancel();
      }
    }
    let tasks = std::mem::take(&mut *self.0.tasks.lock().unwrap_or_else(|e| e.into_inner()));
    for task in tasks {
      let _ = task.await;
    }
  }
  fn progress(&self, id: &str, state: &str, bytes: Option<u64>, total: Option<u64>) {
    let mut jobs = self.0.jobs.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = jobs.get_mut(id) {
      entry.job.state = state.into();
      if let Some(bytes) = bytes {
        entry.job.downloaded_bytes = bytes;
      }
      if total.is_some() {
        entry.job.total_bytes = total;
      }
      let _ = self.0.changed.send(entry.job.clone());
    }
  }
  fn finish_error(&self, id: &str, cancel: &CancellationToken, error: String) {
    let mut jobs = self.0.jobs.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = jobs.get_mut(id) {
      entry.job.state = if cancel.is_cancelled() { "cancelled" } else { "failed" }.into();
      entry.job.error = Some(error);
      let _ = self.0.changed.send(entry.job.clone());
    }
  }
  async fn install(&self, id: &str, provider: Provider, cancel: &CancellationToken) -> Result<(), String> {
    let staging = tempfile::Builder::new()
      .prefix(&format!(".{}-staging-", provider_name(provider)))
      .tempdir_in(&self.0.root)
      .map_err(|_| "Could not create a private installation staging directory")?;
    let mut record = if matches!(provider, Provider::Pi | Provider::Dsh) {
      package::install(self, id, provider, staging.path(), cancel).await?
    } else {
      download::install(self, id, provider, staging.path(), cancel).await?
    };
    check_cancel(cancel)?;
    record.generation = uuid::Uuid::new_v4().to_string();
    filesystem::write_record(staging.path(), &record)?;
    self.progress(id, "verifying", None, None);
    filesystem::validate_staged(staging.path(), &record)?;
    if !record.node {
      let mut command = tokio::process::Command::new(staging.path().join(&record.entrypoint));
      command.arg("--version").current_dir(staging.path());
      package::run(&mut command, cancel, std::time::Duration::from_secs(15))
        .await
        .map_err(|_| "The downloaded agent failed its version check; the previous installation was retained")?;
    }
    check_cancel(cancel)?;
    replace(&self.0.root, provider, staging.path())?;
    Ok(())
  }
}
fn check_cancel(cancel: &CancellationToken) -> Result<(), String> {
  if cancel.is_cancelled() {
    Err("Agent installation cancelled".into())
  } else {
    Ok(())
  }
}
