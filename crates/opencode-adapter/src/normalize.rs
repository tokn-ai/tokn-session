use serde_json::{Value, json};
use tokn_interaction_core::*;
use tokn_opencode_rpc_protocol as wire;

pub(crate) fn model(model: &wire::ModelRef) -> ModelIdentity {
  ModelIdentity {
    model_provider: Some(model.provider_id.clone()),
    model_id: model.id.clone(),
    display_name: None,
  }
}

pub(crate) fn question(native: wire::QuestionRequest) -> PendingInteraction {
  let questions = native
    .questions
    .iter()
    .enumerate()
    .map(|(index, question)| QuestionItem {
      question_id: format!("{}:{index}", native.id),
      prompt: question.question.clone(),
      options: question
        .options
        .iter()
        .map(|option| QuestionOption {
          label: option.label.clone(),
          description: Some(option.description.clone()),
        })
        .collect(),
      allows_free_text: question.custom,
      allows_option_notes: false,
      is_secret: false,
      multiple: question.multiple,
    })
    .collect();
  PendingInteraction {
    interaction_id: format!("question:{}", native.id),
    session_id: Some(native.session_id.clone()),
    kind: InteractionKind::Question,
    title: native
      .questions
      .first()
      .map(|question| question.header.clone())
      .unwrap_or_else(|| "Question".into()),
    questions,
    native: serde_json::to_value(native).expect("native question is serializable"),
  }
}

pub(crate) fn permission(native: wire::PermissionRequest) -> PendingInteraction {
  PendingInteraction {
    interaction_id: format!("permission:{}", native.id),
    session_id: Some(native.session_id.clone()),
    kind: InteractionKind::Permission,
    title: native.action.clone(),
    questions: vec![],
    native: serde_json::to_value(native).expect("native permission is serializable"),
  }
}

pub(crate) fn answer_body(
  request: &PendingInteraction,
  answers: Vec<QuestionAnswer>,
) -> Result<Value, InteractionError> {
  if answers.len() != request.questions.len() {
    return Err(InteractionError::invalid("Answer every OpenCode question exactly once"));
  }
  let mut result = Vec::new();
  for question in &request.questions {
    let matched: Vec<_> = answers
      .iter()
      .filter(|answer| answer.question_id == question.question_id)
      .collect();
    if matched.len() != 1 {
      return Err(InteractionError::invalid("Unknown or duplicate OpenCode question ID"));
    }
    let answer = matched[0];
    let mut labels = Vec::new();
    for label in &answer.answers {
      if !question.options.iter().any(|option| option.label == *label) || labels.contains(label) {
        return Err(InteractionError::invalid(
          "OpenCode answer contains an unknown or duplicate option",
        ));
      }
      labels.push(label.clone());
    }
    if let Some(custom) = &answer.custom {
      if !question.allows_free_text || custom.trim().is_empty() || custom.len() > 64 * 1024 {
        return Err(InteractionError::invalid(
          "OpenCode question does not allow this free-text answer",
        ));
      }
      labels.push(custom.clone());
    }
    if labels.is_empty() || (!question.multiple && labels.len() > 1) {
      return Err(InteractionError::invalid(
        "Choose one answer unless the OpenCode question allows multiple choices",
      ));
    }
    result.push(labels);
  }
  Ok(json!({"answers":result}))
}

pub(crate) fn event_kind(event: &wire::NativeEvent) -> Option<RuntimeEventKind> {
  match event.kind.as_str() {
    "question.v2.asked" => serde_json::from_value(event.data.clone())
      .ok()
      .map(question)
      .map(RuntimeEventKind::InteractionRequested),
    "permission.v2.asked" => serde_json::from_value(event.data.clone())
      .ok()
      .map(permission)
      .map(RuntimeEventKind::InteractionRequested),
    "question.v2.replied" | "question.v2.rejected" | "permission.v2.replied" => {
      let prefix = if event.kind.starts_with("question.") {
        "question"
      } else {
        "permission"
      };
      event
        .data
        .get("requestID")
        .and_then(Value::as_str)
        .map(|id| RuntimeEventKind::InteractionResolved {
          interaction_id: format!("{prefix}:{id}"),
        })
    }
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn request() -> PendingInteraction {
    question(
      serde_json::from_value(json!({"id":"que_1", "sessionID":"ses_1", "questions":[{
        "question":"Choose", "header":"Choice", "options":[{"label":"One", "description":"First"}]
      }]}))
      .unwrap(),
    )
  }

  #[test]
  fn explicit_custom_answer_is_not_guessed_from_labels() {
    let request = request();
    assert_eq!(
      answer_body(
        &request,
        vec![QuestionAnswer {
          question_id: "que_1:0".into(),
          answers: vec![],
          custom: Some("My answer".into()),
        }]
      )
      .unwrap(),
      json!({"answers":[["My answer"]]})
    );
    assert!(
      answer_body(
        &request,
        vec![QuestionAnswer {
          question_id: "que_1:0".into(),
          answers: vec!["unknown".into()],
          custom: None,
        }]
      )
      .is_err()
    );
  }

  #[test]
  fn answer_requires_exact_ids_and_native_selection_limits() {
    let request = request();
    for answer in [
      QuestionAnswer {
        question_id: "other".into(),
        answers: vec!["One".into()],
        custom: None,
      },
      QuestionAnswer {
        question_id: "que_1:0".into(),
        answers: vec!["One".into(), "One".into()],
        custom: None,
      },
      QuestionAnswer {
        question_id: "que_1:0".into(),
        answers: vec!["One".into()],
        custom: Some("Other".into()),
      },
    ] {
      assert!(answer_body(&request, vec![answer]).is_err());
    }
  }
}
