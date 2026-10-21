//! Model context is independent of work folds and retained-history coverage.
use super::*;
use crate::{
  model::ModelChangeSummary,
  models::{ModelIdentity, ModelStateSource, SessionModelState, valid_identity},
};
use tokn_session_core::{ModelObservation, ModelOrigin};

pub(super) struct ModelProjection {
  pub state: SessionModelState,
  changes: HashMap<usize, ModelChangeSummary>,
}

impl ModelProjection {
  pub fn new(events: &[AgentEvent], baseline: Option<&ModelObservation>) -> Self {
    let mut current = None;
    let mut catalog = None;
    let mut default_provider = None;
    let mut changes = HashMap::new();
    let mut configured = false;
    if let Some(baseline) = baseline {
      let model = recorded_identity(&baseline.model.model_provider, &baseline.model.model_id);
      if baseline.origin == ModelOrigin::Catalog {
        catalog = model;
      } else {
        current = model;
      }
      configured = current.is_some() && matches!(baseline.origin, ModelOrigin::Selection | ModelOrigin::Request);
    }
    for (index, event) in events.iter().enumerate() {
      if event.is_hidden() {
        continue;
      }
      let (provider, id, origin) = match event {
        AgentEvent::ProviderChanged(event) => (
          &event.model_provider,
          &event.model_id,
          event.model_origin.unwrap_or(ModelOrigin::Selection),
        ),
        AgentEvent::SessionSettingsApplied(event) => (&event.model_provider, &event.model_id, ModelOrigin::Selection),
        _ => continue,
      };
      let Some(id) = id else {
        if current.is_none()
          && let Some(provider) = provider.as_ref().filter(|value| valid_identity(value))
        {
          default_provider = Some(provider.clone());
        }
        continue;
      };
      let Some(mut next) = recorded_identity(provider, id) else {
        continue;
      };
      if origin == ModelOrigin::Catalog {
        catalog = Some(next);
        continue;
      }
      // A backend response can report a resolved alias or fallback. Once the
      // configured route is known, it must not appear to switch that route on
      // every response. Response-only histories still retain useful context.
      if origin == ModelOrigin::Response && configured {
        continue;
      }
      let next_configured = matches!(origin, ModelOrigin::Selection | ModelOrigin::Request);
      if next.model_provider.is_none() && current.is_none() {
        next.model_provider = default_provider.clone();
      }
      if let Some(previous) = &current {
        if same_model(previous, &next) {
          // An omitted provider enriches neither identity nor authority. Keep
          // an already known qualifier when the next record omits it.
          if next.model_provider.is_none() {
            next.model_provider = previous.model_provider.clone();
          }
        } else if configured || !next_configured {
          changes.insert(
            index,
            ModelChangeSummary {
              previous: previous.clone(),
              current: next.clone(),
            },
          );
        }
      }
      current = Some(next);
      configured |= next_configured;
    }
    let source = if current.is_some() {
      ModelStateSource::Recorded
    } else if catalog.is_some() {
      ModelStateSource::Catalog
    } else {
      ModelStateSource::Unknown
    };
    Self {
      state: SessionModelState::new(current.or(catalog), source, ""),
      changes,
    }
  }

  pub fn is_change(&self, index: usize) -> bool {
    self.changes.contains_key(&index)
  }

  pub fn summary(&self, mut summary: EventSummary) -> EventSummary {
    if let Ok(index) = decode_event_key(&summary.event_key)
      && let Some(change) = self.changes.get(&index)
    {
      summary.event_type = "model_changed".into();
      summary.title = "Model changed".into();
      summary.summary = if change.previous.model_id == change.current.model_id {
        format!(
          "{}/{} → {}/{}",
          change.previous.model_provider.as_deref().unwrap_or("unknown"),
          change.previous.model_id,
          change.current.model_provider.as_deref().unwrap_or("unknown"),
          change.current.model_id
        )
      } else {
        format!("{} → {}", change.previous.model_id, change.current.model_id)
      };
      summary.is_bookkeeping = false;
      summary.model_change = Some(change.clone());
    }
    summary
  }
}

fn recorded_identity(provider: &Option<String>, id: &str) -> Option<ModelIdentity> {
  if !valid_identity(id) {
    return None;
  }
  Some(ModelIdentity {
    model_provider: provider.as_ref().filter(|value| valid_identity(value)).cloned(),
    model_id: id.to_owned(),
    display_name: None,
  })
}

fn same_model(left: &ModelIdentity, right: &ModelIdentity) -> bool {
  left.model_id == right.model_id
    && (left.model_provider == right.model_provider || left.model_provider.is_none() || right.model_provider.is_none())
}

#[cfg(test)]
mod tests {
  use super::super::tests::{settings_event, tool_call};
  use super::*;
  use tokn_session_core::{ModelIdentity as RecordedModelIdentity, ProviderChanged};

  fn fact(provider: Option<&str>, id: &str, origin: ModelOrigin) -> AgentEvent {
    AgentEvent::ProviderChanged(ProviderChanged {
      provider: Provider::Codex,
      session_id: Some("fixture".into()),
      native_id: None,
      native_parent_id: None,
      model_provider: provider.map(str::to_owned),
      model_id: Some(id.into()),
      model_origin: Some(origin),
      thinking_level: None,
      timestamp: None,
    })
  }

  fn baseline(provider: Option<&str>, id: &str, origin: ModelOrigin) -> ModelObservation {
    ModelObservation {
      model: RecordedModelIdentity {
        model_provider: provider.map(str::to_owned),
        model_id: id.into(),
      },
      origin,
    }
  }

  #[test]
  fn catalog_is_only_a_fallback_and_equal_facts_do_not_create_separators() {
    let events = [
      fact(Some("catalog"), "latest", ModelOrigin::Catalog),
      fact(Some("first"), "same", ModelOrigin::Request),
      fact(Some("first"), "same", ModelOrigin::Selection),
      fact(Some("second"), "same", ModelOrigin::Request),
      fact(Some("first"), "ignored", ModelOrigin::Catalog),
    ];
    let projection = ModelProjection::new(&events, None);
    assert_eq!(projection.changes.len(), 1);
    assert!(projection.is_change(3));
    let current = projection.state.model.unwrap();
    assert_eq!(current.model_provider.as_deref(), Some("second"));
    assert_eq!(current.model_id, "same");
    assert_eq!(projection.state.source, ModelStateSource::Recorded);
    let fallback = ModelProjection::new(&events[..1], None);
    assert_eq!(fallback.state.source, ModelStateSource::Catalog);
    assert!(fallback.changes.is_empty());
  }

  #[test]
  fn missing_provider_enriches_same_model_without_qualifying_a_different_model() {
    let events = [
      fact(Some("first"), "same", ModelOrigin::Request),
      fact(None, "same", ModelOrigin::Request),
      fact(None, "different", ModelOrigin::Request),
    ];
    let unchanged = ModelProjection::new(&events[..2], None);
    assert_eq!(unchanged.state.model.unwrap().model_provider.as_deref(), Some("first"));
    assert!(unchanged.changes.is_empty());
    let changed = ModelProjection::new(&events, None);
    assert!(changed.is_change(2));
    assert!(changed.state.model.unwrap().model_provider.is_none());
  }

  #[test]
  fn response_aliases_never_overwrite_a_known_configured_route() {
    let events = [
      fact(Some("vendor"), "backend", ModelOrigin::Response),
      fact(Some("vendor"), "configured", ModelOrigin::Request),
      fact(Some("vendor"), "backend-alias", ModelOrigin::Response),
      fact(Some("vendor"), "configured", ModelOrigin::Request),
      fact(Some("vendor"), "new-configured", ModelOrigin::Selection),
    ];
    let fallback = ModelProjection::new(&events[..1], None);
    assert_eq!(fallback.state.model.unwrap().model_id, "backend");
    let projection = ModelProjection::new(&events, None);
    assert_eq!(projection.changes.len(), 1);
    assert!(projection.is_change(4));
    assert_eq!(projection.state.model.unwrap().model_id, "new-configured");
  }

  #[test]
  fn baseline_changes_keep_source_keys_and_trajectory_lookup_boundaries() {
    let before = tool_call(
      Provider::Codex,
      "exec_command",
      "before",
      ToolKind::Shell,
      None,
      Phase::Finished,
      None,
    );
    let after = tool_call(
      Provider::Codex,
      "exec_command",
      "after",
      ToolKind::Shell,
      None,
      Phase::Finished,
      None,
    );
    let mut settings = settings_event();
    if let AgentEvent::SessionSettingsApplied(event) = &mut settings {
      event.model_provider = Some("vendor".into());
      event.model_id = Some("next".into());
    }
    let events = [before, settings, after];
    let baseline = baseline(Some("vendor"), "prior", ModelOrigin::Selection);
    let models = ModelProjection::new(&events, Some(&baseline));
    assert!(models.is_change(1));
    let summary = models.summary(event_summary(&events, 1, &events[1]));
    assert_eq!(summary.event_key, encode_event_key(1));
    assert_eq!(summary.event_type, "model_changed");
    assert!(!summary.is_bookkeeping);
    let group = trajectory_for_start_with_models(&events, 0, &models).unwrap();
    assert_eq!(trajectory_source_event_indices(&group), vec![0]);
    let later = trajectory_for_start_with_models(&events, 2, &models).unwrap();
    assert_eq!(trajectory_source_event_indices(&later), vec![2]);
  }
}
