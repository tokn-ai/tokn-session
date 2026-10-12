//! Read-only question presentation derived from provider-recorded identities.
use crate::model::QuestionSelectionSummary;
use tokn_session_core::{AgentEvent, QuestionReplyEvent};

pub(super) fn reply_text(events: &[AgentEvent], index: usize, reply: &QuestionReplyEvent) -> String {
  let rows: Vec<_> = reply
    .replies
    .iter()
    .map(|item| {
      let answers = if item.answers.is_empty() {
        "No answer recorded".into()
      } else {
        item.answers.join("\n\n")
      };
      let quoted = answers
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n");
      let candidates: Vec<_> = events
        .iter()
        .take(index)
        .filter(|event| !event.is_hidden())
        .filter_map(|event| {
          let AgentEvent::QuestionRequest(request) = event else {
            return None;
          };
          if reply
            .request_id
            .as_ref()
            .is_some_and(|id| request.request_id.as_ref() != Some(id))
          {
            return None;
          }
          request
            .questions
            .iter()
            .find(|question| question.id.as_ref() == Some(&item.question_id))
        })
        .collect();
      let linked_question = if reply.request_id.is_some() || candidates.len() == 1 {
        candidates.last().map(|question| question.question.as_str())
      } else {
        None
      };
      let prompt = item
        .question
        .as_deref()
        .or(linked_question)
        .or(item.header.as_deref())
        .unwrap_or(&item.question_id);
      format!("{prompt}\n\n{quoted}")
    })
    .collect();
  if rows.is_empty() {
    "No answers recorded".into()
  } else {
    rows.join("\n\n")
  }
}

pub(super) fn selections(
  events: &[AgentEvent],
  index: usize,
  event: &AgentEvent,
) -> Option<Vec<QuestionSelectionSummary>> {
  let AgentEvent::QuestionRequest(request) = event else {
    return None;
  };
  let mut selections = Vec::new();
  for question in &request.questions {
    let Some(id) = &question.id else {
      continue;
    };
    let options = question.options.as_deref().unwrap_or_default();
    let unique_owner = events.iter().filter(|event| matches!(event,
      AgentEvent::QuestionRequest(other) if !event.is_hidden() && other.questions.iter().any(|question| question.id.as_ref() == Some(id))
    )).count() == 1;
    let answers = events
      .iter()
      .skip(index + 1)
      .filter(|event| !event.is_hidden())
      .filter_map(|event| {
        let AgentEvent::QuestionReply(reply) = event else {
          return None;
        };
        let linked = match &reply.request_id {
          Some(reply_id) => request.request_id.as_ref() == Some(reply_id),
          None => unique_owner,
        };
        linked.then_some(reply)
      })
      .flat_map(|reply| &reply.replies)
      .filter(|reply| &reply.question_id == id && !reply.answers.is_empty())
      .last()
      .map(|reply| &reply.answers);
    let Some(answers) = answers else {
      continue;
    };
    let mut indices = Vec::new();
    let mut written = Vec::new();
    for answer in answers {
      let exact: Vec<_> = options
        .iter()
        .enumerate()
        .filter(|(_, option)| option.label == *answer)
        .map(|(index, _)| index)
        .collect();
      // Recommendation suffixes are display metadata. Accept their removal
      // only when it identifies exactly one recorded option.
      let matches = if exact.is_empty() {
        options
          .iter()
          .enumerate()
          .filter(|(_, option)| option.label.strip_suffix(" (Recommended)") == Some(answer.as_str()))
          .map(|(index, _)| index)
          .collect()
      } else {
        exact
      };
      if matches.len() == 1 {
        indices.push(matches[0]);
      } else {
        written.push(answer.as_str());
      }
    }
    indices.sort_unstable();
    indices.dedup();
    let (written_answer, written_answer_truncated) = if written.is_empty() {
      (None, false)
    } else {
      let (text, truncated) = super::truncate_with_flag(written.join("\n\n"), super::MAX_MESSAGE_SUMMARY_CHARS);
      (Some(text), truncated)
    };
    if !indices.is_empty() || written_answer.is_some() {
      selections.push(QuestionSelectionSummary {
        question_id: id.clone(),
        option_indices: indices,
        written_answer,
        written_answer_truncated,
      });
    }
  }
  (!selections.is_empty()).then_some(selections)
}
