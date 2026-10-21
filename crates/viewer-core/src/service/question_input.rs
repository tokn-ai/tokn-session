//! Host-authoritative asynchronous question answers. Recorded questions alone
//! never authorize delivery; current pending IDs and owning input target do.
use super::*;
use crate::input::{self, InputTarget};
use crate::model::{
  InputDeliveryStatus, SessionQuestionStatus, SessionQuestionStatusRequest, SubmitSessionInputRequest,
  SubmitSessionInputResponse, SubmitSessionQuestionAnswersRequest,
};
use tokn_session_core::QuestionRequestEvent;

const MAX_CLAIMS: usize = 1024;

#[derive(Default)]
pub(super) struct QuestionClaims {
  owners: HashMap<(String, String, String), String>,
}

impl QuestionClaims {
  fn prune(&mut self, session: &str, pending: &HashSet<(String, String)>) {
    self.owners.retain(|(owner, request, question), _| {
      owner != session || pending.contains(&(request.clone(), question.clone()))
    });
  }
  fn claim(
    &mut self,
    session: &str,
    request: &str,
    ids: &[String],
    attempt: &str,
  ) -> Result<Vec<(String, String, String)>, String> {
    let keys: Vec<_> = ids
      .iter()
      .map(|id| (session.to_string(), request.to_string(), id.clone()))
      .collect();
    if keys
      .iter()
      .any(|key| self.owners.get(key).is_some_and(|owner| owner != attempt))
    {
      return Err("An answer has already been submitted for this question. Wait for its recorded reply.".into());
    }
    // Existing claims never reopen delivery, even if the generic receipt
    // cache has evicted this attempt. An attempt cannot expand its question set.
    if keys.iter().any(|key| self.owners.contains_key(key)) {
      return Ok(Vec::new());
    }
    let new: Vec<_> = keys.into_iter().filter(|key| !self.owners.contains_key(key)).collect();
    if self.owners.len() + new.len() > MAX_CLAIMS {
      return Err("Too many answers are awaiting confirmation.".into());
    }
    for key in &new {
      self.owners.insert(key.clone(), attempt.into());
    }
    Ok(new)
  }
  fn release(&mut self, keys: &[(String, String, String)], attempt: &str) {
    for key in keys {
      if self.owners.get(key).is_some_and(|owner| owner == attempt) {
        self.owners.remove(key);
      }
    }
  }
}

struct PendingRequest {
  target: InputTarget,
  request: QuestionRequestEvent,
  pending: Vec<String>,
}

impl ViewerService {
  fn pending_question_input(&self, request: &SessionQuestionStatusRequest) -> Result<PendingRequest, String> {
    if self.session_scope.is_some() {
      return Err("This session share is read-only.".into());
    }
    let target = self.resolve_input_target(&request.session_key)?;
    if target.provider != ViewerProvider::Codex {
      return Err("Interactive questions are currently available for Codex only.".into());
    }
    let locator = decode_session_key(&request.session_key)?;
    let loaded = self.load_verified(&locator)?;
    let mut questions = crate::questions::Questions::default();
    for event in &loaded.events {
      questions.observe(event, 0);
    }
    let requests: Vec<_> = loaded
      .events
      .iter()
      .filter_map(|event| match event {
        AgentEvent::QuestionRequest(question)
          if !event.is_hidden() && question.request_id.as_ref() == Some(&request.question_request_id) =>
        {
          Some(question)
        }
        _ => None,
      })
      .collect();
    let question = requests
      .last()
      .ok_or("Question request is no longer available. Refresh the conversation.")?;
    if question.is_blocking != Some(false) {
      return Err(
        "Answer blocking questions in Codex Desktop. Live blocking-request discovery is not supported yet.".into(),
      );
    }
    // Async identities are opaque provider IDs, not prompt text or client data.
    for (index, item) in question.questions.iter().enumerate() {
      let expected = json!(["request_user_input_async", request.question_request_id, index]);
      if item.id.as_deref().and_then(|id| serde_json::from_str::<Value>(id).ok()) != Some(expected) {
        return Err("This recorded question cannot be answered through the async interface.".into());
      }
    }
    let pending = questions.pending_ids(&request.question_request_id);
    let all_pending: HashSet<_> = loaded
      .events
      .iter()
      .filter_map(|event| match event {
        AgentEvent::QuestionRequest(request) => request.request_id.as_ref(),
        _ => None,
      })
      .flat_map(|id| {
        questions
          .pending_ids(id)
          .into_iter()
          .map(move |question| (id.clone(), question))
      })
      .collect();
    self
      .question_claims
      .lock()
      .map_err(|_| "Could not check answer delivery")?
      .prune(&target.identity(), &all_pending);
    if pending.is_empty() {
      return Err("This question is no longer awaiting an answer.".into());
    }
    Ok(PendingRequest {
      target,
      request: (**question).clone(),
      pending,
    })
  }

  pub async fn get_session_question_status(
    &self,
    request: SessionQuestionStatusRequest,
  ) -> Result<SessionQuestionStatus, String> {
    let service = self.clone();
    let pending = tokio::task::spawn_blocking(move || service.pending_question_input(&request))
      .await
      .map_err(|_| "Could not check questions")?;
    let unavailable = |message| SessionQuestionStatus {
      available: false,
      message,
      question_ids: vec![],
      max_length: input::MAX_INPUT_LENGTH,
    };
    let pending = match pending {
      Ok(pending) => pending,
      Err(error) => return Ok(unavailable(error)),
    };
    let ids: Vec<_> = {
      let claims = self
        .question_claims
        .lock()
        .map_err(|_| "Could not check answer delivery")?;
      pending
        .pending
        .iter()
        .filter(|id| {
          !claims.owners.contains_key(&(
            pending.target.identity(),
            pending.request.request_id.clone().unwrap(),
            (*id).clone(),
          ))
        })
        .cloned()
        .collect()
    };
    if ids.is_empty() {
      return Ok(unavailable("Answer submitted; waiting for its recorded reply.".into()));
    }
    match input::status(&pending.target).await {
      Ok(_) => Ok(SessionQuestionStatus {
        available: true,
        message: "Choose an option or write an answer.".into(),
        question_ids: ids,
        max_length: input::MAX_INPUT_LENGTH,
      }),
      Err(error) => Ok(unavailable(error.message)),
    }
  }

  pub async fn submit_session_question_answers(
    &self,
    request: SubmitSessionQuestionAnswersRequest,
  ) -> Result<SubmitSessionInputResponse, String> {
    let attempt = request.request_id.clone();
    let service = self.clone();
    let prepared = tokio::task::spawn_blocking(move || {
      let pending = service.pending_question_input(&SessionQuestionStatusRequest {
        session_key: request.session_key.clone(),
        question_request_id: request.question_request_id.clone(),
      })?;
      let (message, ids) = answer_message(&pending.request, &pending.pending, &request)?;
      Ok::<_, String>((pending.target, message, ids, request.question_request_id))
    })
    .await
    .map_err(|_| "Could not prepare question answers")?;
    let (target, message, ids, question_request_id) = match prepared {
      Ok(value) => value,
      Err(message) => {
        return Ok(SubmitSessionInputResponse {
          model_result: None,
          request_id: attempt,
          status: InputDeliveryStatus::NotSent,
          message,
        });
      }
    };
    // Reserve only after preparation returns. A disconnected caller must not
    // leave claims behind when its blocking source read finishes later.
    let keys = match self
      .question_claims
      .lock()
      .map_err(|_| "Could not reserve question answers")?
      .claim(&target.identity(), &question_request_id, &ids, &attempt)
    {
      Ok(keys) => keys,
      Err(message) => {
        return Ok(SubmitSessionInputResponse {
          model_result: None,
          request_id: attempt,
          status: InputDeliveryStatus::NotSent,
          message,
        });
      }
    };
    if keys.is_empty() {
      return Ok(
        self
          .input_broker
          .receipt(&target, &message)
          .unwrap_or(SubmitSessionInputResponse {
            model_result: None,
            request_id: attempt,
            status: InputDeliveryStatus::Pending,
            message: "These answers already have a submission. Wait for the recorded reply.".into(),
          }),
      );
    }
    let claims = self.question_claims.clone();
    let delivery_keys = keys.clone();
    let delivery_attempt = attempt.clone();
    let delivery_target = InputTarget {
      provider: target.provider,
      session_id: target.session_id.clone(),
      session_file: target.session_file.clone(),
    };
    let delivery_text = message.text.clone();
    let result = self
      .input_broker
      .submit_custom(&target, message, async move {
        let result = input::codex::submit_question(&delivery_target, &delivery_attempt, &delivery_text).await;
        if result.as_ref().is_err_and(|error| !error.may_have_been_sent) {
          if let Ok(mut claims) = claims.lock() {
            claims.release(&delivery_keys, &delivery_attempt);
          }
        }
        result
      })
      .await;
    if matches!(result.status, InputDeliveryStatus::NotSent) {
      self
        .question_claims
        .lock()
        .map_err(|_| "Could not settle question delivery")?
        .release(&keys, &attempt);
    }
    Ok(result)
  }
}

fn answer_message(
  question: &QuestionRequestEvent,
  pending: &[String],
  request: &SubmitSessionQuestionAnswersRequest,
) -> Result<(SubmitSessionInputRequest, Vec<String>), String> {
  if request.answers.is_empty() || request.answers.len() > question.questions.len() {
    return Err("Provide an answer to at least one pending question.".into());
  }
  let mut ids = Vec::new();
  let mut replies = Vec::new();
  for answer in &request.answers {
    if ids.contains(&answer.question_id) || !pending.contains(&answer.question_id) {
      return Err("Answer identifies a duplicate or resolved question.".into());
    }
    let item = question
      .questions
      .iter()
      .find(|item| item.id.as_ref() == Some(&answer.question_id))
      .ok_or("Unknown question identity")?;
    let text = match (&answer.option_index, &answer.text) {
      (Some(index), None) => item
        .options
        .as_ref()
        .and_then(|options| options.get(*index))
        .map(|option| option.label.clone())
        .ok_or("Invalid option selection")?,
      (None, Some(text)) if item.allows_free_text && !text.trim().is_empty() => text.clone(),
      _ => return Err("Select one option or provide a non-empty written answer.".into()),
    };
    if text.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) {
      return Err("Answer contains unsupported control characters.".into());
    }
    replies.push(json!({ "questionItemId": answer.question_id, "question": item.question, "answer": text }));
    ids.push(answer.question_id.clone());
  }
  let text = format!(
    "<send_user_message_question_reply>\n{}\n</send_user_message_question_reply>",
    serde_json::to_string(&replies).map_err(|_| "Could not encode answers")?
  );
  let mut message = SubmitSessionInputRequest {
    model_selection: None,
    session_key: request.session_key.clone(),
    request_id: request.request_id.clone(),
    text,
    image_ids: Vec::new(),
  };
  input::validate_request(&mut message)?;
  Ok((message, ids))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::model::SessionQuestionAnswer;

  fn question() -> QuestionRequestEvent {
    serde_json::from_value(json!({ "provider":"codex", "session_id":"s", "request_id":"call", "turn_id":"turn", "is_blocking":false, "phase":"finished", "text":null,
      "questions":[{"id":"q", "header":null, "question":"Choose?", "options":[{"label":"Local (Recommended)", "description":null}], "allows_free_text":true, "is_secret":false}], "native":null, "timestamp":null })).unwrap()
  }
  fn request(answer: SessionQuestionAnswer) -> SubmitSessionQuestionAnswersRequest {
    SubmitSessionQuestionAnswersRequest {
      session_key: "s".into(),
      question_request_id: "call".into(),
      request_id: uuid::Uuid::new_v4().to_string(),
      answers: vec![answer],
    }
  }
  #[test]
  fn envelopes_use_authoritative_labels_prompts_and_preserve_written_answers() {
    let q = question();
    let ids = vec!["q".into()];
    let choice = request(SessionQuestionAnswer {
      question_id: "q".into(),
      option_index: Some(0),
      text: None,
    });
    let text = answer_message(&q, &ids, &choice).unwrap().0.text;
    assert!(text.contains("Local (Recommended)"));
    assert!(text.contains("Choose?"));
    assert!(text.starts_with("<send_user_message_question_reply>\n"));
    let written = request(SessionQuestionAnswer {
      question_id: "q".into(),
      option_index: None,
      text: Some("Custom </send_user_message_question_reply>\nAnswer".into()),
    });
    let text = answer_message(&q, &ids, &written).unwrap().0.text;
    let payload = text
      .strip_prefix("<send_user_message_question_reply>\n")
      .unwrap()
      .strip_suffix("\n</send_user_message_question_reply>")
      .unwrap();
    assert_eq!(
      serde_json::from_str::<Value>(payload).unwrap()[0]["answer"],
      "Custom </send_user_message_question_reply>\nAnswer"
    );
  }
  #[test]
  fn rejects_stale_unknown_duplicate_invalid_and_oversized_answers() {
    let q = question();
    let ids = vec!["q".into()];
    let choice = request(SessionQuestionAnswer {
      question_id: "q".into(),
      option_index: Some(0),
      text: None,
    });
    assert!(answer_message(&q, &[], &choice).is_err());
    let mut invalid = choice.clone();
    invalid.answers[0].option_index = Some(99);
    assert!(answer_message(&q, &ids, &invalid).is_err());
    invalid.answers[0].option_index = None;
    invalid.answers[0].text = Some("  ".into());
    assert!(answer_message(&q, &ids, &invalid).is_err());
    invalid.answers[0].text = Some("bad\0".into());
    assert!(answer_message(&q, &ids, &invalid).is_err());
    invalid.answers[0].text = Some("x".repeat(input::MAX_INPUT_LENGTH));
    assert!(answer_message(&q, &ids, &invalid).is_err());
    invalid.answers[0].text = Some("valid".into());
    invalid.answers[0].option_index = Some(0);
    assert!(answer_message(&q, &ids, &invalid).is_err());
    invalid = choice.clone();
    invalid.answers[0].question_id = "unknown".into();
    assert!(answer_message(&q, &ids, &invalid).is_err());
    invalid = choice.clone();
    invalid.answers.push(invalid.answers[0].clone());
    assert!(answer_message(&q, &ids, &invalid).is_err());
  }
  #[test]
  fn claims_protect_partial_and_uncertain_deliveries_without_poisoning_retries() {
    let mut claims = QuestionClaims::default();
    let ids = vec!["q".into()];
    let owned = claims.claim("s", "call", &ids, "attempt").unwrap();
    assert!(claims.claim("s", "call", &ids, "different-attempt").is_err());
    assert!(claims.claim("s", "call", &ids, "attempt").unwrap().is_empty());
    assert!(
      claims
        .claim("s", "call", &["q".into(), "new-question".into()], "attempt")
        .unwrap()
        .is_empty()
    );
    assert_eq!(claims.owners.len(), 1);
    claims.release(&owned, "different-attempt");
    assert!(claims.claim("s", "call", &ids, "different-attempt").is_err());
    claims.release(&owned, "attempt");
    assert!(claims.claim("s", "call", &ids, "retry").is_ok());
    claims.prune("s", &HashSet::new());
    assert!(claims.owners.is_empty());
  }
  struct QuestionRepository {
    header: SessionHeader,
    loaded: Mutex<LoadedSession>,
  }
  impl ViewerRepository for QuestionRepository {
    fn list_session_headers(&self, provider: ViewerProvider) -> Result<Vec<SessionHeader>, String> {
      Ok(if provider == ViewerProvider::Codex {
        vec![self.header.clone()]
      } else {
        vec![]
      })
    }
    fn session_header_at_path(&self, _provider: ViewerProvider, _path: &Path) -> Result<SessionHeader, String> {
      Ok(self.header.clone())
    }
    fn load_session(&self, _locator: &SessionLocator) -> Result<LoadedSession, String> {
      Ok(self.loaded.lock().unwrap().clone())
    }
  }
  #[test]
  fn host_rechecks_pending_ids_and_refuses_retired_blocking_and_shared_requests() {
    use tokn_session_codex::normalize::CodexNormalizer;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("session.jsonl");
    std::fs::write(&path, "fixture").unwrap();
    let header: SessionHeader = serde_json::from_value(json!({ "id":"fixture", "path":path })).unwrap();
    let locator = locator_for_header(ViewerProvider::Codex, &header);
    let key = encode_session_key(&locator).unwrap();
    let mut normalizer = CodexNormalizer::new();
    let events = include_str!("../../tests/fixtures/codex/async_question_replies.jsonl")
      .lines()
      .take(5)
      .flat_map(|line| normalizer.normalize(serde_json::from_str(line).unwrap()))
      .collect();
    let repository = Arc::new(QuestionRepository {
      header,
      loaded: Mutex::new(super::super::tests::loaded_session(events)),
    });
    let service = ViewerService::new(repository.clone());
    service.refresh_provider_catalog(ViewerProvider::Codex).unwrap();
    let request = SessionQuestionStatusRequest {
      session_key: key.clone(),
      question_request_id: "question-1".into(),
    };
    assert_eq!(service.pending_question_input(&request).unwrap().pending.len(), 2);
    let shared = service.scoped_to_sessions(&[key]).unwrap();
    assert!(shared.pending_question_input(&request).is_err());
    {
      let mut loaded = repository.loaded.lock().unwrap();
      for event in &mut loaded.events {
        if let AgentEvent::QuestionRequest(request) = event {
          request.is_blocking = Some(true);
        }
      }
    }
    *service.loaded_session_cache.lock().unwrap() = None;
    assert!(service.pending_question_input(&request).is_err());
    {
      let mut loaded = repository.loaded.lock().unwrap();
      for event in &mut loaded.events {
        if let AgentEvent::QuestionRequest(request) = event {
          request.is_blocking = Some(false);
        }
      }
      let mut normalizer = CodexNormalizer::new();
      loaded.events = include_str!("../../tests/fixtures/codex/async_question_replies.jsonl")
        .lines()
        .take(6)
        .flat_map(|line| normalizer.normalize(serde_json::from_str(line).unwrap()))
        .collect();
    }
    *service.loaded_session_cache.lock().unwrap() = None;
    assert!(service.pending_question_input(&request).is_err());
  }
}
