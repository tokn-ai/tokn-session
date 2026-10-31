use super::*;
use tokn_interaction_core::runtime_plan::{RuntimeReuseScope, RuntimeStrategy};

fn executable(directory: &Path, name: &str) -> PathBuf {
  let path = directory.join(if cfg!(windows) {
    format!("{name}.exe")
  } else {
    name.into()
  });
  std::fs::write(&path, b"fixture executable; resolution must not run this file").unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
  }
  path
}

fn install_environment(manager: &RpcRuntimeManager, bin: &Path) {
  manager.set_discovery_environment(discovery::DiscoveryEnvironment {
    path: Some(std::env::join_paths([bin]).unwrap()),
    executables: BTreeMap::new(),
  });
}

#[tokio::test]
async fn all_shared_providers_discover_the_same_plan_for_availability_catalog_and_startup_policy() {
  let manager = manager();
  let bin = tempfile::tempdir().unwrap();
  let project = tempfile::tempdir().unwrap();
  let cwd = std::fs::canonicalize(project.path()).unwrap();
  for command in ["codex", "opencode", "dsh"] {
    executable(bin.path(), command);
  }
  install_environment(&manager, bin.path());
  let host = Host {
    resolver: manager.0.resolver.clone(),
  };
  for provider in [Provider::Codex, Provider::OpenCode, Provider::Dsh] {
    let plan = manager.resolve_plan(provider, &cwd, None).unwrap();
    assert_eq!(plan.provider, provider);
    assert_eq!(plan.ownership, Ownership::Managed);
    assert_eq!(plan.reuse_scope, RuntimeReuseScope::Project);
    assert_eq!(plan.strategy, RuntimeStrategy::SharedRuntime);
    assert_eq!(plan.working_directory, cwd);
    let selection = plan.shared_runtime_selection().unwrap();
    assert_eq!(
      manager.creation_selection(provider, &cwd, Some(&selection)).unwrap(),
      Some(selection.clone())
    );
    assert!(
      manager.creation_selection(provider, &cwd, None).unwrap().is_none(),
      "legacy default receipts remain unpinned"
    );
    manager.activation_ready(provider, &cwd).await.unwrap();
    assert_eq!(
      core_runtime::RuntimeHost::select_profile(&host, provider, &cwd)
        .await
        .unwrap(),
      (selection.profile_id.clone(), selection.expected_revision)
    );
    assert_eq!(manager.resolve_plan(provider, &cwd, Some(&selection)).unwrap(), plan);
  }
  assert!(manager.resolve_plan(Provider::Pi, &cwd, None).is_err());
  assert!(manager.inventory().runtimes.is_empty());
  assert!(manager.0.interactions.snapshots().is_empty());
}

#[test]
fn discovered_identity_fences_provider_project_executable_and_replacement() {
  let manager = manager();
  let bin = tempfile::tempdir().unwrap();
  let exe = executable(bin.path(), "agent");
  let project = tempfile::tempdir().unwrap();
  let other = tempfile::tempdir().unwrap();
  let mut environment = discovery::DiscoveryEnvironment {
    path: None,
    executables: [
      "TOKN_SESSION_CODEX_EXECUTABLE",
      "TOKN_SESSION_OPENCODE_EXECUTABLE",
      "TOKN_SESSION_DSH_EXECUTABLE",
    ]
    .into_iter()
    .map(|key| (key.to_owned(), exe.clone().into_os_string()))
    .collect(),
  };
  manager.set_discovery_environment(environment.clone());
  let first = manager.resolve_plan(Provider::Codex, project.path(), None).unwrap();
  let alias = manager
    .resolve_plan(Provider::Codex, &project.path().join("."), None)
    .unwrap();
  assert_eq!(first.configuration, alias.configuration);
  assert_ne!(
    first.configuration,
    manager
      .resolve_plan(Provider::Dsh, project.path(), None)
      .unwrap()
      .configuration
  );
  assert_ne!(
    first.configuration,
    manager
      .resolve_plan(Provider::Codex, other.path(), None)
      .unwrap()
      .configuration
  );
  let selected = first.shared_runtime_selection().unwrap();
  assert!(
    manager
      .resolve_plan(Provider::Codex, other.path(), Some(&selected))
      .is_err()
  );
  std::fs::write(&exe, b"changed executable bytes with different length").unwrap();
  assert!(
    manager
      .resolve_plan(Provider::Codex, project.path(), Some(&selected))
      .unwrap_err()
      .contains("changed")
  );
  let updated = manager.resolve_plan(Provider::Codex, project.path(), None).unwrap();
  assert_ne!(first.configuration, updated.configuration);
  environment.executables.insert(
    "TOKN_SESSION_CODEX_EXECUTABLE".into(),
    executable(bin.path(), "another").into_os_string(),
  );
  manager.set_discovery_environment(environment);
  assert_ne!(
    updated.configuration,
    manager
      .resolve_plan(Provider::Codex, project.path(), None)
      .unwrap()
      .configuration
  );
}

#[test]
fn explicit_or_invalid_saved_profiles_never_fall_back_to_discovery() {
  let manager = manager();
  let bin = tempfile::tempdir().unwrap();
  executable(bin.path(), "codex");
  install_environment(&manager, bin.path());
  let project = tempfile::tempdir().unwrap();
  let saved = save_profile(&manager, attached_profile("ws://127.0.0.1:1".into()));
  let plan = manager.resolve_plan(Provider::Codex, project.path(), None).unwrap();
  assert_eq!(plan.configuration.id, saved.profile_id);
  assert_eq!(plan.ownership, Ownership::Attached);
  assert_eq!(plan.reuse_scope, RuntimeReuseScope::Runtime);
  let selected = plan.shared_runtime_selection().unwrap();
  assert!(
    manager
      .resolve_plan(Provider::Dsh, project.path(), Some(&selected))
      .is_err()
  );
  let mut stale = selected.clone();
  stale.expected_revision += 1;
  assert!(
    manager
      .resolve_plan(Provider::Codex, project.path(), Some(&stale))
      .is_err()
  );
  let mut invalid = attached_profile("ws://127.0.0.1:1".into());
  invalid.mode = RpcRuntimeMode::Managed;
  invalid.endpoint = None;
  invalid.executable = Some(bin.path().join("missing").to_string_lossy().into_owned());
  invalid.working_directory = Some(project.path().to_string_lossy().into_owned());
  let broken = save_profile(&manager, invalid);
  assert!(
    manager
      .resolve_plan(
        Provider::Codex,
        project.path(),
        Some(&core_runtime::RuntimeSelection {
          profile_id: broken.profile_id,
          expected_revision: broken.expected_revision
        })
      )
      .is_err()
  );
  assert!(
    manager.resolve_plan(Provider::Codex, project.path(), None).is_err(),
    "invalid configured default must not silently launch discovered binary"
  );
  assert!(manager.inventory().runtimes.is_empty());
}

#[test]
fn opencode_scope_allows_another_projects_local_default_but_rejects_explicit_mismatch() {
  let manager = manager();
  let bin = tempfile::tempdir().unwrap();
  executable(bin.path(), "opencode");
  install_environment(&manager, bin.path());
  let project = tempfile::tempdir().unwrap();
  let other = tempfile::tempdir().unwrap();
  let saved = save_profile(
    &manager,
    RpcRuntimeProfileInput {
      provider: Provider::OpenCode,
      working_directory: Some(project.path().to_string_lossy().into_owned()),
      endpoint: Some("http://127.0.0.1:1".into()),
      ..attached_profile("unused".into())
    },
  );
  let explicit = core_runtime::RuntimeSelection {
    profile_id: saved.profile_id,
    expected_revision: saved.expected_revision,
  };
  assert_eq!(
    manager
      .resolve_plan(Provider::OpenCode, project.path(), Some(&explicit))
      .unwrap()
      .reuse_scope,
    RuntimeReuseScope::Project
  );
  assert!(
    manager
      .resolve_plan(Provider::OpenCode, other.path(), Some(&explicit))
      .is_err()
  );
  assert_eq!(
    manager
      .resolve_plan(Provider::OpenCode, other.path(), None)
      .unwrap()
      .ownership,
    Ownership::Managed
  );
  assert!(manager.inventory().runtimes.is_empty());
}

#[test]
fn invalid_explicit_executable_override_does_not_search_for_a_different_binary() {
  let bin = tempfile::tempdir().unwrap();
  let valid = executable(bin.path(), "dsh");
  let path = std::env::join_paths([bin.path()]).unwrap();
  let missing = bin.path().join("missing");
  assert!(discovery::resolve_executable(Some(missing.as_os_str()), Some(&path), "dsh").is_err());
  assert_eq!(
    discovery::resolve_executable(None, Some(&path), "dsh").unwrap(),
    std::fs::canonicalize(valid).unwrap()
  );
}

#[cfg(windows)]
#[test]
fn windows_bare_commands_find_native_binaries_and_package_manager_shims() {
  let bin = tempfile::tempdir().unwrap();
  let path = std::env::join_paths([bin.path()]).unwrap();
  for (command, extension) in [("pi", "exe"), ("codex", "cmd"), ("opencode", "bat"), ("dsh", "com")] {
    let executable = bin.path().join(command).with_extension(extension);
    std::fs::write(&executable, b"metadata fixture; never launch").unwrap();
    let canonical = std::fs::canonicalize(&executable).unwrap();
    assert_eq!(
      discovery::resolve_executable(Some(command.as_ref()), Some(&path), command).unwrap(),
      canonical
    );
    assert_eq!(
      discovery::resolve_executable(None, Some(&path), command).unwrap(),
      canonical
    );
    assert_eq!(
      discovery::resolve_executable(Some(bin.path().join(command).as_os_str()), None, command).unwrap(),
      canonical
    );
    let wrong_extension = bin.path().join(command).with_extension("missing");
    assert!(discovery::resolve_executable(Some(wrong_extension.as_os_str()), Some(&path), command).is_err());
  }
}
