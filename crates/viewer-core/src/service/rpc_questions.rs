//! Recorded cards identify a request; only a live adapter callback authorizes
//! its response. Native callback envelopes never cross the viewer API.
use super::*;
use crate::{
  input::{self, InputTarget},
  model::{
    SessionQuestionStatus, SessionQuestionStatusRequest, SubmitSessionInputRequest, SubmitSessionInputResponse,
    SubmitSessionQuestionAnswersRequest,
  },
  session_supervisor::{SessionTransport, VerifiedSession},
};
use tokn_interaction_core::{InteractionResponse, PendingInteraction, QuestionAnswer};

impl ViewerService {
  async fn rpc_question(
    &self,
    key: String,
    request_id: &str,
  ) -> Result<Option<(VerifiedSession, PendingInteraction)>, String> {
    let target = self.control_target(key).await?;
    if self.sessions.transport(&target).await? != SessionTransport::Rpc || target.input.provider == ViewerProvider::Pi {
      return Ok(None);
    }
    let requests = self.sessions.pending_rpc(&target).await?;
    let pending = requests.into_iter().find(|pending| {
      pending.session_id.as_deref() == Some(target.input.session_id.as_str())
        && (pending.interaction_id == request_id
          || pending.native.get("params").and_then(|params| params.get("itemId")).and_then(Value::as_str) == Some(request_id))
    }).filter(|pending| !pending.questions.is_empty()).ok_or(
      "No live RPC callback owns this question. Codex Desktop asynchronous questions require explicitly selecting Desktop control.",
    )?;
    Ok(Some((target, pending)))
  }

  pub(super) async fn rpc_question_status(
    &self,
    request: &SessionQuestionStatusRequest,
  ) -> Result<Option<SessionQuestionStatus>, String> {
    let result = self
      .rpc_question(request.session_key.clone(), &request.question_request_id)
      .await;
    Ok(match result {
      Ok(None) => None,
      Ok(Some((_, pending))) => Some(SessionQuestionStatus {
        secret_question_ids: pending
          .questions
          .iter()
          .filter(|question| question.is_secret)
          .map(|question| question.question_id.clone())
          .collect(),
        available: true,
        message: "Answer the pending RPC request.".into(),
        question_ids: pending
          .questions
          .into_iter()
          .map(|question| question.question_id)
          .collect(),
        max_length: input::MAX_INPUT_LENGTH,
      }),
      Err(message) => Some(SessionQuestionStatus {
        secret_question_ids: Vec::new(),
        available: false,
        message,
        question_ids: Vec::new(),
        max_length: input::MAX_INPUT_LENGTH,
      }),
    })
  }

  pub(super) async fn submit_rpc_question(
    &self,
    request: &SubmitSessionQuestionAnswersRequest,
  ) -> Result<Option<SubmitSessionInputResponse>, String> {
    let unavailable = |message: String| {
      Some(crate::session_supervisor::input_response(
        request.request_id.clone(),
        Err(input::DeliveryError::not_sent(message)),
      ))
    };
    if uuid::Uuid::parse_str(&request.request_id).is_err() {
      return Ok(unavailable("Invalid answer request ID".into()));
    }
    let target = match self.control_target(request.session_key.clone()).await {
      Ok(target) => target,
      Err(error) => return Ok(unavailable(error)),
    };
    if self.sessions.transport(&target).await? != SessionTransport::Rpc || target.input.provider == ViewerProvider::Pi {
      return Ok(None);
    }
    let broker_target = InputTarget {
      provider: target.input.provider,
      session_id: target.input.session_id.clone(),
      session_file: target.input.session_file.clone(),
    };
    let fingerprint = request
      .answers
      .iter()
      .map(|answer| (&answer.question_id, answer.option_index, &answer.text))
      .collect::<Vec<_>>();
    let message = SubmitSessionInputRequest {
      session_key: target.session_key.clone(),
      request_id: request.request_id.clone(),
      // A stable receipt fingerprint, never delivered as prompt text. It is
      // available even after the native callback has already been retired.
      text: serde_json::to_string(&("session-control:answer", &request.question_request_id, fingerprint))
        .map_err(|_| "Could not encode answers")?,
      image_ids: Vec::new(),
      model_selection: None,
    };
    if let Some(receipt) = self.input_broker.control_receipt(&broker_target, &message) {
      return Ok(Some(receipt));
    }
    let pending = match self
      .rpc_question(request.session_key.clone(), &request.question_request_id)
      .await
    {
      Ok(Some((_, pending))) => pending,
      Ok(None) => return Ok(unavailable("The session's control transport changed".into())),
      Err(message) => return Ok(unavailable(message)),
    };
    let answers = match rpc_answers(&pending, request) {
      Ok(answers) => answers,
      Err(message) => return Ok(unavailable(message)),
    };
    let sessions = self.sessions.clone();
    let request_id = request.request_id.clone();
    Ok(Some(
      self
        .input_broker
        .submit_control(&broker_target, message, async move {
          sessions
            .respond_rpc(
              &target,
              request_id,
              pending.interaction_id,
              InteractionResponse::Answers { answers },
            )
            .await
        })
        .await,
    ))
  }
}

fn rpc_answers(
  pending: &PendingInteraction,
  request: &SubmitSessionQuestionAnswersRequest,
) -> Result<Vec<QuestionAnswer>, String> {
  if request.answers.len() != pending.questions.len() {
    return Err("Answer every question in this live request together".into());
  }
  let mut seen = HashSet::new();
  request
    .answers
    .iter()
    .map(|answer| {
      if !seen.insert(&answer.question_id) {
        return Err("Duplicate answer identity".into());
      }
      let question = pending
        .questions
        .iter()
        .find(|question| question.question_id == answer.question_id)
        .ok_or("This question is no longer pending")?;
      let selected = answer
        .option_index
        .map(|index| {
          question
            .options
            .get(index)
            .map(|option| option.label.clone())
            .ok_or("Invalid question option")
        })
        .transpose()?;
      let text = answer.text.as_ref().filter(|text| !text.trim().is_empty());
      if text.is_some_and(|text| {
        text.chars().count() > input::MAX_INPUT_LENGTH
          || text.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
      }) {
        return Err("Question answer exceeds its limit or contains unsupported controls".into());
      }
      if selected.is_none() && (text.is_none() || !question.allows_free_text)
        || selected.is_some() && text.is_some() && !question.allows_option_notes
      {
        return Err("Provide an allowed option or written answer".into());
      }
      Ok(QuestionAnswer {
        question_id: answer.question_id.clone(),
        answers: selected.into_iter().collect(),
        custom: text.cloned(),
      })
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn live_rpc_answers_preserve_option_notes_and_require_exact_complete_batch() {
    let pending: PendingInteraction = serde_json::from_value(json!({
      "interaction_id":"request:7", "session_id":"thread", "kind":"question", "title":"Choose", "native":{},
      "questions":[{"question_id":"one","prompt":"Choose", "options":[{"label":"A","description":null}],
        "allows_free_text":true,"allows_option_notes":true,"multiple":false}]
    }))
    .unwrap();
    let mut request: SubmitSessionQuestionAnswersRequest = serde_json::from_value(json!({
      "session_key":"key","question_request_id":"call","request_id":"attempt",
      "answers":[{"question_id":"one","option_index":0,"text":"with notes"}]
    }))
    .unwrap();
    let answers = rpc_answers(&pending, &request).unwrap();
    assert_eq!(answers[0].answers, ["A"]);
    assert_eq!(answers[0].custom.as_deref(), Some("with notes"));
    request.answers[0].question_id = "other".into();
    assert!(rpc_answers(&pending, &request).is_err());
    request.answers.clear();
    assert!(rpc_answers(&pending, &request).is_err());
  }
}
