//! One configuration-only entry point while existing execution strategies migrate.
use super::*;
use std::path::Path;
use tokn_interaction_core::{
  runtime::RuntimeSelection,
  runtime_plan::{RuntimePlan, RuntimeStrategy},
};

impl SessionSupervisor {
  pub(crate) fn resolve_runtime_plan(
    &self,
    provider: Provider,
    cwd: &Path,
    selected: Option<&RuntimeSelection>,
  ) -> Result<RuntimePlan, String> {
    if provider == Provider::Pi {
      if selected.is_some() {
        return Err("Pi uses its managed-session configuration; choose Automatic".into());
      }
      return self.managed.runtime_plan(cwd);
    }
    self.runtimes.resolve_plan(provider, cwd, selected)
  }

  pub(crate) fn resolve_creation_plan(
    &self,
    provider: Provider,
    cwd: &Path,
    selected: Option<&RuntimeSelection>,
  ) -> Result<RuntimePlan, String> {
    let plan = self.resolve_runtime_plan(provider, cwd, selected)?;
    if plan.strategy == RuntimeStrategy::SharedRuntime {
      self
        .runtimes
        .creation_selection(provider, cwd, Some(&plan.shared_runtime_selection()?))?;
    }
    Ok(plan)
  }
}
