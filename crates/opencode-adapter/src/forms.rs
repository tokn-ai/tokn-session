//! Only ordinary question-tool forms map to the shared question contract.
//! Other form schemas remain visible and rejectable without guessed answers.
use super::*;
use std::collections::BTreeSet;

pub(super) fn project(form: wire::current::FormRequest) -> PendingInteraction {
  let questions = questions(&form);
  PendingInteraction {
    interaction_id: format!("form:{}", form.id),
    session_id: Some(form.session_id.clone()),
    kind: if questions.is_some() {
      InteractionKind::Question
    } else {
      InteractionKind::Editor
    },
    title: form.title.clone(),
    questions: questions.unwrap_or_default(),
    native: serde_json::to_value(form).expect("native form serializes"),
  }
}

fn questions(form: &wire::current::FormRequest) -> Option<Vec<QuestionItem>> {
  if form.metadata.as_ref()?.get("kind")?.as_str()? != "question" || form.fields.is_empty() || form.fields.len() > 64 {
    return None;
  }
  let mut keys = BTreeSet::new();
  form
    .fields
    .iter()
    .map(|field| {
      let key = field["key"].as_str()?;
      if key.is_empty()
        || key.len() > 1024
        || !keys.insert(key)
        || field["hidden"] == true
        || field
          .get("when")
          .is_some_and(|when| !when.as_array().is_some_and(Vec::is_empty))
        || [
          "format",
          "minLength",
          "maxLength",
          "pattern",
          "default",
          "minItems",
          "maxItems",
        ]
        .iter()
        .any(|name| field.get(name).is_some())
      {
        return None;
      }
      let multiple = match field["type"].as_str()? {
        "string" => false,
        "multiselect" => true,
        _ => return None,
      };
      let options = match field.get("options") {
        None => None,
        Some(options) => Some(options.as_array()?),
      };
      if options.is_some_and(|options| options.len() > 128) {
        return None;
      }
      let mut labels = BTreeSet::new();
      let options = options
        .into_iter()
        .flatten()
        .map(|option| {
          let label = option["label"].as_str()?;
          option["value"].as_str()?;
          if !labels.insert(label) {
            return None;
          }
          Some(QuestionOption {
            label: label.into(),
            description: option["description"].as_str().map(str::to_owned),
          })
        })
        .collect::<Option<Vec<_>>>()?;
      let allows_free_text = options.is_empty() || field["custom"] == true;
      Some(QuestionItem {
        question_id: format!("{}:{key}", form.id),
        prompt: field["description"]
          .as_str()
          .or_else(|| field["title"].as_str())
          .unwrap_or(&form.title)
          .into(),
        options,
        allows_free_text,
        allows_option_notes: false,
        is_secret: false,
        multiple,
      })
    })
    .collect()
}

pub(super) fn answer_body(
  request: &PendingInteraction,
  answers: Vec<QuestionAnswer>,
) -> Result<Value, InteractionError> {
  if request.kind != InteractionKind::Question || request.questions.is_empty() {
    return Err(InteractionError::unsupported(
      "This OpenCode form cannot be represented by ordinary question answers; decline it or use OpenCode",
    ));
  }
  normalize::answer_body(request, answers.clone())?;
  let fields = request.native["fields"]
    .as_array()
    .ok_or_else(|| InteractionError::unavailable("OpenCode form fields are unavailable"))?;
  let mut body = serde_json::Map::new();
  for (question, field) in request.questions.iter().zip(fields) {
    let answer = answers
      .iter()
      .find(|answer| answer.question_id == question.question_id)
      .expect("validated answer identity");
    let mut values = Vec::new();
    for label in &answer.answers {
      let value = field["options"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|option| option["label"].as_str() == Some(label.as_str()))
        .and_then(|option| option["value"].as_str())
        .ok_or_else(|| InteractionError::unavailable("OpenCode form option identity changed"))?;
      values.push(Value::String(value.into()));
    }
    if let Some(custom) = &answer.custom {
      values.push(Value::String(custom.clone()));
    }
    let value = if question.multiple {
      Value::Array(values)
    } else {
      values.pop().expect("validated nonempty answer")
    };
    let key = field["key"]
      .as_str()
      .ok_or_else(|| InteractionError::unavailable("OpenCode form field identity is unavailable"))?;
    body.insert(key.into(), value);
  }
  Ok(json!({"answer":body}))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn form(fields: Value) -> wire::current::FormRequest {
    serde_json::from_value(json!({"id":"frm_native","sessionID":"ses_native","title":"Questions","metadata":{"kind":"question"},"fields":fields})).unwrap()
  }

  #[test]
  fn question_fields_preserve_native_values_and_free_text_without_inventing_constraints() {
    let request = project(form(json!([
      {"key":"one","type":"string","description":"Choose","options":[{"label":"Readable","value":"opaque:value"}],"custom":true},
      {"key":"many","type":"multiselect","description":"Choose several","options":[{"label":"Alpha","value":"a"},{"label":"Beta","value":"b"}],"custom":true}
    ])));
    assert_eq!(request.kind, InteractionKind::Question);
    assert_eq!(
      answer_body(
        &request,
        vec![
          QuestionAnswer {
            question_id: "frm_native:one".into(),
            answers: vec!["Readable".into()],
            custom: None
          },
          QuestionAnswer {
            question_id: "frm_native:many".into(),
            answers: vec!["Beta".into()],
            custom: Some("Extra".into())
          },
        ]
      )
      .unwrap(),
      json!({"answer":{"one":"opaque:value","many":["b","Extra"]}})
    );
    assert!(answer_body(&request, vec![]).is_err());
  }

  #[test]
  fn unsupported_conditional_hidden_constrained_and_ambiguous_forms_remain_declineable() {
    for field in [
      json!({"key":"one","type":"number"}),
      json!({"key":"one","type":"string","hidden":true}),
      json!({"key":"one","type":"string","when":[{"key":"earlier","op":"eq","value":"yes"}]}),
      json!({"key":"one","type":"string","pattern":"[a-z]+"}),
      json!({"key":"one","type":"string","options":[{"label":"Same","value":"a"},{"label":"Same","value":"b"}]}),
    ] {
      let request = project(form(json!([field])));
      assert_eq!(request.kind, InteractionKind::Editor);
      assert!(request.questions.is_empty());
      assert!(answer_body(&request, vec![]).is_err());
      assert!(request.native["fields"].is_array());
    }
  }
}
