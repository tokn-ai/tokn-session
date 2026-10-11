//! Read-only question presentation derived from provider-recorded identities.
use crate::model::QuestionSelectionSummary;
use tokn_session_core::{AgentEvent, QuestionReplyEvent};

pub(super) fn reply_text(reply: &QuestionReplyEvent) -> String {
  let rows: Vec<_> = reply
    .replies
    .iter()
    .map(|item| {
      let answers = if item.answers.is_empty() {
        "No answer recorded".into()
      } else {
        item.answers.join("\n\n")
      };
      if reply.replies.len() > 1 {
        format!(
          "{}\n\n{answers}",
          item
            .header
            .as_deref()
            .or(item.question.as_deref())
            .unwrap_or(&item.question_id)
        )
      } else {
        answers
      }
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
    let (Some(id), Some(options)) = (&question.id, &question.options) else {
      continue;
    };
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
      }
    }
    indices.sort_unstable();
    indices.dedup();
    if !indices.is_empty() {
      selections.push(QuestionSelectionSummary {
        question_id: id.clone(),
        option_indices: indices,
      });
    }
  }
  (!selections.is_empty()).then_some(selections)
}
