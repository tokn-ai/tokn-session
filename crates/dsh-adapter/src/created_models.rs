//! One-time model preparation for sessions created by this exact adapter.
//! Native selectModel also updates DSH's default; inspection never invokes it.
use crate::*;
use tokio::sync::OwnedMutexGuard;

const MAX_CREATED_SESSIONS: usize = 4096;

pub(crate) struct CreatedControl {
  preparation: Arc<Mutex<Preparation>>,
}

pub(crate) struct Preparation {
  eligible: bool,
  prompt_attempted: bool,
  model: Option<ModelIdentity>,
  result: Option<Result<(), InteractionError>>,
}

pub(crate) async fn register(inner: &Inner, session_id: &str, eligible: bool) {
  let mut created = inner.created.lock().await;
  // Default creation remains usable at the tracking limit. Model preparation
  // fails closed for an untracked identity instead of evicting an old receipt.
  if created.contains_key(session_id) || created.len() >= MAX_CREATED_SESSIONS {
    return;
  }
  created.insert(
    session_id.into(),
    Arc::new(CreatedControl {
      preparation: Arc::new(Mutex::new(Preparation {
        eligible,
        prompt_attempted: false,
        model: None,
        result: None,
      })),
    }),
  );
}

/// Rejects prompts while preparation is pending and permanently retires its
/// eligibility before native message delivery can be attempted.
pub(crate) async fn close_preparation(
  inner: &Inner,
  session_id: &str,
) -> Result<Option<OwnedMutexGuard<Preparation>>, InteractionError> {
  let Some(control) = inner.created.lock().await.get(session_id).cloned() else {
    return Ok(None);
  };
  let mut preparation = control.preparation.clone().try_lock_owned().map_err(|_| {
    InteractionError::new(
      ErrorKind::Conflict,
      "DSH model preparation is still pending; the prompt was not sent",
    )
  })?;
  preparation.prompt_attempted = true;
  Ok(Some(preparation))
}

impl DshAdapter {
  /// Apply an explicit creation choice exactly once before this new session's
  /// first prompt. Native semantics also update DSH's default for new sessions.
  /// Ordinary message/steer input never calls this method.
  pub async fn prepare_created_model(&self, session_id: &str, model: &ModelIdentity) -> Result<(), InteractionError> {
    self.inner.ensure_live()?;
    validate_model(model)?;
    let control = self
      .inner
      .created
      .lock()
      .await
      .get(session_id)
      .cloned()
      .ok_or_else(|| {
        InteractionError::unsupported("DSH model preparation requires a session created by this runtime")
      })?;
    let inner = self.inner.clone();
    let session_id = session_id.to_owned();
    let model = ModelIdentity {
      model_provider: model.model_provider.clone(),
      model_id: model.model_id.clone(),
      display_name: None,
    };
    // Cancellation of a caller cannot release the preparation admission while
    // its native mutation is still in flight or allow a second setter attempt.
    tokio::spawn(async move {
      let mut preparation = control.preparation.lock().await;
      if let Some(previous) = &preparation.model {
        if !same_model(previous, &model) {
          return Err(InteractionError::new(
            ErrorKind::Conflict,
            "The created DSH session already has another model preparation",
          ));
        }
        return preparation.result.clone().unwrap_or_else(|| {
          Err(InteractionError::uncertain(
            "DSH model preparation is unresolved; it was not retried",
          ))
        });
      }
      if !preparation.eligible || preparation.prompt_attempted {
        return Err(InteractionError::unsupported(
          "DSH model preparation requires a verified blank idle session before its first prompt",
        ));
      }
      preparation.model = Some(model.clone());
      preparation.result = Some(Err(InteractionError::uncertain(
        "DSH model preparation did not finish; it was not retried",
      )));
      let result = prepare(&inner, &session_id, &model).await;
      preparation.result = Some(result.clone());
      result
    })
    .await
    .map_err(|_| InteractionError::uncertain("DSH model preparation was interrupted; it was not retried"))?
  }
}

fn validate_model(model: &ModelIdentity) -> Result<(), InteractionError> {
  let valid = |value: &str| {
    !value.is_empty() && value.trim() == value && value.len() <= 512 && !value.chars().any(char::is_control)
  };
  if !valid(&model.model_id) || model.model_provider.as_deref().is_none_or(|provider| !valid(provider)) {
    return Err(InteractionError::invalid(
      "DSH model selection requires valid provider and model identities",
    ));
  }
  Ok(())
}

fn same_model(left: &ModelIdentity, right: &ModelIdentity) -> bool {
  left.model_id == right.model_id && left.model_provider == right.model_provider
}

fn matches_selection(selected: &api::ModelSelection, requested: &ModelIdentity) -> bool {
  requested.model_provider.as_deref() == Some(selected.provider.as_str()) && requested.model_id == selected.model
}

async fn prepare(inner: &Inner, session_id: &str, model: &ModelIdentity) -> Result<(), InteractionError> {
  inner.ensure_live()?;
  let observation = inner
    .state
    .lock()
    .await
    .model_observations
    .get(session_id)
    .copied()
    .unwrap_or(0);
  let catalog: api::ModelCatalog = inner
    .transport
    .call(&uuid::Uuid::new_v4().to_string(), "llm.models", json!({}), false)
    .await
    .map_err(|_| InteractionError::unavailable("Could not verify the configured DSH model before selection"))?;
  let matches = catalog
    .groups
    .iter()
    .filter(|group| Some(group.id.as_str()) == model.model_provider.as_deref())
    .flat_map(|group| group.models.iter())
    .filter(|choice| choice.id == model.model_id)
    .count();
  if matches != 1 {
    return Err(InteractionError::invalid(
      "The selected DSH model is no longer uniquely available; refresh the draft model choice",
    ));
  }
  let sessions: api::SessionList = inner
    .transport
    .call(&uuid::Uuid::new_v4().to_string(), "session.list", json!({}), false)
    .await
    .map_err(|_| InteractionError::unavailable("Could not verify the new DSH session before model selection"))?;
  let mut matches = sessions.items.iter().filter(|entry| entry.session_id == session_id);
  let session = matches
    .next()
    .ok_or_else(|| InteractionError::unavailable("The new DSH session is unavailable"))?;
  if matches.next().is_some() || session.running || session.blank != Some(true) {
    return Err(InteractionError::new(
      ErrorKind::Conflict,
      "The new DSH session is no longer blank and idle; its model was not changed",
    ));
  }
  inner.ensure_live()?;
  {
    let state = inner.state.lock().await;
    if state.model_observations.get(session_id).copied().unwrap_or(0) != observation
      || state
        .sessions
        .get(session_id)
        .is_some_and(|state| matches!(state.status, ExecutionState::Running | ExecutionState::Stopping))
    {
      return Err(InteractionError::new(
        ErrorKind::Conflict,
        "DSH activity changed before model selection; its model was not changed",
      ));
    }
  }
  let selected: api::SessionSelectModelResponse = inner
    .transport
    .call(
      &uuid::Uuid::new_v4().to_string(),
      "session.selectModel",
      serde_json::to_value(api::SessionSelectModelRequest {
        session_id: session_id.into(),
        provider: model.model_provider.clone().expect("validated provider"),
        model: model.model_id.clone(),
      })
      .map_err(|_| InteractionError::invalid("Invalid DSH model selection request"))?,
      true,
    )
    .await
    .map_err(|error| {
      InteractionError::new(
        error.kind,
        "DSH could not confirm the selected model; its first prompt was not sent and selection was not retried",
      )
    })?;
  if !matches_selection(&selected.selected, model) {
    return Err(InteractionError::uncertain(
      "DSH returned another selected model; its first prompt was not sent",
    ));
  }
  inner
    .ensure_live()
    .map_err(|_| InteractionError::uncertain("DSH selected a model but its live connection was lost"))?;
  let confirmed: api::SessionModels = inner
    .transport
    .call(
      &uuid::Uuid::new_v4().to_string(),
      "session.models",
      json!({"sessionId":session_id}),
      false,
    )
    .await
    .map_err(|_| {
      InteractionError::uncertain(
        "DSH selected a model but its session model could not be verified; its first prompt was not sent",
      )
    })?;
  if !confirmed.routable || !matches_selection(&confirmed.current, model) {
    return Err(InteractionError::uncertain(
      "The new DSH session did not confirm the selected model; its first prompt was not sent",
    ));
  }
  inner
    .ensure_live()
    .map_err(|_| InteractionError::uncertain("DSH selected a model but its live connection was lost"))?;
  // Only a verified native read establishes this baseline. Later request/header
  // or request/context events retain the normal authority over actual state.
  let mut state = inner.state.lock().await;
  if state.model_observations.get(session_id).copied().unwrap_or(0) != observation
    || state
      .sessions
      .get(session_id)
      .is_some_and(|state| matches!(state.status, ExecutionState::Running | ExecutionState::Stopping))
  {
    return Err(InteractionError::uncertain(
      "DSH session model observations changed during preparation; its first prompt was not sent",
    ));
  }
  let current = state.sessions.entry(session_id.into()).or_insert_with(|| SessionState {
    session_id: Some(session_id.into()),
    status: ExecutionState::Idle,
    model: None,
    pending_inputs: None,
    revision: None,
  });
  current.model = Some(ModelIdentity {
    model_provider: Some(confirmed.current.provider),
    model_id: confirmed.current.model,
    display_name: None,
  });
  Ok(())
}
