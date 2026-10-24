use super::*;
use base64::Engine;

pub(super) fn model(value: &Value) -> Option<ModelIdentity> {
  wire::parse_model(value).map(model_identity)
}

pub(super) fn model_identity(model: wire::PiModel) -> ModelIdentity {
  ModelIdentity {
    model_provider: Some(model.provider),
    model_id: model.id,
    display_name: model.name,
  }
}

pub(super) fn models(value: &Value) -> Result<Vec<ModelChoice>, InteractionError> {
  let values = value
    .get("models")
    .ok_or_else(|| InteractionError::unavailable("Pi returned no valid model catalog"))?;
  Ok(
    wire::parse_model_catalog(values)
      .map_err(InteractionError::unavailable)?
      .into_iter()
      .map(|model| ModelChoice {
        supports_images: model.supports_images,
        model: model_identity(model),
      })
      .collect(),
  )
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
      allows_option_notes: false,
      is_secret: false,
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
  let response = dialog
    .response(value, confirmed, cancelled)
    .map_err(InteractionError::invalid)?;
  serde_json::to_value(response).map_err(|error| InteractionError::invalid(error.to_string()))
}
