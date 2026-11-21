use super::*;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, VecDeque};

const MAX_PENDING: usize = 64;
const MAX_RETIRED: usize = 4096;

#[derive(Default)]
pub(super) struct Permissions {
  records: BTreeMap<String, Record>,
  retired: BTreeSet<String>,
  retirement_order: VecDeque<String>,
}
struct Record {
  session: String,
  tool: String,
  envelope: Value,
  fingerprint: String,
  allow: Option<String>,
  deny: Option<String>,
  public: PendingInteraction,
  dispatched: bool,
}
impl Permissions {
  fn retire(&mut self, key: String) {
    self.records.remove(&key);
    if self.retired.insert(key.clone()) {
      self.retirement_order.push_back(key);
    }
    while self.retirement_order.len() > MAX_RETIRED {
      if let Some(key) = self.retirement_order.pop_front() {
        self.retired.remove(&key);
      }
    }
  }
}
fn business_id(session: &str, tool: &str) -> String {
  format!(
    "qoder-permission-{:x}",
    Sha256::digest(format!("{session}\0{tool}").as_bytes())
  )
}

impl Inner {
  pub(super) async fn receive_request(
    &self,
    envelope: Value,
    method: &str,
    params: Value,
  ) -> Result<(), InteractionError> {
    if !envelope.is_string() && !envelope.is_number() {
      return Err(InteractionError::unavailable("Qoder sent an invalid request identity"));
    }
    if method != "session/request_permission" {
      return self
        .transport
        .send(
          json!({"jsonrpc":"2.0","id":envelope,"error":{"code":-32601,"message":"Client method is not supported"}}),
          false,
        )
        .await;
    }
    let cancelled = json!({"jsonrpc":"2.0","id":envelope,"result":{"outcome":{"outcome":"cancelled"}}});
    let request: acp::PermissionRequest = match serde_json::from_value(params.clone()) {
      Ok(request) => request,
      Err(_) => return self.transport.send(cancelled, false).await,
    };
    let active = self
      .sessions
      .lock()
      .unwrap()
      .get(&request.session_id)
      .is_some_and(|session| session.prompt.is_some() && session.state.status == ExecutionState::Running);
    let tool_name = request
      .tool_call
      .meta
      .as_ref()
      .and_then(|meta| meta.get("qoder"))
      .and_then(|meta| meta.get("toolName"))
      .and_then(Value::as_str);
    // Native plan options change the permission mode; they are not per-use tool approval.
    // Structured elicitation was not negotiated, so neither plan nor question is presented as permission.
    let unsupported_tool = matches!(tool_name, Some("ExitPlanMode" | "AskUserQuestion" | "EnterPlanMode"));
    let valid = active
      && !unsupported_tool
      && !request.tool_call.tool_call_id.is_empty()
      && request.tool_call.tool_call_id.len() <= 1024
      && request.options.len() <= 16
      && serde_json::to_vec(&params).is_ok_and(|bytes| bytes.len() <= 256 * 1024)
      && request
        .options
        .iter()
        .all(|option| !option.option_id.is_empty() && option.option_id.len() <= 1024);
    if !valid {
      return self.transport.send(cancelled, false).await;
    }
    let allow = request
      .options
      .iter()
      .find(|option| option.kind == "allow_once")
      .map(|option| option.option_id.clone());
    let deny = request
      .options
      .iter()
      .find(|option| option.kind == "reject_once")
      .map(|option| option.option_id.clone());
    if allow.is_none() && deny.is_none() {
      return self.transport.send(cancelled, false).await;
    }
    let key = business_id(&request.session_id, &request.tool_call.tool_call_id);
    let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&params).unwrap()));
    let title = bounded(
      request.tool_call.title.as_deref().unwrap_or("Qoder tool permission"),
      4096,
    );
    let details = bounded(&serde_json::to_string_pretty(&json!({
      "tool":tool_name, "kind":request.tool_call.kind, "input":request.tool_call.raw_input, "content":request.tool_call.content
    })).unwrap_or_default(), 16 * 1024);
    let mut decisions = Vec::new();
    if allow.is_some() {
      decisions.push(PermissionDecision::AllowOnce);
    }
    // Deny can always use ACP's cancelled outcome when no explicit reject option exists.
    decisions.push(PermissionDecision::Deny);
    let public = PendingInteraction {
      interaction_id: key.clone(),
      session_id: Some(request.session_id.clone()),
      kind: InteractionKind::Permission,
      title,
      questions: Vec::new(),
      native: json!({"details":details,"permission_decisions":decisions,"response_pending":false}),
    };
    let outcome = {
      let mut permissions = self.permissions.lock().unwrap();
      if permissions.retired.contains(&key) {
        0
      } else if let Some(record) = permissions.records.get_mut(&key) {
        if record.fingerprint == fingerprint && record.envelope == envelope {
          1
        } else {
          0
        }
      } else if permissions.records.len() >= MAX_PENDING {
        0
      } else {
        permissions.records.insert(
          key,
          Record {
            session: request.session_id.clone(),
            tool: request.tool_call.tool_call_id,
            envelope,
            fingerprint,
            allow,
            deny,
            public: public.clone(),
            dispatched: false,
          },
        );
        2
      }
    };
    if outcome == 0 {
      return self.transport.send(cancelled, false).await;
    }
    if outcome == 1 {
      return Ok(());
    }
    self.publish(Some(request.session_id), RuntimeEventKind::InteractionRequested(public));
    Ok(())
  }

  pub(super) fn pending_permissions(&self, session: &str) -> Vec<PendingInteraction> {
    self
      .permissions
      .lock()
      .unwrap()
      .records
      .values()
      .filter(|record| record.session == session)
      .map(|record| {
        let mut public = record.public.clone();
        public.native["response_pending"] = json!(record.dispatched);
        public
      })
      .collect()
  }
  pub(super) async fn respond_permission(
    &self,
    session: &str,
    id: &str,
    response: InteractionResponse,
  ) -> Result<Reply, InteractionError> {
    let frame = {
      let mut permissions = self.permissions.lock().unwrap();
      let record = permissions
        .records
        .get_mut(id)
        .filter(|record| record.session == session)
        .ok_or_else(|| InteractionError::new(ErrorKind::StaleTarget, "Qoder permission is no longer pending"))?;
      if record.dispatched {
        return Err(InteractionError::new(
          ErrorKind::Conflict,
          "A response was already dispatched for this Qoder permission",
        ));
      }
      let option = match response {
        InteractionResponse::Permission {
          decision: PermissionDecision::AllowOnce,
        } => Some(
          record
            .allow
            .clone()
            .ok_or_else(|| InteractionError::invalid("Qoder did not offer an allow-once option"))?,
        ),
        InteractionResponse::Permission {
          decision: PermissionDecision::Deny,
        }
        | InteractionResponse::Reject => record.deny.clone(),
        _ => {
          return Err(InteractionError::unsupported(
            "Qoder supports only the offered allow-once or deny permission response",
          ));
        }
      };
      record.dispatched = true;
      json!({"jsonrpc":"2.0","id":record.envelope,"result":{"outcome": match option {
        Some(option) => json!({"outcome":"selected","optionId":option}),
        None => json!({"outcome":"cancelled"}),
      }}})
    };
    if let Err(error) = self.transport.send(frame, true).await {
      if error.kind != ErrorKind::Uncertain {
        if let Some(record) = self.permissions.lock().unwrap().records.get_mut(id) {
          record.dispatched = false;
        }
      }
      return Err(error);
    }
    // A pipe write is not native resolution. Retain disabled pending until a tool/turn terminal event.
    Ok(Reply::Acknowledged)
  }
  pub(super) fn observe_permission_update(&self, session: &str, update: &Value) {
    if matches!(
      update.get("status").and_then(Value::as_str),
      Some("completed" | "failed" | "cancelled")
    ) {
      if let Some(tool) = update.get("toolCallId").and_then(Value::as_str) {
        self.retire_permissions(session, Some(tool));
      }
    }
  }
  pub(super) fn retire_permissions(&self, session: &str, tool: Option<&str>) {
    let mut permissions = self.permissions.lock().unwrap();
    let ids: Vec<_> = permissions
      .records
      .iter()
      .filter(|(_, record)| record.session == session && tool.is_none_or(|tool| record.tool == tool))
      .map(|(id, _)| id.clone())
      .collect();
    for id in &ids {
      permissions.retire(id.clone());
    }
    drop(permissions);
    for id in ids {
      self.publish(
        Some(session.into()),
        RuntimeEventKind::InteractionResolved { interaction_id: id },
      );
    }
  }
  pub(super) async fn cancel_permissions(&self, session: &str) -> Result<(), InteractionError> {
    let frames: Vec<_> = {
      let mut permissions = self.permissions.lock().unwrap();
      permissions
        .records
        .values_mut()
        .filter(|record| record.session == session && !record.dispatched)
        .map(|record| {
          record.dispatched = true;
          json!({"jsonrpc":"2.0","id":record.envelope,"result":{"outcome":{"outcome":"cancelled"}}})
        })
        .collect()
    };
    for frame in frames {
      self.transport.send(frame, true).await?;
    }
    self.retire_permissions(session, None);
    Ok(())
  }
}
fn bounded(value: &str, maximum: usize) -> String {
  let mut end = value.len().min(maximum);
  while !value.is_char_boundary(end) {
    end -= 1;
  }
  value[..end].to_owned()
}
