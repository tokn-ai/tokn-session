//! Desktop/TUI async answers are explicit user-message envelopes. Interpret
//! complete envelopes only; quoted examples and malformed input remain text.

use serde::Deserialize;
use tokn_codex_protocol::{AsyncQuestionReply, RolloutLine};
use tokn_session_core::{AgentEvent, Provider, QuestionReplyEvent, UserQuestionReply};

use super::string_field;

pub(super) fn normalize(line: &RolloutLine, session_id: Option<String>) -> Option<Vec<AgentEvent>> {
  let native = line.native();
  if native["type"] != "event_msg" {
    return None;
  }
  let payload = &native["payload"];
  let text = match payload["type"].as_str()? {
    "user_message" => {
      // Preserve attachment-bearing messages intact rather than silently
      // discarding content while converting the text into an answer card.
      for field in ["images", "local_images", "local_audios"] {
        if payload
          .get(field)
          .is_some_and(|value| !value.is_null() && value.as_array().is_none_or(|items| !items.is_empty()))
        {
          return None;
        }
      }
      string_field(payload, "message")?
    }
    "item_completed" if payload["item"]["type"] == "UserMessage" => {
      string_field(payload, "thread_id")?;
      string_field(payload, "turn_id")?;
      let item = &payload["item"];
      string_field(item, "id")?;
      let mut text = String::new();
      for entry in item["content"].as_array()? {
        match entry["type"].as_str()? {
          "text" => text.push_str(entry["text"].as_str()?),
          "skill" | "mention" if string_field(entry, "name").is_some() && string_field(entry, "path").is_some() => {}
          _ => return None,
        }
      }
      text
    }
    _ => return None,
  };
  let replies = parse(&text)?;
  let mut events: Vec<QuestionReplyEvent> = Vec::new();
  for reply in replies {
    let request_id = request_id(&reply.question_item_id);
    let index = events
      .iter()
      .position(|event| event.request_id == request_id)
      .unwrap_or_else(|| {
        events.push(QuestionReplyEvent {
          provider: Provider::Codex,
          session_id: session_id.clone(),
          request_id,
          turn_id: string_field(payload, "turn_id"),
          replies: Vec::new(),
          native: payload.clone(),
          timestamp: line.timestamp().map(str::to_owned),
        });
        events.len() - 1
      });
    events[index].replies.push(UserQuestionReply {
      question_id: reply.question_item_id,
      question: Some(reply.question),
      header: None,
      answers: vec![reply.answer],
    });
  }
  Some(events.into_iter().map(AgentEvent::QuestionReply).collect())
}

fn request_id(question_id: &str) -> Option<String> {
  let (tool, id, _index): (String, String, u64) = serde_json::from_str(question_id).ok()?;
  (tool == "request_user_input_async" && !id.is_empty()).then_some(id)
}

fn parse(text: &str) -> Option<Vec<AsyncQuestionReply>> {
  #[derive(Deserialize)]
  #[serde(untagged)]
  enum Replies {
    Many(Vec<AsyncQuestionReply>),
    One(AsyncQuestionReply),
  }
  let text = text.trim();
  let text = if text.starts_with("# Context from my IDE setup:\n") {
    text.rsplit_once("\n## My request for Codex:\n")?.1.trim()
  } else {
    text
  };
  let json = text
    .strip_prefix("<send_user_message_question_reply>")?
    .strip_suffix("</send_user_message_question_reply>")?
    .trim()
    // Some desktop text exports retain an object-replacement marker before
    // the serialized payload. Remove only that boundary marker, not answers.
    .trim_start_matches('\u{fffc}')
    .trim();
  let replies = match serde_json::from_str::<Replies>(json).ok()? {
    Replies::Many(replies) => replies,
    Replies::One(reply) => vec![reply],
  };
  (!replies.is_empty() && replies.iter().all(|reply| !reply.question_item_id.trim().is_empty())).then_some(replies)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::normalize::CodexNormalizer;
  use serde_json::{Value, json};

  fn envelope(value: Value) -> String {
    format!("<send_user_message_question_reply>\n{value}\n</send_user_message_question_reply>")
  }

  fn reply(id: &str, answer: &str) -> Value {
    json!({"questionItemId":id,"question":"Which environment?","answer":answer,"future_flag":true})
  }

  #[test]
  fn fixture_answers_match_async_question_ids_without_raw_markup_or_duplicate_tool_results() {
    let mut normalizer = CodexNormalizer::new();
    let events: Vec<_> = include_str!("../../fixtures/async_question_replies.jsonl")
      .lines()
      .flat_map(|line| normalizer.normalize(serde_json::from_str(line).unwrap()))
      .collect();
    let request = events
      .iter()
      .find_map(|event| match event {
        AgentEvent::QuestionRequest(event) => Some(event),
        _ => None,
      })
      .unwrap();
    let replies: Vec<_> = events
      .iter()
      .filter_map(|event| match event {
        AgentEvent::QuestionReply(event) => Some(event),
        _ => None,
      })
      .collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].request_id, request.request_id);
    assert_eq!(replies[0].replies.len(), 2);
    for (question, reply) in request.questions.iter().zip(&replies[0].replies) {
      assert_eq!(question.id.as_deref(), Some(reply.question_id.as_str()));
    }
    assert_eq!(replies[0].replies[1].answers, ["Keep it **local**."]);
    assert!(!events.iter().any(
      |event| matches!(event, AgentEvent::Message(message) if message.text.contains("send_user_message_question_reply"))
    ));
    assert!(
      !events
        .iter()
        .any(|event| matches!(event, AgentEvent::ToolCall(_) | AgentEvent::Unknown(_)))
    );
  }

  #[test]
  fn legacy_and_paginated_answers_preserve_explicit_identity_and_native_payload() {
    let id = r#"["request_user_input_async","question-call",0]"#;
    let text = envelope(json!([reply(id, "Staging"), reply("opaque-id", "Keep it **local**.")]));
    for mode in ["legacy", "paginated"] {
      let payload = if mode == "legacy" {
        json!({"type":"user_message","message":text})
      } else {
        json!({"type":"item_completed","thread_id":"session","turn_id":"turn","item":{"type":"UserMessage","id":"user-1","content":[{"type":"text","text":text}]}})
      };
      let mut normalizer = CodexNormalizer::new();
      normalizer.normalize(
        serde_json::from_value(json!({"type":"session_meta","payload":{"id":"session","history_mode":mode}})).unwrap(),
      );
      let events = normalizer.normalize(
        serde_json::from_value(json!({"type":"event_msg","payload":payload,"timestamp":"recorded"})).unwrap(),
      );
      assert_eq!(events.len(), 2);
      let AgentEvent::QuestionReply(answer) = &events[0] else {
        panic!("expected answer")
      };
      assert_eq!(answer.request_id.as_deref(), Some("question-call"));
      assert_eq!(answer.replies[0].question_id, id);
      assert_eq!(answer.replies[0].answers, ["Staging"]);
      assert_eq!(answer.native, payload);
      assert_eq!(answer.timestamp.as_deref(), Some("recorded"));
      assert!(
        matches!(&events[1], AgentEvent::QuestionReply(answer) if answer.request_id.is_none() && answer.replies[0].answers == ["Keep it **local**."])
      );
    }
  }

  #[test]
  fn single_batched_ide_and_export_marker_envelopes_are_supported() {
    let value = reply(
      "opaque-id",
      "Answer with ￼ and </send_user_message_question_reply> inside JSON",
    );
    for text in [
      envelope(value.clone()),
      envelope(json!([value.clone()])),
      format!(
        "# Context from my IDE setup:\n## Open tabs:\nfile.rs\n## My request for Codex:\n{}",
        envelope(value.clone())
      ),
      format!(
        "<send_user_message_question_reply>￼{}</send_user_message_question_reply>",
        json!([value])
      ),
    ] {
      let parsed = parse(&text).unwrap();
      assert_eq!(parsed.len(), 1);
      assert!(parsed[0].answer.contains('￼'));
    }
    assert_eq!(
      parse(&envelope(json!([reply("one", "Yes"), reply("two", "No")])))
        .unwrap()
        .len(),
      2
    );
  }

  #[test]
  fn malformed_quoted_and_incomplete_envelopes_remain_user_text() {
    let valid = envelope(reply("one", "Yes"));
    for text in [
      format!("Quoted: {valid}"),
      format!("{valid} trailing"),
      "<send_user_message_question_reply>[{".into(),
      envelope(json!([])),
      envelope(json!([reply("one", "Yes"), null])),
      envelope(json!({"questionItemId":"one","question":"Which?","answer":42})),
    ] {
      let mut normalizer = CodexNormalizer::new();
      let events = normalizer.normalize(
        serde_json::from_value(json!({"type":"event_msg","payload":{"type":"user_message","message":text}})).unwrap(),
      );
      assert!(matches!(&events[..], [AgentEvent::Message(message)] if message.text == text));
    }
  }

  #[test]
  fn attachment_bearing_canonical_messages_are_not_partially_converted() {
    let payload = json!({"type":"item_completed","thread_id":"s","turn_id":"t","item":{"type":"UserMessage","id":"u","content":[{"type":"text","text":envelope(reply("one", "Yes"))},{"type":"image","image_url":"image"}]}});
    let line = serde_json::from_value(json!({"type":"event_msg","payload":payload})).unwrap();
    assert!(normalize(&line, None).is_none());
  }
}
