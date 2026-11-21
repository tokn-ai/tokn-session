//! Standard ACP session configuration projected into the interaction model catalog.
use super::*;

#[derive(Clone)]
pub(super) enum Route {
  Config { config_id: String },
  QoderLegacy,
  Unavailable,
}
pub(super) struct Catalog {
  pub current: Option<ModelIdentity>,
  pub choices: Vec<ModelChoice>,
  pub route: Route,
}
impl Default for Catalog {
  fn default() -> Self {
    Self {
      current: None,
      choices: Vec::new(),
      route: Route::Unavailable,
    }
  }
}

pub(super) fn from_config(
  options: Option<&[acp::SessionConfigOption]>,
  selected_config: Option<&str>,
) -> Result<Option<Catalog>, InteractionError> {
  let Some(options) = options else {
    return Ok(None);
  };
  let Some(option) = options.iter().find(|option| {
    selected_config.map_or_else(|| option.category.as_deref() == Some("model"), |id| option.id == id)
      && matches!(&option.value, acp::SessionConfigValue::Select { .. })
  }) else {
    return Ok(None);
  };
  if option.id.is_empty() || option.id.len() > 1024 {
    return Err(invalid_catalog());
  }
  let acp::SessionConfigValue::Select { current_value, options } = &option.value else {
    unreachable!()
  };
  let choices: Vec<_> = match options {
    acp::ConfigSelectOptions::Flat(options) => options.iter().collect(),
    acp::ConfigSelectOptions::Grouped(groups) => groups.iter().flat_map(|group| group.options.iter()).collect(),
  };
  let choices = choices
    .into_iter()
    .map(|choice| (choice.value.clone(), choice.name.clone()))
    .collect();
  catalog(
    current_value,
    choices,
    Route::Config {
      config_id: option.id.clone(),
    },
  )
  .map(Some)
}

pub(super) fn catalog(
  current: &str,
  choices: Vec<(String, String)>,
  route: Route,
) -> Result<Catalog, InteractionError> {
  if choices.len() > 512 || current.len() > 1024 {
    return Err(invalid_catalog());
  }
  let mut models = Vec::with_capacity(choices.len());
  let mut current_model = None;
  for (id, name) in choices {
    if id.is_empty() || id.len() > 1024 || name.len() > 4096 {
      return Err(invalid_catalog());
    }
    let model = ModelIdentity {
      model_provider: None,
      model_id: id,
      display_name: Some(name),
    };
    if models
      .iter()
      .any(|choice: &ModelChoice| choice.model.model_id == model.model_id)
    {
      return Err(invalid_catalog());
    }
    if model.model_id == current {
      current_model = Some(model.clone());
    }
    models.push(ModelChoice {
      model,
      supports_images: false,
    });
  }
  if current_model.is_none() && !current.is_empty() {
    current_model = Some(ModelIdentity {
      model_provider: None,
      model_id: current.into(),
      display_name: None,
    });
  }
  Ok(Catalog {
    current: current_model,
    choices: models,
    route,
  })
}
fn invalid_catalog() -> InteractionError {
  InteractionError::unavailable("Qoder returned an invalid model catalog")
}
