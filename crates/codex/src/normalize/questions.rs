use serde_json::Value;
use tokn_codex_protocol::{AsyncUserInputQuestion, FunctionCallItem, RequestUserInputEvent};
use tokn_session_core::{AgentEvent, Phase, Provider, QuestionRequestEvent, UserQuestion, UserQuestionOption};

use super::string_field;

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
  let questions = async_questions(item.get("questions")?)?;
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

fn async_questions(value: &Value) -> Option<Vec<UserQuestion>> {
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
      .map(|question| UserQuestion {
        id: None,
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
      questions: async_questions(input.get("questions")?)?,
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
