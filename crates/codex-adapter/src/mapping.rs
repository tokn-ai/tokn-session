use base64::Engine;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use tokn_codex_rpc_protocol as wire;
use tokn_interaction_core::*;

pub(super) fn valid_id(id: &str) -> bool {
  !id.is_empty() && id.len() <= 512 && id.trim() == id && !id.chars().any(char::is_control)
}

pub(super) fn model(value: &Value) -> Option<ModelIdentity> {
  let model_id = value.get("model")?.as_str()?.to_owned();
  Some(ModelIdentity {
    model_id,
    model_provider: value.get("modelProvider").and_then(Value::as_str).map(str::to_owned),
    display_name: None,
  })
}

pub(super) fn status(value: &Value) -> ExecutionState {
  match value.get("type").and_then(Value::as_str) {
    Some("active") => ExecutionState::Running,
    Some("idle") => ExecutionState::Idle,
    Some("notLoaded") => ExecutionState::Stopped,
    _ => ExecutionState::Unknown,
  }
}

pub(super) fn input(input: &MessageInput) -> Result<Vec<Value>, InteractionError> {
  if input.text.len() > 256 * 1024
    || input.text.contains('\0')
    || input.text.trim().is_empty() && input.images.is_empty()
    || input.images.len() > 4
  {
    return Err(InteractionError::invalid(
      "Codex needs bounded text or at most four images",
    ));
  }
  let mut result = Vec::new();
  let mut total = 0;
  for image in &input.images {
    if !matches!(
      image.mime_type.as_str(),
      "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    ) || image.data.len() > 12 * 1024 * 1024
    {
      return Err(InteractionError::invalid(
        "Unsupported Codex image format or encoded size",
      ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
      .decode(&image.data)
      .map_err(|_| InteractionError::invalid("Codex image data must be base64"))?;
    total += bytes.len();
    if bytes.is_empty() || bytes.len() > 8 * 1024 * 1024 || total > 24 * 1024 * 1024 {
      return Err(InteractionError::invalid(
        "Codex image exceeds the 8 MiB per-image or 24 MiB batch limit",
      ));
    }
    result.push(json!({"type":"image","url":format!("data:{};base64,{}", image.mime_type,image.data)}));
  }
  if !input.text.is_empty() {
    result.push(json!({"type":"text","text":input.text,"text_elements":[]}));
  }
  Ok(result)
}

pub(super) fn interaction_id(id: &wire::RpcId) -> String {
  format!("request:{}", serde_json::to_string(id).expect("RPC IDs serialize"))
}

pub(super) fn interaction(
  id: &wire::RpcId,
  method: &str,
  params: &Value,
  native: Value,
) -> Result<PendingInteraction, InteractionError> {
  if !valid_id(&interaction_id(id)) || serde_json::to_vec(&native).map_or(true, |bytes| bytes.len() > 256 * 1024) {
    return Err(InteractionError::invalid(
      "Codex interaction exceeds its identity or payload bound",
    ));
  }
  let valid_shape = match method {
    "item/tool/requestUserInput" => serde_json::from_value::<wire::ToolRequestUserInputParams>(params.clone()).is_ok(),
    "item/commandExecution/requestApproval" => {
      serde_json::from_value::<wire::CommandExecutionRequestApprovalParams>(params.clone()).is_ok()
    }
    "item/fileChange/requestApproval" => {
      serde_json::from_value::<wire::FileChangeRequestApprovalParams>(params.clone()).is_ok()
    }
    _ => return Err(InteractionError::unsupported("Unsupported Codex server request")),
  };
  if !valid_shape
    || ["turnId", "itemId"]
      .iter()
      .any(|key| params.get(key).and_then(Value::as_str).is_none_or(|id| !valid_id(id)))
  {
    return Err(InteractionError::invalid("Malformed Codex server request"));
  }
  let session = params
    .get("threadId")
    .and_then(Value::as_str)
    .filter(|id| valid_id(id))
    .ok_or_else(|| InteractionError::invalid("Codex request has no valid thread ID"))?;
  let (kind, title, questions) = match method {
    "item/tool/requestUserInput" => {
      let questions = params
        .get("questions")
        .and_then(Value::as_array)
        .filter(|questions| !questions.is_empty() && questions.len() <= 64)
        .ok_or_else(|| InteractionError::invalid("Codex question batch is invalid"))?;
      let mut ids = HashSet::new();
      let questions = questions
        .iter()
        .map(|question| {
          let id = question
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| valid_id(id))
            .ok_or_else(|| InteractionError::invalid("Codex question ID is invalid"))?;
          if !ids.insert(id) {
            return Err(InteractionError::invalid("Duplicate Codex question ID"));
          }
          let options = match question.get("options") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(options)) if options.len() <= 128 => options
              .iter()
              .map(|option| {
                Ok(QuestionOption {
                  label: option
                    .get("label")
                    .and_then(Value::as_str)
                    .filter(|label| !label.is_empty() && label.len() <= 4096)
                    .ok_or_else(|| InteractionError::invalid("Codex option label is missing"))?
                    .to_owned(),
                  description: option
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|text| text.len() <= 16 * 1024)
                    .map(str::to_owned),
                })
              })
              .collect::<Result<Vec<_>, InteractionError>>()?,
            _ => return Err(InteractionError::invalid("Codex question options are invalid")),
          };
          if options.iter().map(|option| &option.label).collect::<HashSet<_>>().len() != options.len() {
            return Err(InteractionError::invalid("Duplicate Codex option labels"));
          }
          Ok(QuestionItem {
            question_id: id.to_owned(),
            prompt: question
              .get("question")
              .and_then(Value::as_str)
              .filter(|text| text.len() <= 16 * 1024)
              .ok_or_else(|| InteractionError::invalid("Codex question text is missing"))?
              .to_owned(),
            allows_free_text: true,
            allows_option_notes: true,
            is_secret: question.get("isSecret").and_then(Value::as_bool).unwrap_or(false),
            options,
            multiple: false,
          })
        })
        .collect::<Result<Vec<_>, InteractionError>>()?;
      (InteractionKind::Question, "Codex question".to_owned(), questions)
    }
    "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
      let title = params
        .get("reason")
        .and_then(Value::as_str)
        .or_else(|| params.get("command").and_then(Value::as_str))
        .unwrap_or(if method.contains("fileChange") {
          "Approve file changes"
        } else {
          "Approve command execution"
        });
      (InteractionKind::Permission, title.to_owned(), Vec::new())
    }
    _ => return Err(InteractionError::unsupported("Unsupported Codex server request")),
  };
  Ok(PendingInteraction {
    interaction_id: interaction_id(id),
    session_id: Some(session.to_owned()),
    kind,
    title,
    questions,
    native,
  })
}

pub(super) fn response(
  pending: &PendingInteraction,
  method: &str,
  params: &Value,
  response: &InteractionResponse,
) -> Result<Value, InteractionError> {
  if method == "item/tool/requestUserInput" {
    let InteractionResponse::Answers { answers } = response else {
      return if matches!(response, InteractionResponse::Reject) {
        Ok(json!({"answers":{}}))
      } else {
        Err(InteractionError::invalid(
          "This Codex request requires question answers",
        ))
      };
    };
    let mut result = BTreeMap::new();
    for answer in answers {
      let question = pending
        .questions
        .iter()
        .find(|question| question.question_id == answer.question_id)
        .ok_or_else(|| InteractionError::invalid("Unknown Codex question"))?;
      if result.contains_key(&answer.question_id)
        || answer.answers.len() > 1
        || answer.answers.is_empty() && answer.custom.is_none()
        || answer
          .custom
          .as_ref()
          .is_some_and(|custom| !question.allows_free_text || custom.trim().is_empty() || custom.len() > 64 * 1024)
        || answer
          .answers
          .iter()
          .any(|label| !question.options.iter().any(|option| &option.label == label))
      {
        return Err(InteractionError::invalid(
          "Invalid, duplicate, or empty Codex answer; use custom for free text",
        ));
      }
      let mut values = answer.answers.clone();
      if let Some(custom) = &answer.custom {
        values.push(format!("user_note: {custom}"));
      }
      result.insert(answer.question_id.clone(), json!({"answers":values}));
    }
    if result.len() != pending.questions.len() {
      return Err(InteractionError::invalid("Answer the complete Codex question batch"));
    }
    return Ok(json!({"answers":result}));
  }
  let decision = match response {
    InteractionResponse::Permission {
      decision: PermissionDecision::AllowOnce,
    } => "accept",
    InteractionResponse::Permission {
      decision: PermissionDecision::Deny,
    }
    | InteractionResponse::Reject => "decline",
    InteractionResponse::Permission {
      decision: PermissionDecision::AllowAlways,
    } => {
      return Err(InteractionError::unsupported(
        "Codex generic approvals cannot promise a permanent grant; approve once",
      ));
    }
    _ => {
      return Err(InteractionError::invalid(
        "This Codex request requires a permission decision",
      ));
    }
  };
  if let Some(available) = params.get("availableDecisions").filter(|value| !value.is_null()) {
    let available = available
      .as_array()
      .ok_or_else(|| InteractionError::invalid("Invalid Codex approval decisions"))?;
    if !available.iter().any(|value| value.as_str() == Some(decision)) {
      return Err(InteractionError::unsupported(
        "This decision was not offered by the Codex approval request",
      ));
    }
  }
  Ok(json!({"decision":decision}))
}
