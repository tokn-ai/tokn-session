//! Host-only managed human checkpoints. No request enters the generic broker.
use crate::{Inner, mapping, wire};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
  collections::BTreeMap,
  future::Future,
  pin::Pin,
  sync::{Arc, Mutex, atomic::Ordering},
};
use tokio::sync::watch;

pub type HumanCommandReviewer =
  Arc<dyn Fn(HumanCommandApproval) -> Pin<Box<dyn Future<Output = HumanCommandDecision> + Send>> + Send + Sync>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HumanCommandDecision {
  AllowOnce,
  Decline,
  Cancel,
}

/// Exact native operation details. Deliberately not serializable.
#[derive(Clone, Debug)]
pub struct HumanCommandApproval {
  pub command: String,
  pub cwd: String,
  pub reason: Option<String>,
  pub environment_id: Option<String>,
  pub additional_permissions: Option<Value>,
  pub thread_id: String,
  pub turn_id: String,
  pub item_id: String,
  pub approval_id: String,
  pub cancellation: HumanCommandCancellation,
}

#[derive(Clone, Debug)]
pub struct HumanCommandCancellation {
  receiver: watch::Receiver<bool>,
}
impl HumanCommandCancellation {
  pub fn is_cancelled(&self) -> bool {
    *self.receiver.borrow() || self.receiver.has_changed().is_err()
  }
  pub async fn cancelled(&mut self) {
    while !*self.receiver.borrow() {
      if self.receiver.changed().await.is_err() {
        return;
      }
    }
  }
}

const MAX_ACTIVE: usize = 16;
const MAX_RECORDS: usize = 256;
struct Record {
  fingerprint: [u8; 32],
  thread: Option<String>,
  approval: Option<String>,
  cancel: watch::Sender<bool>,
  retired: bool,
  responding: bool,
}
#[derive(Default)]
struct State {
  records: BTreeMap<wire::RpcId, Record>,
}
pub(super) struct HumanCommands {
  reviewer: Option<HumanCommandReviewer>,
  state: Mutex<State>,
}
impl HumanCommands {
  pub(super) fn new(reviewer: Option<HumanCommandReviewer>) -> Self {
    Self {
      reviewer,
      state: Mutex::new(State::default()),
    }
  }
  pub(super) fn enabled(&self) -> bool {
    self.reviewer.is_some()
  }
  pub(super) fn contains(&self, id: &wire::RpcId) -> bool {
    self
      .state
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .records
      .contains_key(id)
  }
  pub(super) fn cancel_all(&self) {
    for record in self
      .state
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .records
      .values_mut()
    {
      record.retired = true;
      record.cancel.send_replace(true);
    }
  }
  pub(super) fn notification(&self, method: &str, params: &Value) {
    if !matches!(method, "serverRequest/resolved" | "thread/closed" | "thread/archived") {
      return;
    }
    let Some(thread) = params["threadId"].as_str() else {
      return;
    };
    let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
    let retire = |record: &mut Record| {
      if record.thread.as_deref() == Some(thread) {
        record.retired = true;
        record.cancel.send_replace(true);
      }
    };
    if method == "serverRequest/resolved" {
      if let Ok(id) = serde_json::from_value::<wire::RpcId>(params["requestId"].clone())
        && let Some(record) = state.records.get_mut(&id)
      {
        retire(record);
      }
    } else {
      state.records.values_mut().for_each(retire);
    }
  }
}

impl Inner {
  pub(super) async fn human_request(self: &Arc<Self>, id: wire::RpcId, params: Value) {
    if self
      .snapshot
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .interactions
      .values()
      .any(|record| record.id == id)
    {
      self.disconnect("Codex reused a pending request ID for a human checkpoint".into());
      let _ = self.transport.close().await;
      return;
    }
    let fingerprint: [u8; 32] = Sha256::digest(serde_json::to_vec(&params).expect("JSON value serializes")).into();
    let (cancel, receiver) = watch::channel(false);
    let cancellation = HumanCommandCancellation { receiver };
    let parsed = parse(&params, cancellation.clone());
    let mut review = None;
    let mut conflict = false;
    {
      let mut state = self
        .human_commands
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
      if let Some(previous) = state.records.get(&id) {
        if previous.fingerprint == fingerprint {
          return;
        }
        conflict = true;
      } else {
        if state.records.len() >= MAX_RECORDS {
          if let Some(oldest) = state
            .records
            .iter()
            .find(|(_, record)| record.retired)
            .map(|(id, _)| id.clone())
          {
            state.records.remove(&oldest);
          }
        }
        // Failed/queued writes remain live until their worker retires them.
        // A malicious peer cannot grow the private ledger by flooding denials.
        if state.records.len() >= MAX_RECORDS {
          conflict = true;
        }
        let active = state.records.values().filter(|record| !record.retired).count();
        let duplicate_business = parsed.as_ref().is_some_and(|request| {
          state.records.values().any(|record| {
            record.thread.as_deref() == Some(request.thread_id.as_str())
              && record.approval.as_deref() == Some(request.approval_id.as_str())
          })
        });
        if active < MAX_ACTIVE && !duplicate_business {
          if let (Some(request), Some(reviewer)) = (parsed.clone(), self.human_commands.reviewer.clone()) {
            review = Some((request, reviewer));
          }
        }
        if !conflict {
          state.records.insert(
            id.clone(),
            Record {
              fingerprint,
              thread: params["threadId"]
                .as_str()
                .filter(|id| mapping::valid_id(id))
                .map(str::to_owned),
              approval: params["approvalId"]
                .as_str()
                .filter(|id| mapping::valid_id(id))
                .map(str::to_owned),
              cancel,
              retired: false,
              responding: false,
            },
          );
        }
      }
    }
    if conflict {
      self.disconnect("Codex human checkpoint request identity is conflicting or its bounded ledger is full".into());
      let _ = self.transport.close().await;
      return;
    }
    if let Some((approval, reviewer)) = review {
      let weak = Arc::downgrade(self);
      tokio::spawn(async move {
        // A callback panic cannot turn into an approval. Its detached task is
        // aborted when Codex retires the request or this adapter is closed.
        let mut task = tokio::spawn(async move { reviewer(approval).await });
        let mut cancelled = cancellation.clone();
        let decision = tokio::select! {
          biased;
          _ = cancelled.cancelled() => { task.abort(); return; },
          decision = &mut task => decision.unwrap_or(HumanCommandDecision::Decline),
        };
        if let Some(inner) = weak.upgrade() {
          inner.human_decision(id, decision, cancellation).await;
        }
      });
    } else {
      let weak = Arc::downgrade(self);
      tokio::spawn(async move {
        if let Some(inner) = weak.upgrade() {
          inner
            .human_decision(id, HumanCommandDecision::Decline, cancellation)
            .await;
        }
      });
    }
  }

  async fn human_decision(
    &self,
    id: wire::RpcId,
    decision: HumanCommandDecision,
    cancellation: HumanCommandCancellation,
  ) {
    {
      let mut state = self
        .human_commands
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
      let Some(record) = state.records.get_mut(&id) else {
        return;
      };
      if record.retired || record.responding || cancellation.is_cancelled() || self.disconnected.load(Ordering::Acquire)
      {
        return;
      }
      record.responding = true;
    }
    let decision = match decision {
      HumanCommandDecision::AllowOnce => "accept",
      HumanCommandDecision::Decline => "decline",
      HumanCommandDecision::Cancel => "cancel",
    };
    // The transport rechecks cancellation after acquiring its writer. Once a
    // frame starts, finish it or close the transport; never retry its outcome.
    let result = self
      .transport
      .respond_unless_cancelled(&id, json!({"decision":decision}), &cancellation.receiver)
      .await;
    if let Some(record) = self
      .human_commands
      .state
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .records
      .get_mut(&id)
    {
      record.retired = true;
      record.cancel.send_replace(true);
    }
    if result.is_err() {
      self.disconnect("Codex human checkpoint response could not be confirmed; it was not retried".into());
      let _ = self.transport.close().await;
    }
  }
}

fn parse(params: &Value, cancellation: HumanCommandCancellation) -> Option<HumanCommandApproval> {
  if serde_json::to_vec(params).ok()?.len() > 256 * 1024 {
    return None;
  }
  let parsed: wire::CommandExecutionRequestApprovalParams = serde_json::from_value(params.clone()).ok()?;
  let approval_id = parsed.approval_id.filter(|id| mapping::valid_id(id))?;
  let command = parsed
    .command
    .filter(|command| !command.trim().is_empty() && command.len() <= 64 * 1024 && !command.contains('\0'))?;
  let cwd = parsed
    .cwd
    .filter(|cwd| cwd.len() <= 16 * 1024 && !cwd.contains('\0') && std::path::Path::new(cwd).is_absolute())?;
  if ![&parsed.thread_id, &parsed.turn_id, &parsed.item_id]
    .into_iter()
    .all(|id| mapping::valid_id(id))
    || parsed.reason.as_ref().is_some_and(|value| value.len() > 16 * 1024)
    || parsed.environment_id.as_ref().is_some_and(|id| !mapping::valid_id(id))
    || parsed
      .additional_permissions
      .as_ref()
      .is_some_and(|value| serde_json::to_vec(value).map_or(true, |value| value.len() > 32 * 1024))
    || parsed.available_decisions.as_ref().is_none_or(|values| {
      values.len() != 3
        || ["accept", "decline", "cancel"]
          .into_iter()
          .any(|decision| values.iter().filter(|value| value.as_str() == Some(decision)).count() != 1)
    })
  {
    return None;
  }
  Some(HumanCommandApproval {
    command,
    cwd,
    reason: parsed.reason,
    environment_id: parsed.environment_id,
    additional_permissions: parsed.additional_permissions,
    thread_id: parsed.thread_id,
    turn_id: parsed.turn_id,
    item_id: parsed.item_id,
    approval_id,
    cancellation,
  })
}
