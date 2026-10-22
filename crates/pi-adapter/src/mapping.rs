use super::*;
use base64::Engine;
use std::collections::HashSet;

pub(super) fn model(value: &Value) -> Option<ModelIdentity> {
  let provider = value.get("provider")?.as_str()?;
  let id = value.get("id")?.as_str()?;
  if !valid_identity(provider) || !valid_identity(id) {
    return None;
  }
  Some(ModelIdentity {
    model_provider: Some(provider.to_owned()),
    model_id: id.to_owned(),
    display_name: value
      .get("name")
      .and_then(Value::as_str)
      .filter(|value| valid_identity(value))
      .map(str::to_owned),
  })
}

pub(super) fn models(value: &Value) -> Result<Vec<ModelChoice>, InteractionError> {
  let values = value
    .get("models")
    .and_then(Value::as_array)
    .ok_or_else(|| InteractionError::unavailable("Pi returned no valid model catalog"))?;
  if values.len() > 512 {
    return Err(InteractionError::unavailable("Pi model catalog exceeds 512 entries"));
  }
  let mut seen = HashSet::new();
  values
    .iter()
    .map(|value| {
      let model = model(value).ok_or_else(|| InteractionError::unavailable("Pi returned an invalid model identity"))?;
      if !seen.insert((model.model_provider.clone(), model.model_id.clone())) {
        return Err(InteractionError::unavailable("Pi returned duplicate model identities"));
      }
      Ok(ModelChoice {
        model,
        supports_images: value
          .get("input")
          .and_then(Value::as_array)
          .is_some_and(|input| input.iter().any(|value| value.as_str() == Some("image"))),
      })
    })
    .collect()
}

pub(super) fn commands(value: &Value) -> Result<Vec<String>, InteractionError> {
  let commands = value
    .get("commands")
    .and_then(Value::as_array)
    .ok_or_else(|| InteractionError::unavailable("Pi returned no command metadata"))?;
  if commands.len() > 2048 {
    return Err(InteractionError::unavailable("Pi command catalog exceeds 2048 entries"));
  }
  commands
    .iter()
    .map(|command| {
      command
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| valid_identity(name))
        .map(str::to_owned)
        .ok_or_else(|| InteractionError::unavailable("Pi returned an invalid command name"))
    })
    .collect()
}

pub(super) fn validate_input(input: &MessageInput) -> Result<(), InteractionError> {
  if (input.text.trim().is_empty() && input.images.is_empty())
    || input.text.len() > 256 * 1024
    || input.text.contains('\0')
  {
    return Err(InteractionError::invalid(
      "Pi input requires bounded text or an image attachment",
    ));
  }
  if input.images.len() > 4 {
    return Err(InteractionError::invalid("Pi input supports at most four images"));
  }
  let mut total = 0;
  for image in &input.images {
    if !matches!(
      image.mime_type.as_str(),
      "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    ) || image.data.len() > 12 * 1024 * 1024
    {
      return Err(InteractionError::invalid(
        "Pi image format or encoded size is unsupported",
      ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
      .decode(&image.data)
      .map_err(|_| InteractionError::invalid("Pi image data is not valid base64"))?;
    if bytes.is_empty() || bytes.len() > 8 * 1024 * 1024 {
      return Err(InteractionError::invalid(
        "Pi image exceeds the 8 MiB limit or is empty",
      ));
    }
    total += bytes.len();
  }
  if total > 24 * 1024 * 1024 {
    return Err(InteractionError::invalid("Pi images exceed the 24 MiB total limit"));
  }
  Ok(())
}

pub(super) fn interaction(
  dialog: &wire::ExtensionDialog,
  session_id: Option<String>,
  native: Value,
) -> PendingInteraction {
  let kind = match dialog.kind.as_str() {
    "confirm" => InteractionKind::Confirm,
    "select" => InteractionKind::Select,
    "editor" => InteractionKind::Editor,
    _ => InteractionKind::Input,
  };
  PendingInteraction {
    interaction_id: dialog.dialog_id.clone(),
    session_id,
    kind,
    title: dialog.title.clone(),
    questions: vec![QuestionItem {
      question_id: dialog.dialog_id.clone(),
      prompt: dialog.message.clone().unwrap_or_else(|| dialog.title.clone()),
      options: dialog
        .options
        .iter()
        .map(|label| QuestionOption {
          label: label.clone(),
          description: None,
        })
        .collect(),
      allows_free_text: matches!(dialog.kind.as_str(), "input" | "editor"),
      multiple: false,
    }],
    native,
  }
}

pub(super) fn dialog_response(
  dialog: &wire::ExtensionDialog,
  response: &InteractionResponse,
) -> Result<Value, InteractionError> {
  let (value, confirmed, cancelled) = match response {
    InteractionResponse::Reject => (None, None, true),
    InteractionResponse::Dialog {
      value,
      confirmed,
      cancelled,
    } => (value.clone(), *confirmed, *cancelled),
    InteractionResponse::Answers { answers }
      if answers.len() == 1 && answers[0].question_id == dialog.dialog_id && dialog.kind != "confirm" =>
    {
      let answer = &answers[0];
      let value = match (&answer.custom, answer.answers.as_slice()) {
        (Some(value), []) if matches!(dialog.kind.as_str(), "input" | "editor") => value.clone(),
        (None, [value]) => value.clone(),
        _ => return Err(InteractionError::invalid("This Pi dialog requires one value")),
      };
      (Some(value), None, false)
    }
    _ => return Err(InteractionError::invalid("The response does not match this Pi dialog")),
  };
  let response = if cancelled {
    wire::ExtensionUiResponse::ExtensionUiResponse {
      id: dialog.dialog_id.clone(),
      value: None,
      confirmed: None,
      cancelled: Some(true),
    }
  } else if dialog.kind == "confirm" {
    if value.is_some() {
      return Err(InteractionError::invalid("Pi confirmation requires a boolean"));
    }
    wire::ExtensionUiResponse::ExtensionUiResponse {
      id: dialog.dialog_id.clone(),
      value: None,
      confirmed: Some(confirmed.ok_or_else(|| InteractionError::invalid("Pi confirmation requires yes or no"))?),
      cancelled: None,
    }
  } else {
    if confirmed.is_some() {
      return Err(InteractionError::invalid("Pi selection/text dialogs require a value"));
    }
    let value = value.ok_or_else(|| InteractionError::invalid("This Pi dialog requires a value"))?;
    if value.len() > wire::MAX_DIALOG_TEXT_BYTES || (dialog.kind == "select" && !dialog.options.contains(&value)) {
      return Err(InteractionError::invalid(
        "The Pi dialog answer is oversized or not an available option",
      ));
    }
    wire::ExtensionUiResponse::ExtensionUiResponse {
      id: dialog.dialog_id.clone(),
      value: Some(value),
      confirmed: None,
      cancelled: None,
    }
  };
  serde_json::to_value(response).map_err(|error| InteractionError::invalid(error.to_string()))
}
