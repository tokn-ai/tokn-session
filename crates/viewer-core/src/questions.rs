//! Question attention is independent of unread replies and running state.
//! Keep bounded identities/positions, never transcript bodies, in projections.
use crate::model::QuestionAttention;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use tokn_session_core::{AgentEvent, LifecycleScope, MessageDelivery, Phase, Role};

#[derive(Clone, Default)]
pub(crate) struct Questions {
  active_turn: Option<String>,
  requests: BTreeMap<String, Request>,
  order: VecDeque<String>,
}

#[derive(Clone)]
struct Request {
  turn_id: Option<String>,
  required: bool,
  source_index: usize,
  unanswered: BTreeSet<String>,
  anonymous_count: usize,
}

impl Request {
  fn count(&self) -> usize {
    self.unanswered.len() + self.anonymous_count
  }
  fn retire(&mut self) {
    self.unanswered.clear();
    self.anonymous_count = 0;
  }
}

impl Questions {
  pub fn observe(&mut self, event: &AgentEvent, source_index: usize) {
    if event.is_hidden() {
      return;
    }
    match event {
      AgentEvent::QuestionRequest(question) if question.phase == Phase::Finished => {
        let Some(id) = question.request_id.as_ref() else {
          return;
        };
        if self.requests.contains_key(id) {
          return;
        }
        self.order.push_back(id.clone());
        self.requests.insert(
          id.clone(),
          Request {
            turn_id: question.turn_id.clone().or_else(|| self.active_turn.clone()),
            required: question.is_blocking == Some(true),
            source_index,
            unanswered: question.questions.iter().filter_map(|item| item.id.clone()).collect(),
            anonymous_count: question.questions.iter().filter(|item| item.id.is_none()).count(),
          },
        );
        while self.order.len() > 256 {
          if let Some(oldest) = self.order.pop_front() {
            self.requests.remove(&oldest);
          }
        }
      }
      AgentEvent::QuestionReply(reply) => {
        for answer in &reply.replies {
          if answer.answers.is_empty() {
            continue;
          }
          if let Some(id) = reply.request_id.as_ref() {
            if let Some(request) = self.requests.get_mut(id) {
              request.unanswered.remove(&answer.question_id);
            }
          } else {
            // An opaque question identity can still resolve an exact recorded
            // identity; prompts and message proximity never establish a link.
            let matches: Vec<_> = self
              .requests
              .iter()
              .filter(|(_, request)| request.unanswered.contains(&answer.question_id))
              .take(2)
              .map(|(id, _)| id.clone())
              .collect();
            if matches.len() == 1 {
              self
                .requests
                .get_mut(&matches[0])
                .unwrap()
                .unanswered
                .remove(&answer.question_id);
            }
          }
        }
      }
      AgentEvent::Lifecycle(turn) if matches!(turn.scope, LifecycleScope::Turn) => {
        if turn.phase == Phase::Started {
          self.retire_except(&turn.turn_id);
          self.active_turn = Some(turn.turn_id.clone());
        } else if turn.phase == Phase::Finished {
          for request in self.requests.values_mut() {
            if request.turn_id.as_ref().is_none_or(|id| id == &turn.turn_id) {
              request.retire();
            }
          }
          if self.active_turn.as_ref() == Some(&turn.turn_id) {
            self.active_turn = None;
          }
        }
      }
      AgentEvent::Message(message)
        if message.role == Role::Assistant
          && message.delivery == MessageDelivery::Final
          && message.phase == Phase::Finished =>
      {
        self.retire()
      }
      AgentEvent::Error(_) => self.retire(),
      _ => {}
    }
  }

  fn retire_except(&mut self, turn: &str) {
    for request in self.requests.values_mut() {
      if request.turn_id.as_deref() != Some(turn) {
        request.retire();
      }
    }
  }

  fn retire(&mut self) {
    for request in self.requests.values_mut() {
      request.retire();
    }
    self.active_turn = None;
  }

  pub fn summary(&self) -> QuestionAttention {
    let mut summary = QuestionAttention::default();
    for request in self.requests.values() {
      let count = request.count() as u64;
      if request.required {
        summary.required_count += count;
      } else {
        summary.available_count += count;
      }
    }
    summary
  }

  pub fn outstanding(&self) -> impl Iterator<Item = (usize, bool, usize)> + '_ {
    self
      .requests
      .values()
      .filter(|request| request.count() > 0)
      .map(|request| (request.source_index, request.required, request.count()))
  }

  pub fn first_index(&self) -> Option<usize> {
    self.outstanding().map(|(index, _, _)| index).min()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;
  use tokn_session_core::{LifecycleEvent, Provider, QuestionReplyEvent, UserQuestionReply};

  fn request(id: &str, blocking: Option<bool>) -> AgentEvent {
    serde_json::from_value(json!({"type":"question_request","provider":"codex","session_id":"s","request_id":id,"turn_id":"t","is_blocking":blocking,"phase":"finished","text":null,"questions":[
      {"id":"q1","header":null,"question":"First?","options":null,"allows_free_text":true,"is_secret":false},
      {"id":"q2","header":null,"question":"Second?","options":null,"allows_free_text":true,"is_secret":false}
    ],"native":{},"timestamp":null})).unwrap()
  }

  fn answer(id: Option<&str>, question: &str, answers: Vec<String>) -> AgentEvent {
    AgentEvent::QuestionReply(QuestionReplyEvent {
      provider: Provider::Codex,
      session_id: None,
      request_id: id.map(str::to_owned),
      turn_id: None,
      replies: vec![UserQuestionReply {
        question_id: question.into(),
        question: None,
        header: None,
        answers,
      }],
      native: json!({}),
      timestamp: None,
    })
  }

  fn turn(id: &str, phase: Phase) -> AgentEvent {
    AgentEvent::Lifecycle(LifecycleEvent {
      provider: Provider::Codex,
      session_id: None,
      turn_id: id.into(),
      scope: LifecycleScope::Turn,
      phase,
      outcome: None,
      step_id: None,
      native: json!({}),
      timestamp: None,
    })
  }

  #[test]
  fn partial_answers_resolve_only_matching_questions_and_duplicate_requests_do_not_reopen_them() {
    let mut state = Questions::default();
    state.observe(&request("r", Some(true)), 10);
    state.observe(&answer(Some("other"), "q1", vec!["Yes".into()]), 11);
    state.observe(&answer(Some("r"), "q1", vec![]), 12);
    assert_eq!(state.summary().required_count, 2);
    state.observe(&answer(Some("r"), "q1", vec!["Yes".into()]), 13);
    state.observe(&request("r", Some(true)), 14);
    assert_eq!(state.summary().required_count, 1);
    assert_eq!(state.first_index(), Some(10));
    state.observe(&answer(None, "q2", vec!["".into()]), 15);
    assert_eq!(state.summary(), QuestionAttention::default());
    assert_eq!(state.first_index(), None);
  }

  #[test]
  fn unknown_and_async_requests_are_available_while_only_explicit_blocking_is_required() {
    let mut state = Questions::default();
    state.observe(&request("async", Some(false)), 1);
    state.observe(&request("unknown", None), 2);
    state.observe(&request("blocking", Some(true)), 3);
    assert_eq!(
      state.summary(),
      QuestionAttention {
        required_count: 2,
        available_count: 4
      }
    );
    state.observe(&turn("other", Phase::Finished), 4);
    assert_eq!(state.summary().required_count, 2);
    state.observe(&turn("t", Phase::Finished), 5);
    assert_eq!(state.summary(), QuestionAttention::default());
  }

  #[test]
  fn opaque_reply_ids_require_a_unique_question_match() {
    let mut state = Questions::default();
    state.observe(&request("r1", None), 1);
    state.observe(&request("r2", None), 2);
    state.observe(&answer(None, "q1", vec!["Yes".into()]), 3);
    assert_eq!(state.summary().available_count, 4);
  }

  #[test]
  fn superseded_turns_and_final_replies_retire_questions_without_inferred_user_answers() {
    let mut state = Questions::default();
    state.observe(&request("r", None), 1);
    state.observe(&turn("new", Phase::Started), 2);
    assert_eq!(state.first_index(), None);
    state.observe(&request("r2", None), 3);
    let mut normalizer = tokn_session_codex::normalize::CodexNormalizer::new();
    let events: Vec<AgentEvent> = include_str!("../tests/fixtures/codex/async_question_replies.jsonl")
      .lines()
      .flat_map(|line| normalizer.normalize(serde_json::from_str(line).unwrap()))
      .collect();
    let final_reply = events
      .iter()
      .find(|event| matches!(event, AgentEvent::Message(message) if message.role == Role::Assistant))
      .unwrap();
    state.observe(final_reply, 4);
    assert_eq!(state.first_index(), None);
  }
}
