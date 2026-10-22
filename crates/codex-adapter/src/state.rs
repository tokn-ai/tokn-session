use super::*;

impl Inner {
  pub(super) fn ensure_open(&self) -> Result<(), InteractionError> {
    if self.transport.is_closed() {
      Err(InteractionError::unavailable("Codex app-server connection is closed"))
    } else {
      Ok(())
    }
  }
  pub(super) fn check_identity(&self, target: &SessionTarget) -> Result<(), InteractionError> {
    if target.runtime_id != self.descriptor.runtime_id || target.instance_id != self.descriptor.instance_id {
      return Err(InteractionError::new(
        ErrorKind::StaleTarget,
        "Codex runtime instance changed",
      ));
    }
    if target.session_id.as_deref().is_some_and(|id| !mapping::valid_id(id)) {
      return Err(InteractionError::invalid("Invalid Codex thread ID"));
    }
    Ok(())
  }
  pub(super) fn check_target(&self, target: &SessionTarget) -> Result<(), InteractionError> {
    self.check_identity(target)?;
    self.ensure_open()
  }
  pub(super) fn epoch(&self) -> u64 {
    self.snapshot.lock().unwrap_or_else(|error| error.into_inner()).epoch
  }
  pub(super) fn accept_thread(&self, thread: &Value, expected: Option<u64>) -> Result<SessionRecord, InteractionError> {
    let id = thread
      .get("id")
      .and_then(Value::as_str)
      .filter(|id| mapping::valid_id(id))
      .ok_or_else(|| InteractionError::unavailable("Codex thread identity is invalid"))?;
    let mut snapshot = self.snapshot.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(record) = snapshot.sessions.get(id) {
      if expected.is_some_and(|epoch| record.generation > epoch) {
        return Ok(record.clone());
      }
    } else if snapshot.sessions.len() >= MAX_SESSIONS {
      return Err(InteractionError::unavailable(
        "Codex session cache is full; attach a new runtime",
      ));
    }
    snapshot.epoch = snapshot.epoch.saturating_add(1);
    let generation = snapshot.epoch;
    let previous = snapshot.sessions.get(id);
    let status = mapping::status(&thread["status"]);
    let turn_id = thread
      .get("turns")
      .filter(|_| status == ExecutionState::Running)
      .and_then(Value::as_array)
      .and_then(|turns| turns.iter().rev().find(|turn| turn["status"] == "inProgress"))
      .and_then(|turn| turn["id"].as_str())
      .map(str::to_owned)
      .or_else(|| {
        previous
          .filter(|_| status == ExecutionState::Running)
          .and_then(|record| record.turn_id.clone())
      });
    let record = SessionRecord {
      generation,
      state: SessionState {
        session_id: Some(id.to_owned()),
        status,
        model: if thread.get("model").is_some() {
          mapping::model(thread)
        } else {
          previous.and_then(|record| record.state.model.clone())
        },
        pending_inputs: None,
        revision: turn_id.clone(),
      },
      name: thread
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| previous.and_then(|record| record.name.clone())),
      turn_id,
    };
    snapshot.sessions.insert(id.to_owned(), record.clone());
    Ok(record)
  }
  pub(super) fn publish(&self, session_id: Option<String>, kind: RuntimeEventKind) {
    let mut sequence = self.sequence.lock().unwrap_or_else(|error| error.into_inner());
    *sequence = sequence.checked_add(1).expect("Codex event sequence exhausted");
    let _ = self.events.send(RuntimeEvent {
      runtime_id: self.descriptor.runtime_id.clone(),
      instance_id: self.descriptor.instance_id.clone(),
      session_id,
      sequence: *sequence,
      kind,
    });
  }
  pub(super) fn disconnect(&self, reason: String) {
    if self.disconnected.swap(true, Ordering::AcqRel) {
      return;
    }
    let interactions = std::mem::take(
      &mut self
        .snapshot
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .interactions,
    );
    for (interaction_id, record) in interactions {
      self.publish(
        record.interaction.session_id,
        RuntimeEventKind::InteractionResolved { interaction_id },
      );
    }
    self.publish(None, RuntimeEventKind::Disconnected { reason });
  }
  pub(super) async fn receive(&self, message: Incoming) {
    if self.disconnected.load(Ordering::Acquire) {
      if let Incoming::Response {
        reply,
        result,
        mutation,
        ..
      } = message
      {
        let error = result.err().unwrap_or_else(|| {
          if mutation {
            InteractionError::uncertain("Codex RPC disconnected before admission could be reconciled")
          } else {
            InteractionError::unavailable("Codex RPC disconnected")
          }
        });
        let _ = reply.send(Err(error));
      }
      return;
    }
    match message {
      Incoming::Response {
        method,
        params,
        result,
        reply,
        context,
        ..
      } => {
        let result = result.and_then(|value| self.accept_response(&method, &params, value, context));
        let _ = reply.send(result);
      }
      Incoming::Disconnected { reason } => self.disconnect(reason),
      Incoming::Native(native) => self.publish(None, RuntimeEventKind::Native(native)),
      Incoming::Notification { method, params, native } => {
        self.notification(&method, &params);
        let id = params
          .get("threadId")
          .and_then(Value::as_str)
          .or_else(|| {
            params
              .get("thread")
              .and_then(|thread| thread.get("id"))
              .and_then(Value::as_str)
          })
          .map(str::to_owned);
        self.publish(id, RuntimeEventKind::Native(native));
      }
      Incoming::Request {
        id,
        method,
        params,
        native,
      } => {
        match mapping::interaction(&id, &method, &params, native.clone()) {
          Ok(interaction) => {
            let duplicate = {
              let snapshot = self.snapshot.lock().unwrap_or_else(|error| error.into_inner());
              snapshot
                .interactions
                .get(&interaction.interaction_id)
                .map(|record| record.method == method && record.params == params)
            };
            if duplicate == Some(true) {
              return;
            }
            if duplicate == Some(false) {
              self.disconnect("Codex reused a pending request ID with different content".into());
              let _ = self.transport.close().await;
              return;
            }
            let accepted = {
              let mut snapshot = self.snapshot.lock().unwrap_or_else(|error| error.into_inner());
              if snapshot.interactions.len() >= MAX_INTERACTIONS
                || snapshot.interactions.contains_key(&interaction.interaction_id)
              {
                false
              } else {
                snapshot.interactions.insert(
                  interaction.interaction_id.clone(),
                  PendingRecord {
                    responding: false,
                    id: id.clone(),
                    method,
                    params,
                    interaction: interaction.clone(),
                  },
                );
                true
              }
            };
            if accepted {
              self.publish(
                interaction.session_id.clone(),
                RuntimeEventKind::InteractionRequested(interaction),
              );
            } else {
              let _ = self
                .transport
                .respond_error(&id, -32000, "Codex interaction capacity reached or duplicate request")
                .await;
            }
          }
          Err(error) => {
            let code = if error.kind == ErrorKind::Unsupported {
              -32601
            } else {
              -32602
            };
            let _ = self.transport.respond_error(&id, code, &error.message).await;
          }
        }
        self.publish(params_session(&native), RuntimeEventKind::Native(native));
      }
    }
  }
  pub(super) fn accept_response(
    &self,
    method: &str,
    params: &Value,
    mut value: Value,
    context: Option<u64>,
  ) -> Result<Value, InteractionError> {
    if matches!(method, "thread/start" | "thread/resume" | "thread/read") {
      let parsed: wire::ThreadReadResponse = serde_json::from_value(value.clone()).map_err(|_| {
        if method == "thread/read" {
          InteractionError::unavailable("Malformed Codex thread response")
        } else {
          InteractionError::uncertain("Codex may have loaded a thread, but its response was malformed")
        }
      })?;
      if params
        .get("threadId")
        .and_then(Value::as_str)
        .is_some_and(|id| id != parsed.thread.id)
      {
        return Err(InteractionError::new(
          if method == "thread/read" {
            ErrorKind::StaleTarget
          } else {
            ErrorKind::Uncertain
          },
          "Codex returned an unexpected thread identity",
        ));
      }
      let mut thread = value["thread"].clone();
      for key in ["model", "modelProvider"] {
        if let Some(model) = value.get(key) {
          thread[key] = model.clone();
        }
      }
      self.accept_thread(&thread, context).map_err(|error| {
        if method == "thread/read" {
          error
        } else {
          InteractionError::uncertain(format!(
            "Codex may have loaded a thread, but its state could not be retained: {}",
            error.message
          ))
        }
      })?;
      if method != "thread/read" {
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|error| error.into_inner());
        snapshot.bound.insert(parsed.thread.id);
      }
      value["thread"] = thread;
    }
    if method == "thread/turns/list" {
      let turns: Vec<wire::Turn> = serde_json::from_value(value["data"].clone())
        .map_err(|_| InteractionError::unavailable("Malformed Codex turn page"))?;
      if turns.len() > 1 {
        return Err(InteractionError::unavailable(
          "Codex exceeded the requested turn page bound",
        ));
      }
      let mut snapshot = self.snapshot.lock().unwrap_or_else(|error| error.into_inner());
      if let Some(record) = params["threadId"].as_str().and_then(|id| snapshot.sessions.get_mut(id)) {
        if context.is_some_and(|epoch| record.generation <= epoch) && record.state.status == ExecutionState::Running {
          record.turn_id = turns
            .iter()
            .find(|turn| turn.status == "inProgress")
            .map(|turn| turn.id.clone());
          record.state.revision = record.turn_id.clone();
        }
      }
    }
    Ok(value)
  }
  pub(super) fn notification(&self, method: &str, params: &Value) {
    if method == "thread/started" {
      if let Ok(record) = self.accept_thread(&params["thread"], None) {
        self.publish(record.state.session_id.clone(), RuntimeEventKind::State(record.state));
      }
      return;
    }
    let Some(id) = params.get("threadId").and_then(Value::as_str) else {
      return;
    };
    if matches!(method, "turn/completed" | "thread/closed" | "thread/archived") {
      let resolved = {
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|error| error.into_inner());
        let keys = snapshot
          .interactions
          .iter()
          .filter(|(_, record)| {
            record.interaction.session_id.as_deref() == Some(id)
              && (method != "turn/completed"
                || record.params.get("turnId") == params.get("turn").and_then(|turn| turn.get("id")))
          })
          .map(|(key, _)| key.clone())
          .collect::<Vec<_>>();
        for key in &keys {
          snapshot.interactions.remove(key);
        }
        if method != "turn/completed" {
          snapshot.bound.remove(id);
        }
        keys
      };
      for interaction_id in resolved {
        self.publish(
          Some(id.to_owned()),
          RuntimeEventKind::InteractionResolved { interaction_id },
        );
      }
    }
    if method == "serverRequest/resolved" {
      if let Ok(rpc_id) = serde_json::from_value::<wire::RpcId>(params["requestId"].clone()) {
        let key = mapping::interaction_id(&rpc_id);
        let removed = {
          let mut snapshot = self.snapshot.lock().unwrap_or_else(|error| error.into_inner());
          if snapshot
            .interactions
            .get(&key)
            .is_some_and(|record| record.interaction.session_id.as_deref() == Some(id))
          {
            snapshot.interactions.remove(&key).is_some()
          } else {
            false
          }
        };
        if removed {
          self.publish(
            Some(id.to_owned()),
            RuntimeEventKind::InteractionResolved { interaction_id: key },
          );
        }
      }
      return;
    }
    let state = {
      let mut snapshot = self.snapshot.lock().unwrap_or_else(|error| error.into_inner());
      snapshot.epoch = snapshot.epoch.saturating_add(1);
      let generation = snapshot.epoch;
      let Some(record) = snapshot.sessions.get_mut(id) else {
        return;
      };
      match method {
        "turn/started" => {
          record.state.status = ExecutionState::Running;
          record.turn_id = params["turn"]["id"].as_str().map(str::to_owned);
        }
        "turn/completed" => {
          if record
            .turn_id
            .as_deref()
            .is_some_and(|active| Some(active) != params["turn"]["id"].as_str())
          {
            return;
          }
          record.turn_id = None;
          record.state.status = ExecutionState::Idle;
        }
        "thread/status/changed" => {
          record.state.status = mapping::status(&params["status"]);
          if record.state.status != ExecutionState::Running {
            record.turn_id = None;
          }
        }
        "thread/settings/updated" => {
          record.state.model = mapping::model(&params["threadSettings"]);
        }
        "thread/closed" | "thread/archived" => {
          record.turn_id = None;
          record.state.status = ExecutionState::Stopped;
        }
        _ => return,
      }
      record.generation = generation;
      record.state.revision = record.turn_id.clone();
      record.state.clone()
    };
    self.publish(Some(id.to_owned()), RuntimeEventKind::State(state));
  }
}
