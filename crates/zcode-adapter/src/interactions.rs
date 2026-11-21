//! The native business request ID survives reannouncement; envelope IDs do not.
use super::*;
use std::collections::{BTreeMap, VecDeque};

const MAX_PENDING: usize = 64;
const MAX_CALLBACK_BYTES: usize = 256 * 1024;
const MAX_DETAILS_BYTES: usize = 16 * 1024;
const MAX_ANSWER_CHARS: usize = 32 * 1024;
const MAX_RETIRED: usize = 4096;

#[derive(Default)]
pub(super) struct Interactions {
  revision: u64,
  records: BTreeMap<String, PendingRecord>,
  retired_ids: HashSet<String>,
  retired_order: VecDeque<String>,
}

struct PendingRecord {
  envelope_id: Value,
  fingerprint: [u8; 32],
  revision: u64,
  observed_pending: bool,
  dispatched: bool,
  callback: Callback,
}

struct Callback {
  view: PendingInteraction,
  from_subagent: bool,
  response: ResponseKind,
}

enum ResponseKind {
  Permission {
    allow_once: Option<Value>,
    deny: Option<Value>,
  },
  Questions {
    native_questions: Vec<NativeQuestion>,
    plan: bool,
  },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallbackParams {
  request_id: String,
  session_id: String,
  turn_id: Option<String>,
  tool_call_id: Option<String>,
  tool_name: String,
  reason: Option<String>,
  risk_level: Option<String>,
  #[serde(default)]
  input: Value,
  options: Option<Vec<PermissionOption>>,
  questions: Option<Vec<NativeQuestion>>,
  schema: Option<Value>,
  origin: Option<Value>,
}

#[derive(Deserialize)]
struct PermissionOption {
  kind: String,
  response: Value,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeQuestion {
  question: String,
  header: String,
  options: Vec<NativeOption>,
  #[serde(default)]
  multi_select: bool,
}

#[derive(Clone, Deserialize)]
struct NativeOption {
  label: String,
  value: String,
  description: Option<String>,
  preview: Option<String>,
}

#[derive(Deserialize)]
struct QuestionInput {
  questions: Vec<InputQuestion>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputQuestion {
  question: String,
  header: String,
  options: Vec<InputOption>,
  #[serde(default)]
  multi_select: bool,
}

#[derive(Deserialize)]
struct InputOption {
  label: String,
  description: Option<String>,
  preview: Option<String>,
}

enum Registration {
  New(PendingInteraction),
  Reannounced,
  Declined,
}

impl Inner {
  pub(super) async fn receive_callback(
    &self,
    id: &Value,
    method: &str,
    params: &Value,
  ) -> Result<(), InteractionError> {
    validate_envelope_id(id)?;
    let Some(callback) = Callback::parse(method, params) else {
      return self.transport.send(decline_callback(id, method)?, false).await;
    };
    if !self
      .sessions
      .lock()
      .unwrap()
      .contains(callback.view.session_id.as_deref().unwrap_or_default())
    {
      return self.transport.send(decline_callback(id, method)?, false).await;
    }
    let registration = self
      .interactions
      .lock()
      .unwrap()
      .register(id.clone(), method, params, callback);
    match registration {
      Registration::New(view) => self.publish(view.session_id.clone(), RuntimeEventKind::InteractionRequested(view)),
      Registration::Reannounced => {}
      Registration::Declined => self.transport.send(decline_callback(id, method)?, false).await?,
    }
    Ok(())
  }

  pub(super) fn native_interaction_event(&self, session: &str, params: &Value) {
    let Some(id) = params.pointer("/payload/requestId").and_then(Value::as_str) else {
      return;
    };
    if !identity(id) {
      return;
    }
    match params["type"].as_str() {
      Some("permission.requested" | "userInput.requested") => {
        self.interactions.lock().unwrap().observe_pending(session, id);
      }
      Some("permission.resolved" | "userInput.resolved") => {
        let retired = self.interactions.lock().unwrap().retire(session, Some(id), false);
        self.publish_retired(retired);
      }
      _ => {}
    }
  }

  pub(super) fn retire_interactions(&self, session: &str, root_only: bool) {
    let retired = self.interactions.lock().unwrap().retire(session, None, root_only);
    self.publish_retired(retired);
  }

  fn publish_retired(&self, retired: Vec<(String, String)>) {
    for (session, interaction_id) in retired {
      self.publish(Some(session), RuntimeEventKind::InteractionResolved { interaction_id });
    }
  }

  async fn reconcile_interactions(&self, session: &str) -> Result<(), InteractionError> {
    let cutoff = self.interactions.lock().unwrap().revision;
    let snapshot = self.snapshot(session).await?;
    if let Some(pending) = snapshot.runtime.pending_request_ids {
      let retired = self.interactions.lock().unwrap().reconcile(session, &pending, cutoff);
      self.publish_retired(retired);
    }
    Ok(())
  }

  pub(super) async fn pending_interactions(&self, session: &str) -> Result<Reply, InteractionError> {
    self.reconcile_interactions(session).await?;
    Ok(Reply::Interactions(self.interactions.lock().unwrap().list(session)))
  }

  pub(super) async fn respond_interaction(
    &self,
    session: &str,
    interaction_id: &str,
    response: InteractionResponse,
  ) -> Result<Reply, InteractionError> {
    self.reconcile_interactions(session).await?;
    let (envelope_id, result) =
      self
        .interactions
        .lock()
        .unwrap()
        .prepare_response(session, interaction_id, response)?;
    let result = self
      .transport
      .send(json!({"id":envelope_id,"result":result}), true)
      .await;
    if let Err(error) = result {
      // A definite pre-write failure can remain answerable. An uncertain response
      // never becomes eligible for retransmission under a new envelope ID.
      if error.kind != ErrorKind::Uncertain {
        self
          .interactions
          .lock()
          .unwrap()
          .release_response(session, interaction_id);
      }
      return Err(error);
    }
    // Reverse responses have no ACK. The native resolution or a subsequent
    // authoritative pending snapshot retires the retained dispatched request.
    Ok(Reply::Acknowledged)
  }
}

impl Interactions {
  fn register(&mut self, envelope_id: Value, method: &str, params: &Value, callback: Callback) -> Registration {
    let fingerprint = Sha256::digest(serde_json::to_vec(&(method, params)).expect("native callback JSON")).into();
    let id = callback.view.interaction_id.clone();
    if self.retired_ids.contains(&id)
      || self
        .records
        .iter()
        .any(|(key, record)| key != &id && record.envelope_id == envelope_id)
    {
      return Registration::Declined;
    }
    self.revision = self.revision.saturating_add(1);
    if let Some(record) = self.records.get_mut(&id) {
      if record.fingerprint != fingerprint {
        return Registration::Declined;
      }
      record.envelope_id = envelope_id;
      record.revision = self.revision;
      return Registration::Reannounced;
    }
    if self.records.len() >= MAX_PENDING || self.records.values().any(|record| record.envelope_id == envelope_id) {
      return Registration::Declined;
    }
    let view = callback.view.clone();
    self.records.insert(
      id,
      PendingRecord {
        envelope_id,
        fingerprint,
        revision: self.revision,
        observed_pending: false,
        dispatched: false,
        callback,
      },
    );
    Registration::New(view)
  }

  fn observe_pending(&mut self, session: &str, id: &str) {
    if let Some(record) = self
      .records
      .get_mut(id)
      .filter(|record| record.callback.view.session_id.as_deref() == Some(session))
    {
      record.observed_pending = true;
    }
  }

  fn retire(&mut self, session: &str, id: Option<&str>, root_only: bool) -> Vec<(String, String)> {
    let mut retired = Vec::new();
    self.records.retain(|key, record| {
      if record.callback.view.session_id.as_deref() == Some(session)
        && id.is_none_or(|id| id == key)
        && (!root_only || !record.callback.from_subagent)
      {
        retired.push((session.to_owned(), key.clone()));
        false
      } else {
        true
      }
    });
    self.remember_retired(&retired);
    if let Some(id) = id.filter(|id| !self.records.contains_key(*id)) {
      self.remember_retired(&[(session.to_owned(), id.to_owned())]);
    }
    retired
  }

  fn reconcile(&mut self, session: &str, native_ids: &[String], cutoff: u64) -> Vec<(String, String)> {
    let mut retired = Vec::new();
    self.records.retain(|key, record| {
      if record.callback.view.session_id.as_deref() != Some(session) || record.revision > cutoff {
        return true;
      }
      if native_ids.contains(key) {
        record.observed_pending = true;
        return true;
      }
      // The callback is registered before its PermissionRequested event is
      // published. An initial absent snapshot cannot invalidate that callback.
      if !record.observed_pending && !record.dispatched {
        return true;
      }
      retired.push((session.to_owned(), key.clone()));
      false
    });
    self.remember_retired(&retired);
    retired
  }

  fn remember_retired(&mut self, retired: &[(String, String)]) {
    for (_, id) in retired {
      if !self.retired_ids.insert(id.clone()) {
        continue;
      }
      self.retired_order.push_back(id.clone());
      if self.retired_order.len() > MAX_RETIRED {
        if let Some(oldest) = self.retired_order.pop_front() {
          self.retired_ids.remove(&oldest);
        }
      }
    }
  }

  fn list(&self, session: &str) -> Vec<PendingInteraction> {
    self
      .records
      .values()
      .filter(|record| record.callback.view.session_id.as_deref() == Some(session))
      .map(|record| {
        let mut view = record.callback.view.clone();
        view.native["response_pending"] = json!(record.dispatched);
        view
      })
      .collect()
  }

  fn prepare_response(
    &mut self,
    session: &str,
    id: &str,
    response: InteractionResponse,
  ) -> Result<(Value, Value), InteractionError> {
    let record = self
      .records
      .get_mut(id)
      .filter(|record| record.callback.view.session_id.as_deref() == Some(session))
      .ok_or_else(|| InteractionError::new(ErrorKind::StaleTarget, "ZCode interaction is no longer pending"))?;
    if record.dispatched {
      return Err(InteractionError::new(
        ErrorKind::Conflict,
        "A response was already dispatched to this ZCode interaction",
      ));
    }
    let result = record.callback.respond(response)?;
    record.dispatched = true;
    Ok((record.envelope_id.clone(), result))
  }

  fn release_response(&mut self, session: &str, id: &str) {
    if let Some(record) = self
      .records
      .get_mut(id)
      .filter(|record| record.callback.view.session_id.as_deref() == Some(session))
    {
      record.dispatched = false;
    }
  }
}

impl Callback {
  fn parse(method: &str, params: &Value) -> Option<Self> {
    if serde_json::to_vec(params).ok()?.len() > MAX_CALLBACK_BYTES {
      return None;
    }
    let parsed: CallbackParams = serde_json::from_value(params.clone()).ok()?;
    if !identity(&parsed.request_id)
      || !identity(&parsed.session_id)
      || parsed.turn_id.as_ref().is_some_and(|id| !identity(id))
      || parsed.tool_call_id.as_ref().is_some_and(|id| !identity(id))
      || !display_text(&parsed.tool_name, 256)
    {
      return None;
    }
    let (kind, response, questions, title, details, permission_decisions) = match method {
      "interaction/requestPermission" => {
        if parsed.tool_call_id.is_none()
          || !matches!(
            parsed.risk_level.as_deref(),
            Some("low" | "medium" | "high" | "critical")
          )
        {
          return None;
        }
        let mut allow_once = None;
        let mut deny = None;
        for option in parsed.options? {
          if option.kind == "allow_once" && valid_permission_response(&option.response, "allow") {
            if allow_once.replace(option.response).is_some() {
              return None;
            }
          } else if option.kind == "deny"
            && valid_permission_response(&option.response, "deny")
            && deny.replace(option.response).is_some()
          {
            return None;
          }
        }
        if deny.is_none() {
          return None;
        }
        let mut decisions = Vec::new();
        if allow_once.is_some() {
          decisions.push(PermissionDecision::AllowOnce);
        }
        decisions.push(PermissionDecision::Deny);
        let reason = parsed.reason.as_ref().filter(|reason| display_text(reason, 4096))?;
        let title = format!("{} — {}", parsed.tool_name, reason);
        let details = details(&parsed.input, None);
        (
          InteractionKind::Permission,
          ResponseKind::Permission { allow_once, deny },
          Vec::new(),
          title,
          details,
          decisions,
        )
      }
      "interaction/requestUserInput" => {
        let plan = parsed.tool_name == "ExitPlanMode"
          && parsed.schema.as_ref()?.get("interaction")?.as_str() == Some("plan_approval");
        if parsed.tool_name != "AskUserQuestion" && !plan {
          return None;
        }
        let native_questions = parsed.questions?;
        let questions = question_items(&native_questions, plan)?;
        if plan {
          if native_questions.len() != 1
            || native_questions[0].options.len() != 1
            || native_questions[0].options[0].value != "approve"
            || !parsed
              .input
              .get("plan")
              .and_then(Value::as_str)
              .is_some_and(|plan| !plan.trim().is_empty())
          {
            return None;
          }
        } else if !questions_match_input(&native_questions, &parsed.input) {
          return None;
        }
        let title = if plan {
          "Review implementation plan".into()
        } else {
          "ZCode needs your input".into()
        };
        let details = if plan {
          details(&parsed.input, parsed.input.get("plan").and_then(Value::as_str))
        } else {
          String::new()
        };
        (
          InteractionKind::Question,
          ResponseKind::Questions { native_questions, plan },
          questions,
          title,
          details,
          Vec::new(),
        )
      }
      _ => return None,
    };
    Some(Self {
      from_subagent: parsed.origin.is_some(),
      response,
      view: PendingInteraction {
        interaction_id: parsed.request_id,
        session_id: Some(parsed.session_id),
        kind,
        title,
        questions,
        native: json!({
          "method": method, "params": params, "details": details,
          "permission_decisions": permission_decisions, "response_pending": false
        }),
      },
    })
  }

  fn respond(&self, response: InteractionResponse) -> Result<Value, InteractionError> {
    match (&self.response, response) {
      (
        ResponseKind::Permission { deny, .. },
        InteractionResponse::Reject
        | InteractionResponse::Permission {
          decision: PermissionDecision::Deny,
        },
      ) => deny
        .clone()
        .ok_or_else(|| InteractionError::unsupported("ZCode did not offer denial for this interaction")),
      (
        ResponseKind::Permission { allow_once, .. },
        InteractionResponse::Permission {
          decision: PermissionDecision::AllowOnce,
        },
      ) => allow_once
        .clone()
        .ok_or_else(|| InteractionError::unsupported("ZCode did not offer approval for this interaction")),
      (ResponseKind::Questions { .. }, InteractionResponse::Reject) => Ok(json!({"action":"decline"})),
      (ResponseKind::Questions { native_questions, plan }, InteractionResponse::Answers { answers }) => {
        let mut seen = HashSet::new();
        let mut result = BTreeMap::new();
        let mut annotations = BTreeMap::new();
        if answers.len() != native_questions.len() {
          return Err(InteractionError::invalid("Answer every ZCode question together"));
        }
        for answer in answers {
          let index: usize = answer
            .question_id
            .parse()
            .map_err(|_| InteractionError::invalid("Invalid ZCode question ID"))?;
          let question = native_questions
            .get(index)
            .filter(|_| seen.insert(index))
            .ok_or_else(|| InteractionError::invalid("Unknown or duplicate ZCode question ID"))?;
          if answer.question_id != index.to_string() {
            return Err(InteractionError::invalid("Invalid ZCode question ID"));
          }
          let custom = answer.custom.as_deref().filter(|text| !text.trim().is_empty());
          if answer.custom.is_some() && custom.is_none()
            || custom.is_some_and(|text| !valid_answer(text))
            || !question.multi_select && answer.answers.len() > 1
            || *plan && custom.is_some() && !answer.answers.is_empty()
            || custom.is_none() && answer.answers.is_empty()
            || answer.answers.iter().collect::<HashSet<_>>().len() != answer.answers.len()
          {
            return Err(InteractionError::invalid("Invalid answer for a ZCode question"));
          }
          let selected = answer
            .answers
            .iter()
            .map(|label| {
              question
                .options
                .iter()
                .find(|option| &option.label == label)
                .map(|option| option.value.clone())
                .ok_or_else(|| InteractionError::invalid("Unknown ZCode question option"))
            })
            .collect::<Result<Vec<_>, _>>()?;
          let value = if selected.is_empty() {
            custom.expect("validated custom answer").to_owned()
          } else {
            if let Some(notes) = custom {
              annotations.insert(question.question.as_str(), json!({"notes":notes}));
            }
            selected.join(", ")
          };
          result.insert(question.question.as_str(), value);
        }
        if *plan {
          Ok(json!({"action":"accept","content":{"answer":result.into_values().next().expect("one plan question")}}))
        } else {
          let mut content = json!({"answers":result});
          if !annotations.is_empty() {
            content["annotations"] = json!(annotations);
          }
          Ok(json!({"action":"accept","content":content}))
        }
      }
      _ => Err(InteractionError::unsupported(
        "This response is unavailable for the ZCode interaction",
      )),
    }
  }
}

fn question_items(questions: &[NativeQuestion], plan: bool) -> Option<Vec<QuestionItem>> {
  if questions.is_empty() || questions.len() > 4 {
    return None;
  }
  let mut seen = HashSet::new();
  questions
    .iter()
    .enumerate()
    .map(|(index, question)| {
      if !display_text(&question.question, 8192)
        || !display_text(&question.header, 256)
        || !seen.insert(&question.question)
        || question.options.is_empty()
        || question.options.len() > 4
      {
        return None;
      }
      let mut labels = HashSet::new();
      let options = question
        .options
        .iter()
        .map(|option| {
          if !display_text(&option.label, 2048)
            || !labels.insert(&option.label)
            || (!plan && option.value != option.label)
            || option.description.as_ref().is_some_and(|text| !valid_answer(text))
            || option.preview.as_ref().is_some_and(|text| !valid_answer(text))
          {
            return None;
          }
          let description = [option.description.as_deref(), option.preview.as_deref()]
            .into_iter()
            .flatten()
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
          Some(QuestionOption {
            label: option.label.clone(),
            description: (!description.is_empty()).then(|| bound_details(description)),
          })
        })
        .collect::<Option<Vec<_>>>()?;
      Some(QuestionItem {
        question_id: index.to_string(),
        prompt: question.question.clone(),
        options,
        allows_free_text: true,
        allows_option_notes: !plan,
        is_secret: false,
        multiple: question.multi_select,
      })
    })
    .collect()
}

fn questions_match_input(questions: &[NativeQuestion], input: &Value) -> bool {
  let Ok(input) = serde_json::from_value::<QuestionInput>(input.clone()) else {
    return false;
  };
  input.questions.len() == questions.len()
    && input.questions.iter().zip(questions).all(|(input, question)| {
      input.question == question.question
        && input.header == question.header
        && input.multi_select == question.multi_select
        && input.options.len() == question.options.len()
        && input.options.iter().zip(&question.options).all(|(input, option)| {
          input.label == option.label && input.description == option.description && input.preview == option.preview
        })
    })
}

fn valid_permission_response(value: &Value, decision: &str) -> bool {
  value.as_object().is_some_and(|value| {
    value.get("decision").and_then(Value::as_str) == Some(decision)
      && value.keys().all(|key| matches!(key.as_str(), "decision" | "reason"))
      && value
        .get("reason")
        .is_none_or(|reason| reason.as_str().is_some_and(|reason| display_text(reason, 8192)))
  })
}

fn validate_envelope_id(id: &Value) -> Result<(), InteractionError> {
  if id.as_str().is_some_and(identity) || id.as_u64().is_some() {
    Ok(())
  } else {
    Err(InteractionError::unavailable("ZCode returned an invalid callback ID"))
  }
}

fn identity(value: &str) -> bool {
  !value.is_empty() && value.len() <= 512 && value.trim() == value && !value.chars().any(char::is_control)
}

fn display_text(value: &str, bytes: usize) -> bool {
  !value.trim().is_empty()
    && value.len() <= bytes
    && !value
      .chars()
      .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
}

fn valid_answer(value: &str) -> bool {
  value.chars().count() <= MAX_ANSWER_CHARS
    && !value
      .chars()
      .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
}

fn details(input: &Value, plan: Option<&str>) -> String {
  let text = match plan {
    Some(plan) => {
      let extras = input.as_object().map(|input| {
        input
          .iter()
          .filter(|(key, _)| key.as_str() != "plan")
          .map(|(key, value)| (key.clone(), value.clone()))
          .collect::<serde_json::Map<_, _>>()
      });
      if let Some(extras) = extras.filter(|extras| !extras.is_empty()) {
        format!(
          "{plan}\n\n{}",
          serde_json::to_string_pretty(&extras).unwrap_or_default()
        )
      } else {
        plan.to_owned()
      }
    }
    None => serde_json::to_string_pretty(input).unwrap_or_default(),
  };
  bound_details(text)
}

fn bound_details(text: String) -> String {
  if text.len() <= MAX_DETAILS_BYTES {
    return text;
  }
  let marker = "\n… [truncated]";
  let mut end = MAX_DETAILS_BYTES - marker.len();
  while !text.is_char_boundary(end) {
    end -= 1;
  }
  format!("{}{marker}", &text[..end])
}

#[cfg(test)]
mod tests;
