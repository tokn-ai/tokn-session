use crate::Inner;
use serde_json::{Value, json};
use tokn_interaction_core::*;

impl Inner {
  pub(crate) async fn respond(
    &self,
    session_id: Option<&str>,
    interaction_id: &str,
    response: InteractionResponse,
  ) -> Result<Reply, InteractionError> {
    self.ensure_live()?;
    let pending = self
      .state
      .lock()
      .await
      .pending
      .get(interaction_id)
      .cloned()
      .ok_or_else(|| {
        InteractionError::new(
          ErrorKind::Conflict,
          "DSH interaction is no longer pending or has not been recovered yet",
        )
      })?;
    if session_id != pending.session_id.as_deref() {
      return Err(InteractionError::new(
        ErrorKind::StaleTarget,
        "DSH interaction belongs to a different session",
      ));
    }
    let result = response_result(&pending, response)?;
    let receipt = self.transport.respond(interaction_id, result).await?;
    if !receipt.accepted {
      return Err(InteractionError::new(
        ErrorKind::Conflict,
        format!(
          "DSH interaction response refused: {}",
          receipt.reason.as_deref().unwrap_or("unknown reason")
        ),
      ));
    }
    // Resolve locally after authoritative acceptance; the stream's matching
    // resolution remains safe and idempotent when it arrives before/after this.
    if self.state.lock().await.pending.remove(interaction_id).is_some() {
      self.emit(
        pending.session_id,
        RuntimeEventKind::InteractionResolved {
          interaction_id: interaction_id.into(),
        },
      );
    }
    Ok(Reply::Acknowledged)
  }
}

fn response_result(pending: &PendingInteraction, response: InteractionResponse) -> Result<Value, InteractionError> {
  match (pending.kind, response) {
    (InteractionKind::Question, InteractionResponse::Answers { answers }) => {
      if answers.len() != pending.questions.len() {
        return Err(InteractionError::invalid("DSH requires the complete answer batch"));
      }
      let mut native = Vec::new();
      let mut ids = std::collections::BTreeSet::new();
      for question in &pending.questions {
        let answer = answers
          .iter()
          .find(|answer| answer.question_id == question.question_id)
          .ok_or_else(|| InteractionError::invalid("DSH answer is missing a question id"))?;
        if !ids.insert(answer.question_id.clone()) {
          return Err(InteractionError::invalid("duplicate DSH answer id"));
        }
        let custom = answer.custom.as_deref().map(str::trim);
        if custom == Some("") {
          return Err(InteractionError::invalid("DSH custom answer must not be empty"));
        }
        let mut selected = std::collections::BTreeSet::new();
        for label in &answer.answers {
          if !question.options.iter().any(|option| option.label == *label) || !selected.insert(label) {
            return Err(InteractionError::invalid(
              "DSH answer contains an unknown or duplicate option",
            ));
          }
        }
        if answer.answers.is_empty() && custom.is_none() {
          return Err(InteractionError::invalid("DSH question has no answer"));
        }
        if !question.multiple && (answer.answers.len() > 1 || (!answer.answers.is_empty() && custom.is_some())) {
          return Err(InteractionError::invalid(
            "DSH single-choice questions require one option or custom text",
          ));
        }
        let mut item = json!({"id":question.question_id,"selected":answer.answers});
        if let Some(custom) = custom {
          item["custom"] = json!(custom);
        }
        native.push(item);
      }
      Ok(json!({"ok":true,"value":{"sessionId":pending.session_id,"answer":{"answers":native}}}))
    }
    (InteractionKind::Question, InteractionResponse::Reject)
    | (InteractionKind::Question, InteractionResponse::Dialog { cancelled: true, .. }) => {
      Ok(json!({"ok":false,"error":{"code":"cancelled","message":"The user cancelled the question","details":{}}}))
    }
    (InteractionKind::Permission, InteractionResponse::Permission { decision }) => {
      let outcome = match decision {
        PermissionDecision::AllowOnce => "allowed-once",
        PermissionDecision::Deny => "rejected",
        PermissionDecision::AllowAlways => {
          return Err(InteractionError::unsupported(
            "DSH permissions offer one-shot approval only",
          ));
        }
      };
      Ok(
        json!({"ok":true,"value":{"sessionId":pending.session_id,"approvalId":pending.native["payload"]["approvalId"],"outcome":outcome}}),
      )
    }
    (InteractionKind::Permission, InteractionResponse::Reject) => Ok(
      json!({"ok":true,"value":{"sessionId":pending.session_id,"approvalId":pending.native["payload"]["approvalId"],"outcome":"rejected"}}),
    ),
    _ => Err(InteractionError::invalid(
      "response type does not match the pending DSH interaction",
    )),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn question(multiple: bool) -> PendingInteraction {
    PendingInteraction {
      interaction_id: "native-rpc".into(),
      session_id: Some("session".into()),
      kind: InteractionKind::Question,
      title: "Question".into(),
      questions: vec![QuestionItem {
        question_id: "q".into(),
        prompt: "Select".into(),
        options: vec![QuestionOption {
          label: "A".into(),
          description: None,
        }],
        allows_free_text: true,
        multiple,
      }],
      native: json!({}),
    }
  }

  #[test]
  fn multi_select_preserves_selected_and_custom_separately() {
    let response = InteractionResponse::Answers {
      answers: vec![QuestionAnswer {
        question_id: "q".into(),
        answers: vec!["A".into()],
        custom: Some("A".into()),
      }],
    };
    let result = response_result(&question(true), response).unwrap();
    assert_eq!(
      result["value"]["answer"]["answers"][0],
      json!({"id":"q","selected":["A"],"custom":"A"})
    );
  }

  #[test]
  fn single_select_rejects_option_plus_custom_and_unknown_options() {
    for answer in [
      QuestionAnswer {
        question_id: "q".into(),
        answers: vec!["A".into()],
        custom: Some("other".into()),
      },
      QuestionAnswer {
        question_id: "q".into(),
        answers: vec!["missing".into()],
        custom: None,
      },
    ] {
      assert!(response_result(&question(false), InteractionResponse::Answers { answers: vec![answer] }).is_err());
    }
  }
}
