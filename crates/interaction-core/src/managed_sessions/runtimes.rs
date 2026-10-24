use super::*;
use tokio::sync::OwnedMutexGuard;

impl ManagedSessions {
  pub async fn list_runtimes(&self) -> Vec<ManagedRuntimeInfo> {
    let ids: Vec<_> = self.inner.records.lock().unwrap().keys().cloned().collect();
    let mut result = Vec::with_capacity(ids.len());
    for id in ids {
      let stopped = {
        let records = self.inner.records.lock().unwrap();
        records
          .get(&id)
          .filter(|record| record.sender.is_none() && !restart_pending(record))
          .map(|record| record.start_gate.clone())
      };
      let unavailable = if let Some(gate) = stopped {
        self.inspect_stopped(&id, gate).await
      } else {
        None
      };
      if let Some(record) = self.inner.records.lock().unwrap().get(&id) {
        result.push(runtime_info(
          record,
          self.inner.provider,
          self.available().err().or(unavailable),
        ));
      }
    }
    result.sort_by(|left, right| {
      let dormant = |runtime: &ManagedRuntimeInfo| matches!(runtime.status.as_str(), "stopped" | "failed");
      dormant(left)
        .cmp(&dormant(right))
        .then_with(|| right.started_at_ms.cmp(&left.started_at_ms))
        .then_with(|| left.runtime_id.cmp(&right.runtime_id))
    });
    result
  }

  async fn inspect_stopped(&self, id: &str, gate: Arc<AsyncMutex<()>>) -> Option<String> {
    // Inventory must not wait for a slow startup or restart to become ready.
    // A lifecycle event will refresh the temporary unavailable capability.
    let Ok(_guard) = gate.try_lock_owned() else {
      return Some("The agent worker is changing; refresh after it is ready".into());
    };
    let receipt = {
      let records = self.inner.records.lock().unwrap();
      records
        .get(id)
        .filter(|record| record.sender.is_none())
        .map(|record| record.receipt.clone())
    }?;
    let store = self.inner.store.clone();
    // Inspect only the known header under its process lease. Re-reading the
    // entire receipt catalog for every row would make live inventory quadratic.
    // Restart still reloads and verifies its latest durable receipt on admission.
    tokio::task::spawn_blocking(move || {
      let _lease = store.lease(&receipt.request_id)?;
      store.validate_resume_target(&receipt)
    })
    .await
    .map_err(|error| error.to_string())
    .and_then(|result| result)
    .err()
  }

  pub async fn restart_runtime(&self, request: &RuntimeRestartRequest) -> Result<(), String> {
    let manager = self.clone();
    let request = request.clone();
    tokio::spawn(async move { manager.restart_inner(request).await })
      .await
      .map_err(|error| error.to_string())?
  }

  async fn restart_inner(&self, request: RuntimeRestartRequest) -> Result<(), String> {
    validate_uuid(&request.request_id, "runtime restart request")?;
    self.available()?;
    let id = request
      .runtime_id
      .strip_prefix(&format!("{}:", provider_name(self.inner.provider)))
      .ok_or("Unknown managed runtime")?
      .to_owned();
    validate_uuid(&id, "creation request")?;
    let gate = self.start_gate(&id)?;
    let guard = gate.clone().lock_owned().await;
    let receipt = {
      let records = self.inner.records.lock().unwrap();
      let record = records.get(&id).ok_or("Managed session disappeared")?;
      if record.instance_id != request.expected_instance_id {
        return Err("The agent worker changed. Refresh runtimes before restarting it.".into());
      }
      if restart_pending(record) {
        return Err("This agent worker is already restarting".into());
      }
      record.receipt.clone()
    };
    // Do not terminate an active worker if its conversation cannot be resumed.
    let store = self.inner.store.clone();
    tokio::task::spawn_blocking(move || store.validate_resume_target(&receipt))
      .await
      .map_err(|error| error.to_string())??;
    self.available()?;
    let (done, completion) = watch::channel(false);
    let (termination, state, snapshot) = {
      let mut records = self.inner.records.lock().unwrap();
      let record = records.get_mut(&id).ok_or("Managed session disappeared")?;
      record.restarting = Some(completion);
      (
        record.termination.clone(),
        record.worker_state.clone(),
        record.receipt.snapshot(),
      )
    };
    let _ = self.inner.events.send(snapshot);
    if let Some(termination) = termination {
      termination.cancel();
    }
    // Cleanup needs the admission gate to reap the child and release its lease.
    // A concurrent Send waits on completion outside that gate.
    drop(guard);
    let result = self.replace_worker(&id, gate, state).await;
    let snapshot = {
      let mut records = self.inner.records.lock().unwrap();
      records.get_mut(&id).map(|record| {
        record.restarting = None;
        record.receipt.snapshot()
      })
    };
    done.send_replace(true);
    if let Some(snapshot) = snapshot {
      let _ = self.inner.events.send(snapshot);
    }
    result
  }

  async fn replace_worker(
    &self,
    id: &str,
    gate: Arc<AsyncMutex<()>>,
    state: Option<watch::Receiver<WorkerState>>,
  ) -> Result<(), String> {
    if let Some(mut state) = state {
      loop {
        if matches!(*state.borrow_and_update(), WorkerState::Stopped(_)) {
          break;
        }
        state
          .changed()
          .await
          .map_err(|_| "Agent worker cleanup did not complete")?;
      }
    }
    let _guard = gate.lock_owned().await;
    self.available()?;
    self.start_locked(id.to_owned(), None, true).await?;
    let mut state = self
      .inner
      .records
      .lock()
      .unwrap()
      .get(id)
      .and_then(|record| record.worker_state.clone())
      .ok_or("Agent worker did not start")?;
    loop {
      let current = state.borrow_and_update().clone();
      match current {
        WorkerState::Ready => return Ok(()),
        WorkerState::Closing(error) | WorkerState::Stopped(error) => {
          return Err(error.unwrap_or_else(|| "The agent stopped before it was ready".into()));
        }
        WorkerState::Starting => {}
      }
      state
        .changed()
        .await
        .map_err(|_| "The agent stopped before it was ready")?;
    }
  }

  pub(super) async fn admission_gate(
    &self,
    id: &str,
    gate: Arc<AsyncMutex<()>>,
  ) -> Result<OwnedMutexGuard<()>, String> {
    loop {
      self.available()?;
      let guard = gate.clone().lock_owned().await;
      let completion = {
        let records = self.inner.records.lock().unwrap();
        let record = records.get(id).ok_or("Managed session disappeared")?;
        record.restarting.clone().filter(|completion| !*completion.borrow())
      };
      let Some(mut completion) = completion else {
        return Ok(guard);
      };
      drop(guard);
      while !*completion.borrow_and_update() {
        completion
          .changed()
          .await
          .map_err(|_| "Agent worker restart was interrupted")?;
      }
    }
  }
}

fn restart_pending(record: &Record) -> bool {
  record
    .restarting
    .as_ref()
    .is_some_and(|completion| !*completion.borrow())
}

fn runtime_info(record: &Record, provider: crate::Provider, unavailable: Option<String>) -> ManagedRuntimeInfo {
  let receipt = &record.receipt;
  let restarting = restart_pending(record);
  let has_history = receipt.session_key.is_some() && receipt.session_file.is_some() && receipt.session_id.is_some();
  let external = unavailable
    .as_deref()
    .is_some_and(|message| message.contains("already has an owning host runtime"));
  let reason = unavailable.or_else(|| {
    if restarting {
      Some("This agent worker is restarting".into())
    } else if !has_history {
      Some("The agent has not persisted a conversation to resume".into())
    } else if receipt.status == ManagedSessionStatus::Starting {
      Some("The agent is starting".into())
    } else {
      None
    }
  });
  ManagedRuntimeInfo {
    runtime_id: format!(
      "{}:{}",
      receipt.provider.map(provider_name).unwrap_or("unknown"),
      receipt.request_id
    ),
    instance_id: record.instance_id.clone(),
    provider,
    externally_owned: external,
    has_worker: record.sender.is_some(),
    status: if restarting {
      "restarting"
    } else if external {
      "external"
    } else {
      match receipt.status {
        ManagedSessionStatus::Starting => "starting",
        ManagedSessionStatus::Running => "running",
        ManagedSessionStatus::Idle => "idle",
        ManagedSessionStatus::Stopping => "stopping",
        ManagedSessionStatus::Stopped => "stopped",
        ManagedSessionStatus::Failed => "failed",
      }
    }
    .into(),
    pid: record.worker_pid,
    started_at_ms: record.started_at_ms,
    session_key: receipt.session_key.clone(),
    session_id: receipt.session_id.clone(),
    project_id: receipt.project_id.clone(),
    working_directory: receipt.folder.to_string_lossy().into_owned(),
    last_error: if receipt.status == ManagedSessionStatus::Failed {
      receipt.message.clone().or_else(|| receipt.last_turn_error.clone())
    } else {
      receipt.last_turn_error.clone()
    },
    can_restart: reason.is_none(),
    restart_disabled_reason: reason,
  }
}
