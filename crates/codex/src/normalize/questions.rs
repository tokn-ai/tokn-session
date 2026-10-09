use serde_json::Value;
use tokn_codex_protocol::{AsyncUserInputQuestion, FunctionCallItem, RequestUserInputEvent};
use tokn_session_core::{AgentEvent, Phase, Provider, QuestionRequestEvent, UserQuestion, UserQuestionOption};

use super::string_field;

/// Bounded correlation retained across incremental reads. Never identify a
/// reply from answer-shaped JSON alone: it must belong to a recorded request
/// or a canonical output explicitly named request_user_input.
#[derive(Default)]
pub(super) struct RepliesNormalizer {
  requests: std::collections::BTreeMap<String, ReplyContext>,
  order: std::collections::VecDeque<String>,
  outputs: std::collections::BTreeMap<String, Value>,
}

struct ReplyContext {
  turn_id: Option<String>,
  questions: Vec<UserQuestion>,
}

impl RepliesNormalizer {
  pub(super) fn observe(&mut self, events: &[AgentEvent]) {
    for event in events {
      let AgentEvent::QuestionRequest(request) = event else {
        continue;
      };
      // Async acceptance is not a user reply. Its answers arrive separately
      // through user-message envelopes, not function outputs.
      if request.native["item"]["delivery"] == "async"
        || request.native["name"]
          .as_str()
          .is_some_and(|name| name.rsplit('.').next() == Some("request_user_input_async"))
        || request.questions.iter().all(|question| question.id.is_none())
      {
        continue;
      }
      let Some(id) = request.request_id.as_ref() else {
        continue;
      };
      if !self.requests.contains_key(id) {
        self.order.push_back(id.clone());
      }
      self.requests.insert(
        id.clone(),
        ReplyContext {
          turn_id: request.turn_id.clone(),
          questions: request.questions.clone(),
        },
      );
      self.evict();
    }
  }

  fn evict(&mut self) {
    while self.order.len() > 256 {
      if let Some(oldest) = self.order.pop_front() {
        self.requests.remove(&oldest);
        self.outputs.remove(&oldest);
      }
    }
  }

  pub(super) fn output(
    &mut self,
    line: &tokn_codex_protocol::RolloutLine,
    session_id: Option<String>,
  ) -> Option<Vec<AgentEvent>> {
    let payload = line.native().get("payload")?;
    let canonical = line.native()["type"] == "event_msg"
      && payload["type"] == "item_completed"
      && payload["item"]["type"] == "FunctionCallOutput";
    let (id, output) = if canonical {
      let item = &payload["item"];
      if item["name"].as_str()?.rsplit('.').next()? != "request_user_input" {
        return None;
      }
      (string_field(item, "id")?, &item["output"])
    } else if line.native()["type"] == "response_item" && payload["type"] == "function_call_output" {
      let id = string_field(payload, "call_id")?;
      if !self.requests.contains_key(&id) {
        return None;
      }
      (id, &payload["output"])
    } else {
      return None;
    };
    let decoded = reply_value(output);
    let response = decoded
      .as_ref()
      .and_then(|value| serde_json::from_value::<tokn_codex_protocol::RequestUserInputResponse>(value.clone()).ok());
    let Some(response) = response.filter(|response| response.answers.keys().all(|id| !id.trim().is_empty())) else {
      return Some(vec![super::unknown_event(
        session_id,
        Some(
          if canonical {
            "event_msg.item_completed.FunctionCallOutput"
          } else {
            "response_item.function_call_output"
          }
          .into(),
        ),
        Some(payload.clone()),
        line.timestamp().map(str::to_owned),
      )]);
    };
    let decoded = decoded.unwrap();
    if self.outputs.get(&id) == Some(&decoded) {
      return Some(Vec::new());
    }
    // Orphan canonical outputs remain readable without invented question text.
    // Cache their identity too so a repeated output cannot duplicate the reply.
    if !self.requests.contains_key(&id) {
      self.order.push_back(id.clone());
      self.requests.insert(
        id.clone(),
        ReplyContext {
          turn_id: string_field(payload, "turn_id"),
          questions: Vec::new(),
        },
      );
      self.evict();
    }
    self.outputs.insert(id.clone(), decoded);
    let request = self.requests.get(&id);
    let mut answers = response.answers;
    let mut replies = Vec::new();
    if let Some(request) = request {
      for question in &request.questions {
        let Some(question_id) = question.id.as_ref() else {
          continue;
        };
        if let Some(answer) = answers.remove(question_id) {
          replies.push(tokn_session_core::UserQuestionReply {
            question_id: question_id.clone(),
            question: Some(question.question.clone()),
            header: question.header.clone(),
            answers: answer.answers,
          });
        }
      }
    }
    // Unexpected question IDs stay visible rather than silently disappearing.
    replies.extend(
      answers
        .into_iter()
        .map(|(question_id, answer)| tokn_session_core::UserQuestionReply {
          question_id,
          question: None,
          header: None,
          answers: answer.answers,
        }),
    );
    Some(vec![AgentEvent::QuestionReply(tokn_session_core::QuestionReplyEvent {
      provider: Provider::Codex,
      session_id,
      request_id: Some(id),
      turn_id: string_field(payload, "turn_id").or_else(|| request.and_then(|request| request.turn_id.clone())),
      replies,
      native: payload.clone(),
      timestamp: line.timestamp().map(str::to_owned),
    })])
  }
}

fn reply_value(output: &Value) -> Option<Value> {
  if let Some(text) = output.as_str() {
    return serde_json::from_str(text).ok();
  }
  if let Some(parts) = output.as_array() {
    let mut text = String::new();
    for part in parts {
      if part["type"] != "input_text" {
        return None;
      }
      text.push_str(part.get("text")?.as_str()?);
    }
    return serde_json::from_str(&text).ok();
  }
  output.is_object().then(|| output.clone())
}

pub(super) fn async_message(
  session_id: Option<String>,
  item: &Value,
  payload: &Value,
  text: String,
  phase: Phase,
  timestamp: Option<String>,
) -> Option<AgentEvent> {
  if item.get("delivery")?.as_str()? != "async" {
    return None;
  }
  let questions = async_questions(item.get("questions")?, item["id"].as_str())?;
  Some(AgentEvent::QuestionRequest(QuestionRequestEvent {
    provider: Provider::Codex,
    session_id,
    request_id: string_field(item, "id"),
    turn_id: string_field(payload, "turn_id"),
    is_blocking: Some(false),
    phase,
    text: (!text.is_empty()).then_some(text),
    questions,
    native: payload.clone(),
    timestamp,
  }))
}

fn async_questions(value: &Value, request_id: Option<&str>) -> Option<Vec<UserQuestion>> {
  let questions: Vec<AsyncUserInputQuestion> = serde_json::from_value(value.clone()).ok()?;
  if questions.is_empty()
    || questions.iter().any(|question| {
      question.title.trim().is_empty()
        || question
          .options
          .as_ref()
          .is_some_and(|options| options.is_empty() || options.iter().any(|label| label.trim().is_empty()))
    })
  {
    return None;
  }
  Some(
    questions
      .into_iter()
      .enumerate()
      .map(|(index, question)| UserQuestion {
        id: request_id.map(|id| serde_json::json!(["request_user_input_async", id, index]).to_string()),
        header: None,
        question: question.title,
        options: question.options.map(|options| {
          options
            .into_iter()
            .map(|label| UserQuestionOption {
              label,
              description: None,
            })
            .collect()
        }),
        allows_free_text: true,
        is_secret: false,
      })
      .collect(),
  )
}

/// Legacy rollouts persist tool calls instead of canonical question items.
pub(super) fn function_call(
  session_id: Option<String>,
  item: &FunctionCallItem,
  timestamp: Option<String>,
) -> Option<AgentEvent> {
  let name = item.name.as_deref()?.rsplit('.').next()?;
  if !matches!(name, "request_user_input" | "request_user_input_async") {
    return None;
  }
  let input = super::parse_json_string_or_value(item.arguments.clone());
  let native = super::json_value(item);
  if name == "request_user_input_async" {
    return Some(AgentEvent::QuestionRequest(QuestionRequestEvent {
      provider: Provider::Codex,
      session_id,
      request_id: item.call_id.clone(),
      turn_id: None,
      is_blocking: Some(false),
      phase: Phase::Finished,
      text: None,
      questions: async_questions(input.get("questions")?, item.call_id.as_deref())?,
      native,
      timestamp,
    }));
  }
  let mut payload = input;
  payload
    .as_object_mut()?
    .insert("call_id".into(), Value::String(item.call_id.clone()?));
  let AgentEvent::QuestionRequest(mut event) = request(session_id, &payload, timestamp)? else {
    return None;
  };
  event.native = native;
  // The handler derives blocking from the effective collaboration mode, not
  // model-authored arguments, and always adds the free-form Other choice.
  event.is_blocking = None;
  for question in &mut event.questions {
    if question.options.as_ref().is_none_or(Vec::is_empty) {
      return None;
    }
    question.allows_free_text = true;
  }
  Some(AgentEvent::QuestionRequest(event))
}

pub(super) fn request(session_id: Option<String>, payload: &Value, timestamp: Option<String>) -> Option<AgentEvent> {
  let request: RequestUserInputEvent = serde_json::from_value(payload.clone()).ok()?;
  let mut ids = std::collections::HashSet::new();
  if request.call_id.trim().is_empty()
    || request.questions.is_empty()
    || request.questions.iter().any(|question| {
      question.id.trim().is_empty()
        || !ids.insert(&question.id)
        || question.question.trim().is_empty()
        || question
          .options
          .as_ref()
          .is_some_and(|options| options.iter().any(|option| option.label.trim().is_empty()))
    })
  {
    return None;
  }
  Some(AgentEvent::QuestionRequest(QuestionRequestEvent {
    provider: Provider::Codex,
    session_id,
    request_id: Some(request.call_id),
    turn_id: request.turn_id,
    is_blocking: Some(request.is_blocking.unwrap_or(true)),
    phase: Phase::Finished,
    text: None,
    questions: request
      .questions
      .into_iter()
      .map(|question| {
        let allows_free_text = question.is_other || question.options.as_ref().is_none_or(Vec::is_empty);
        UserQuestion {
          id: Some(question.id),
          header: Some(question.header),
          question: question.question,
          options: question.options.map(|options| {
            options
              .into_iter()
              .map(|option| UserQuestionOption {
                label: option.label,
                description: Some(option.description),
              })
              .collect()
          }),
          allows_free_text,
          is_secret: question.is_secret,
        }
      })
      .collect(),
    native: payload.clone(),
    timestamp,
  }))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::normalize::CodexNormalizer;
  use serde_json::json;

  fn normalize(values: impl IntoIterator<Item = Value>) -> Vec<AgentEvent> {
    let mut normalizer = CodexNormalizer::new();
    values
      .into_iter()
      .flat_map(|value| normalizer.normalize(serde_json::from_value(value).unwrap()))
      .collect()
  }

  fn structured_call(id: &str) -> Value {
    json!({"type":"response_item","payload":{"type":"function_call","name":"request_user_input","call_id":id,"arguments":json!({"questions":[
      {"id":"storage","header":"Storage","question":"Which storage?","options":[{"label":"SQLite","description":"Local"}]},
      {"id":"notes","header":"Notes","question":"Any constraints?","options":[{"label":"Local","description":"No cloud"}]}
    ]}).to_string()}})
  }

  fn structured_output(id: &str, output: Value) -> Value {
    json!({"timestamp":"2026-10-09T00:00:05Z","type":"response_item","payload":{"type":"function_call_output","call_id":id,"output":output,"future_hint":"kept"}})
  }

  #[test]
  fn replies_correlate_by_call_and_question_ids_in_legacy_and_paginated_history() {
    for mode in ["legacy", "paginated"] {
      let answers = json!({"answers":{"notes":{"answers":["No cloud", "**Keep it local**"]},"storage":{"answers":["SQLite"]},"future_question":{"answers":["Still visible"]}},"future_reply_flag":true});
      let raw = structured_output("call-1", Value::String(answers.to_string()));
      let canonical = json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"session-1","turn_id":"turn-1","item":{"type":"FunctionCallOutput","name":"request_user_input","id":"call-1","output":answers.to_string()}}});
      let events = normalize([
        json!({"type":"session_meta","payload":{"id":"session-1","history_mode":mode}}),
        structured_call("call-1"),
        raw.clone(),
        canonical,
      ]);
      let replies: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
          AgentEvent::QuestionReply(reply) => Some(reply),
          _ => None,
        })
        .collect();
      assert_eq!(replies.len(), 1);
      let reply = replies[0];
      assert_eq!(reply.request_id.as_deref(), Some("call-1"));
      assert_eq!(reply.session_id.as_deref(), Some("session-1"));
      assert_eq!(reply.replies[0].question_id, "storage");
      assert_eq!(reply.replies[0].question.as_deref(), Some("Which storage?"));
      assert_eq!(reply.replies[0].answers, ["SQLite"]);
      assert_eq!(reply.replies[1].answers, ["No cloud", "**Keep it local**"]);
      assert!(reply.replies[2].question.is_none());
      assert_eq!(reply.native, raw["payload"]);
      assert_eq!(reply.timestamp.as_deref(), Some("2026-10-09T00:00:05Z"));
      assert!(!events.iter().any(|event| matches!(event, AgentEvent::ToolCall(_))));
    }
  }

  #[test]
  fn answers_work_in_incremental_reads_and_accept_content_items_without_dropping_media() {
    let mut normalizer = CodexNormalizer::new();
    normalizer.normalize(serde_json::from_value(structured_call("call-1")).unwrap());
    let answers = json!({"answers":{"storage":{"answers":["SQLite"]}}});
    let output = structured_output("call-1", json!([{"type":"input_text","text":answers.to_string()}]));
    let events = normalizer.normalize(serde_json::from_value(output).unwrap());
    assert!(matches!(&events[..], [AgentEvent::QuestionReply(reply)] if reply.replies[0].answers == ["SQLite"]));
    let output = structured_output(
      "call-1",
      json!([{"type":"input_text","text":answers.to_string()},{"type":"input_image","image_url":"test"}]),
    );
    let events = normalizer.normalize(serde_json::from_value(output).unwrap());
    assert!(matches!(&events[..], [AgentEvent::Unknown(_)]));
  }

  #[test]
  fn malformed_empty_and_unrelated_outputs_do_not_fabricate_answers() {
    for (output, valid) in [
      (json!({"answers":{}}), true),
      (json!({"answers":{"storage":{"answers":[]}}}), true),
      (json!({"answers":{"storage":{"answers":[42]}}}), false),
      (json!("cancelled"), false),
      (json!({"accepted":true}), false),
      (Value::Null, false),
    ] {
      let events = normalize([structured_call("call-1"), structured_output("call-1", output)]);
      if valid {
        assert!(matches!(&events[1], AgentEvent::QuestionReply(_)));
      } else {
        assert!(matches!(&events[1], AgentEvent::Unknown(_)));
      }
    }
    let events = normalize([
      structured_call("call-1"),
      json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"call-1"}}),
    ]);
    assert!(matches!(&events[1], AgentEvent::Unknown(_)));
    let events = normalize([structured_output(
      "unrelated",
      json!({"answers":{"storage":{"answers":["SQLite"]}}}),
    )]);
    assert!(matches!(&events[..], [AgentEvent::ToolCall(_)]));
    let async_call = json!({"type":"response_item","payload":{"type":"function_call","name":"request_user_input_async","call_id":"async-1","arguments":"{\"questions\":[{\"title\":\"Storage?\"}]}"}});
    let events = normalize([async_call, structured_output("async-1", json!("{\"accepted\":true}"))]);
    assert!(!events.iter().any(|event| matches!(event, AgentEvent::QuestionReply(_))));
  }

  #[test]
  fn orphan_canonical_answers_preserve_ids_without_inventing_question_text() {
    let event = json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"session-1","turn_id":"turn-1","item":{"type":"FunctionCallOutput","id":"call-1","name":"request_user_input","output":"{\"answers\":{\"q1\":{\"answers\":[\"Yes\"]}}}"}}});
    let events = normalize([event]);
    assert!(
      matches!(&events[..], [AgentEvent::QuestionReply(reply)] if reply.turn_id.as_deref() == Some("turn-1") && reply.replies[0].question.is_none() && reply.replies[0].question_id == "q1")
    );
  }

  #[test]
  fn request_correlation_is_bounded() {
    let mut normalizer = RepliesNormalizer::default();
    for index in 0..300 {
      let events = normalize([structured_call(&format!("call-{index}"))]);
      normalizer.observe(&events);
    }
    assert_eq!(normalizer.requests.len(), 256);
    assert_eq!(normalizer.order.len(), 256);
    assert!(!normalizer.requests.contains_key("call-0"));
  }

  #[test]
  fn paginated_history_preserves_choices_without_duplicate_questions_or_false_final_reply() {
    let events = normalize(
      include_str!("../../fixtures/questions.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap()),
    );
    let requests: Vec<_> = events
      .iter()
      .filter_map(|event| match event {
        AgentEvent::QuestionRequest(request) => Some(request),
        _ => None,
      })
      .collect();
    assert_eq!(requests.len(), 1);
    let request = requests[0];
    assert_eq!(request.is_blocking, Some(false));
    assert_eq!(request.request_id.as_deref(), Some("question-1"));
    assert_eq!(request.turn_id.as_deref(), Some("turn-1"));
    assert_eq!(request.questions.len(), 2);
    assert_eq!(request.questions[0].options.as_ref().unwrap()[0].label, "SQLite");
    assert!(request.questions[1].options.is_none());
    assert_eq!(request.native["item"]["questions"][1]["future_hint"], "preserved");
    assert!(
      events
        .iter()
        .any(|event| matches!(event, AgentEvent::Message(message) if message.text == "SQLite; keep everything local."))
    );
    assert_eq!(
      events
        .iter()
        .filter(
          |event| matches!(event, AgentEvent::Message(message) if message.role == tokn_session_core::Role::Assistant)
        )
        .count(),
      1
    );
  }

  #[test]
  fn legacy_history_can_display_unpaired_async_message_items() {
    let mut value: Value =
      serde_json::from_str(include_str!("../../fixtures/questions.jsonl").lines().nth(4).unwrap()).unwrap();
    value["payload"]["item"]["content"] = json!([]);
    let events = normalize([value]);
    assert!(matches!(&events[..], [AgentEvent::QuestionRequest(request)] if request.text.is_none()));
  }

  #[test]
  fn malformed_question_metadata_stays_native_unknown_instead_of_becoming_partial_text() {
    let base: Value =
      serde_json::from_str(include_str!("../../fixtures/questions.jsonl").lines().nth(4).unwrap()).unwrap();
    for malformed in [
      json!([]),
      json!([{"title":"","options":null}]),
      json!([{"title":"Valid?","options":[42]}]),
      json!([{"title":"Valid?","options":[]}]),
      json!([{"title":"Valid?","options":[" "]}]),
    ] {
      let mut value = base.clone();
      value["payload"]["item"]["questions"] = malformed.clone();
      let events = normalize([value]);
      assert!(
        matches!(&events[..], [AgentEvent::Unknown(unknown)] if unknown.native.as_ref().unwrap()["item"]["questions"] == malformed)
      );
    }
    let mut value = base;
    value["payload"]["item"]["delivery"] = json!("future_delivery");
    assert!(matches!(&normalize([value])[..], [AgentEvent::Unknown(_)]));
  }

  #[test]
  fn blocking_events_accept_historical_defaults_and_current_flags() {
    let mut payload = json!({"type":"request_user_input","call_id":"call-1","questions":[{"id":"storage","header":"Storage","question":"Choose storage","isOther":true,"isSecret":true,"options":[{"label":"SQLite","description":"Local"}]}]});
    let AgentEvent::QuestionRequest(event) = request(None, &payload, None).unwrap() else {
      panic!()
    };
    assert_eq!(event.is_blocking, Some(true));
    assert!(event.questions[0].allows_free_text);
    assert!(event.questions[0].is_secret);
    assert_eq!(
      event.questions[0].options.as_ref().unwrap()[0].description.as_deref(),
      Some("Local")
    );
    payload["isBlocking"] = json!(false);
    payload["autoResolutionMs"] = json!(100);
    let AgentEvent::QuestionRequest(event) = request(None, &payload, None).unwrap() else {
      panic!()
    };
    assert_eq!(event.is_blocking, Some(false));
    payload["questions"][0]["options"] = json!(false);
    let events = normalize([json!({"type":"event_msg","payload":payload})]);
    assert!(matches!(&events[..], [AgentEvent::Unknown(_)]));
  }

  #[test]
  fn legacy_function_calls_preserve_async_choices_and_blocking_descriptions() {
    for (name, arguments, blocking) in [
      (
        "functions.request_user_input_async",
        json!({"questions":[{"title":"Choose storage","options":["SQLite"]}]}),
        Some(false),
      ),
      (
        "request_user_input",
        json!({"questions":[{"id":"storage","header":"Storage","question":"Choose storage","options":[{"label":"SQLite","description":"Local"}]}]}),
        None,
      ),
    ] {
      let native = json!({"type":"function_call","name":name,"call_id":"call-1","arguments":arguments.to_string(),"future_hint":{"keep":true}});
      let events = normalize([json!({"type":"response_item","payload":native})]);
      let [AgentEvent::QuestionRequest(event)] = &events[..] else {
        panic!("expected question request");
      };
      assert_eq!(event.is_blocking, blocking);
      for field in ["type", "name", "call_id", "arguments", "future_hint"] {
        assert_eq!(event.native[field], native[field]);
      }
      assert_eq!(event.questions[0].options.as_ref().unwrap()[0].label, "SQLite");
    }
  }

  #[test]
  fn paginated_history_retains_blocking_calls_and_malformed_requests() {
    let meta = json!({"type":"session_meta","payload":{"id":"session-1","history_mode":"paginated"}});
    for arguments in [
      json!({"questions":[{"id":"storage","header":"Storage","question":"Choose storage","options":[{"label":"SQLite","description":"Local"}]}]}),
      json!({"questions":[42]}),
    ] {
      let valid = arguments["questions"][0].is_object();
      let call = json!({"type":"response_item","payload":{"type":"function_call","name":"request_user_input","call_id":"call-1","arguments":arguments.to_string()}});
      let events = normalize([meta.clone(), call]);
      if valid {
        assert!(matches!(&events[1], AgentEvent::QuestionRequest(request) if request.questions[0].allows_free_text));
      } else {
        assert!(matches!(&events[1], AgentEvent::Unknown(_)));
      }
    }
  }
}
