//! Native execution hints start a passive monitor; only the process-owned
//! active map proves that a busy period has finished, across both dialects.
use super::*;
use std::{collections::BTreeSet, sync::Mutex as StdMutex};

const MAX_SESSIONS: usize = 4096;

#[derive(Default)]
struct Cache {
  states: BTreeMap<String, SessionState>,
  tracked: BTreeSet<String>,
  epoch: u64,
}

pub(super) struct LiveState {
  descriptor: RuntimeDescriptor,
  events: broadcast::Sender<RuntimeEvent>,
  sequence: StdMutex<u64>,
  cache: StdMutex<Cache>,
}

impl LiveState {
  pub(super) fn new(descriptor: RuntimeDescriptor, events: broadcast::Sender<RuntimeEvent>) -> Self {
    Self {
      descriptor,
      events,
      sequence: StdMutex::new(0),
      cache: StdMutex::new(Cache::default()),
    }
  }

  pub(super) fn emit(&self, session_id: Option<String>, kind: RuntimeEventKind) {
    let mut sequence = self.sequence.lock().expect("OpenCode publication lock");
    *sequence += 1;
    let _ = self.events.send(RuntimeEvent {
      runtime_id: self.descriptor.runtime_id.clone(),
      instance_id: self.descriptor.instance_id.clone(),
      session_id,
      sequence: *sequence,
      kind,
    });
  }

  /// Only exact metadata reads and acknowledged creates establish monitored
  /// identities. Unrelated global SSE traffic never adds sessions or reads.
  pub(super) fn remember_info(&self, info: &wire::SessionInfo) {
    let mut cache = self.cache.lock().expect("OpenCode live state lock");
    if !cache.states.contains_key(&info.id) && cache.states.len() >= MAX_SESSIONS {
      return;
    }
    let state = cache.states.entry(info.id.clone()).or_insert_with(|| SessionState {
      session_id: Some(info.id.clone()),
      status: ExecutionState::Unknown,
      model: None,
      pending_inputs: None,
      revision: None,
    });
    // Lifecycle events contain no model identity. Retain the last known
    // selection until a complete authoritative State read replaces it.
    if let Some(model) = &info.model {
      state.model = Some(normalize::model(model));
    }
  }

  pub(super) fn epoch(&self) -> u64 {
    self.cache.lock().expect("OpenCode live state lock").epoch
  }

  pub(super) fn remember_state(&self, state: &SessionState, epoch: u64) {
    let Some(id) = &state.session_id else {
      return;
    };
    let mut cache = self.cache.lock().expect("OpenCode live state lock");
    if cache.epoch != epoch {
      return;
    }
    if !cache.states.contains_key(id) && cache.states.len() >= MAX_SESSIONS {
      return;
    }
    let changed = cache.states.get(id).is_some_and(|previous| {
      previous.status != ExecutionState::Unknown
        && (previous.status != state.status
          || previous.model != state.model
          || previous.pending_inputs != state.pending_inputs
          || previous.revision != state.revision)
    });
    cache.states.insert(id.clone(), state.clone());
    if state.status == ExecutionState::Running {
      cache.tracked.insert(id.clone());
    }
    if changed {
      cache.epoch = cache.epoch.wrapping_add(1);
      self.emit(Some(id.clone()), RuntimeEventKind::State(state.clone()));
    }
  }

  pub(super) fn event(&self, native: &wire::NativeEvent) {
    if !matches!(
      native.kind.as_str(),
      "session.execution.started" | "session.next.step.started"
    ) {
      return;
    }
    let Some(id) = native.data["sessionID"].as_str() else {
      return;
    };
    let mut cache = self.cache.lock().expect("OpenCode live state lock");
    let Some(state) = cache.states.get_mut(id) else {
      return;
    };
    state.status = ExecutionState::Running;
    let state = state.clone();
    cache.tracked.insert(id.into());
    cache.epoch = cache.epoch.wrapping_add(1);
    self.emit(Some(id.into()), RuntimeEventKind::State(state));
  }

  pub(super) fn has_running(&self) -> bool {
    !self.cache.lock().expect("OpenCode live state lock").tracked.is_empty()
  }

  pub(super) fn owns_unscoped_lifecycle(&self, kind: &str, id: &str) -> bool {
    matches!(
      kind,
      "session.execution.started"
        | "session.execution.succeeded"
        | "session.execution.failed"
        | "session.execution.interrupted"
    ) && self
      .cache
      .lock()
      .expect("OpenCode live state lock")
      .states
      .contains_key(id)
  }

  pub(super) fn active(&self, active: &BTreeMap<String, Value>, epoch: u64) {
    let mut cache = self.cache.lock().expect("OpenCode live state lock");
    if cache.epoch != epoch {
      return;
    }
    let mut changed = Vec::new();
    for id in cache.tracked.iter().cloned().collect::<Vec<_>>() {
      let status = if active.contains_key(&id) {
        ExecutionState::Running
      } else {
        ExecutionState::Idle
      };
      let state = cache.states.get_mut(&id).expect("tracked exact session");
      if state.status != status {
        state.status = status;
        changed.push((id.clone(), state.clone()));
      }
      if status == ExecutionState::Idle {
        cache.tracked.remove(&id);
      }
    }
    if !changed.is_empty() {
      cache.epoch = cache.epoch.wrapping_add(1);
    }
    for (id, state) in changed {
      self.emit(Some(id), RuntimeEventKind::State(state));
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn setup() -> (LiveState, broadcast::Receiver<RuntimeEvent>) {
    let (events, receiver) = broadcast::channel(32);
    let descriptor = RuntimeDescriptor {
      runtime_id: "fixture".into(),
      instance_id: "instance".into(),
      provider: Provider::OpenCode,
      ownership: Ownership::Attached,
      capabilities: Capabilities::default(),
    };
    (LiveState::new(descriptor, events), receiver)
  }

  #[test]
  fn both_native_start_events_preserve_model_until_active_map_confirms_idle() {
    for kind in ["session.execution.started", "session.next.step.started"] {
      let (live, mut events) = setup();
      let state = SessionState {
        session_id: Some("session".into()),
        status: ExecutionState::Idle,
        model: Some(ModelIdentity {
          model_provider: Some("fixture".into()),
          model_id: "model".into(),
          display_name: None,
        }),
        pending_inputs: None,
        revision: None,
      };
      live.remember_state(&state, live.epoch());
      assert!(events.try_recv().is_err());
      let native =
        |kind: &str| serde_json::from_value(json!({"id":"event","type":kind,"data":{"sessionID":"session"}})).unwrap();
      live.event(&native(kind));
      let RuntimeEventKind::State(running) = events.try_recv().unwrap().kind else {
        panic!("missing running state")
      };
      assert_eq!(running.status, ExecutionState::Running);
      assert_eq!(running.model, state.model);
      live.event(&native("session.next.step.ended"));
      live.event(&native("session.execution.succeeded"));
      assert!(
        events.try_recv().is_err(),
        "steps and settlement hooks cannot prove process idle"
      );
      live.active(&BTreeMap::from([("session".into(), json!({}))]), live.epoch());
      assert!(events.try_recv().is_err());
      assert!(live.has_running());
      live.active(&BTreeMap::new(), live.epoch());
      let RuntimeEventKind::State(idle) = events.try_recv().unwrap().kind else {
        panic!("missing idle state")
      };
      assert_eq!(idle.status, ExecutionState::Idle);
      assert_eq!(idle.model, state.model);
      assert!(!live.has_running());
      live.active(&BTreeMap::new(), live.epoch());
      assert!(events.try_recv().is_err());
    }
  }

  #[test]
  fn unrelated_native_traffic_does_not_activate_monitoring() {
    let (live, mut events) = setup();
    live.event(
      &serde_json::from_value(
        json!({"id":"event","type":"session.execution.started","data":{"sessionID":"unverified"}}),
      )
      .unwrap(),
    );
    assert!(!live.has_running());
    assert!(events.try_recv().is_err());
    assert!(!live.owns_unscoped_lifecycle("session.execution.started", "unverified"));
  }

  #[test]
  fn a_passive_read_started_before_new_native_activity_cannot_publish_stale_idle() {
    let (live, mut events) = setup();
    let idle = SessionState {
      session_id: Some("session".into()),
      status: ExecutionState::Idle,
      model: None,
      pending_inputs: None,
      revision: None,
    };
    live.remember_state(&idle, live.epoch());
    assert!(events.try_recv().is_err());
    let before = live.epoch();
    live.event(
      &serde_json::from_value(json!({"id":"event","type":"session.execution.started","data":{"sessionID":"session"}}))
        .unwrap(),
    );
    events.try_recv().unwrap();
    live.remember_state(&idle, before);
    live.active(&BTreeMap::new(), before);
    assert!(events.try_recv().is_err());
    assert!(live.has_running());
    live.active(&BTreeMap::new(), live.epoch());
    assert!(matches!(
      events.try_recv().unwrap().kind,
      RuntimeEventKind::State(SessionState {
        status: ExecutionState::Idle,
        ..
      })
    ));
  }

  #[test]
  fn changed_reads_wake_existing_observers_without_self_invalidating_first_baseline() {
    let (live, mut events) = setup();
    let mut state = SessionState {
      session_id: Some("session".into()),
      status: ExecutionState::Idle,
      model: None,
      pending_inputs: None,
      revision: None,
    };
    live.remember_state(&state, live.epoch());
    assert!(events.try_recv().is_err());
    assert!(live.owns_unscoped_lifecycle("session.execution.started", "session"));
    assert!(!live.owns_unscoped_lifecycle("arbitrary.event", "session"));
    state.status = ExecutionState::Running;
    live.remember_state(&state, live.epoch());
    assert!(matches!(
      events.try_recv().unwrap().kind,
      RuntimeEventKind::State(SessionState {
        status: ExecutionState::Running,
        ..
      })
    ));
    assert!(live.has_running());
  }
}
