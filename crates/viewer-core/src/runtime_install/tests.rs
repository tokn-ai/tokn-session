use super::*;
use std::fs;

fn fixture(root: &Path, provider: Provider) -> InstallRecord {
  let directory = root.join(provider_name(provider));
  fs::create_dir_all(directory.join("bin")).unwrap();
  fs::write(directory.join("bin/agent"), b"fixture").unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(directory.join("bin/agent"), fs::Permissions::from_mode(0o755)).unwrap();
  }
  let record = InstallRecord {
    provider,
    version: "1.0.0".into(),
    generation: uuid::Uuid::new_v4().to_string(),
    entrypoint: "bin/agent".into(),
    node: false,
    digest: "sha256:fixture".into(),
  };
  filesystem::write_record(&directory, &record).unwrap();
  record
}
fn job(provider: Provider, state: &str) -> RuntimeInstallJob {
  RuntimeInstallJob {
    job_id: uuid::Uuid::new_v4().to_string(),
    request_id: uuid::Uuid::new_v4().to_string(),
    provider,
    state: state.into(),
    downloaded_bytes: 0,
    total_bytes: None,
    error: None,
  }
}
#[test]
fn software_use_lease_blocks_update_and_stale_generation_rejects() {
  let root = tempfile::tempdir().unwrap();
  let record = fixture(root.path(), Provider::Codex);
  let lease = acquire_use(root.path(), Provider::Codex, &record.generation).unwrap();
  assert!(exclusive_lock(root.path(), Provider::Codex).is_err());
  assert!(acquire_use(root.path(), Provider::Codex, &uuid::Uuid::new_v4().to_string()).is_err());
  drop(lease);
  let update = exclusive_lock(root.path(), Provider::Codex).unwrap();
  assert!(acquire_use(root.path(), Provider::Codex, &record.generation).is_err());
  drop(update);
}
#[test]
fn failed_replacement_preserves_previous_install_and_crash_recovers_on_discovery() {
  let root = tempfile::tempdir().unwrap();
  let old = fixture(root.path(), Provider::Codex);
  assert!(replace(root.path(), Provider::Codex, &root.path().join("missing-stage")).is_err());
  assert_eq!(
    read_record(root.path(), Provider::Codex).unwrap().unwrap().generation,
    old.generation
  );
  fs::rename(root.path().join("codex"), root.path().join(".codex-previous")).unwrap();
  let recovered = launch_spec(root.path(), Provider::Codex).unwrap().unwrap();
  assert_eq!(recovered.generation, old.generation);
  assert!(!root.path().join(".codex-previous").exists());
}
#[test]
fn activation_replaces_one_installation_and_invalidates_old_generation() {
  let root = tempfile::tempdir().unwrap();
  let old = fixture(root.path(), Provider::Codex);
  let stage_root = root.path().join("staging");
  let new = fixture(&stage_root, Provider::Codex);
  replace(root.path(), Provider::Codex, &stage_root.join("codex")).unwrap();
  assert!(acquire_use(root.path(), Provider::Codex, &old.generation).is_err());
  assert_eq!(
    launch_spec(root.path(), Provider::Codex).unwrap().unwrap().generation,
    new.generation
  );
  assert!(!root.path().join(".codex-previous").exists());
}
#[cfg(unix)]
#[test]
fn dangling_installation_symlink_is_an_error_not_missing_software() {
  let root = tempfile::tempdir().unwrap();
  std::os::unix::fs::symlink(root.path().join("absent"), root.path().join("codex")).unwrap();
  assert!(launch_spec(root.path(), Provider::Codex).is_err());
}
#[tokio::test]
async fn request_receipt_survives_removal_and_retries_do_not_reinstall() {
  let manager = RuntimeInstallManager::temporary();
  let record = fixture(manager.root(), Provider::Codex);
  let completed = job(Provider::Codex, "completed");
  manager.0.jobs.lock().unwrap().insert(
    completed.job_id.clone(),
    JobEntry {
      job: completed.clone(),
      cancel: CancellationToken::new(),
    },
  );
  manager
    .0
    .latest
    .lock()
    .unwrap()
    .insert("codex".into(), completed.job_id.clone());
  manager
    .remove(RuntimeInstallRemoveRequest {
      provider: Provider::Codex,
      generation: record.generation,
    })
    .await
    .unwrap();
  let retry = manager
    .start(RuntimeInstallRequest {
      request_id: completed.request_id,
      provider: Provider::Codex,
    })
    .await
    .unwrap();
  assert_eq!(retry.job_id, completed.job_id);
  assert!(launch_spec(manager.root(), Provider::Codex).unwrap().is_none());
}
#[tokio::test]
async fn inventory_uses_latest_pointer_and_cancel_recovers_from_ui_navigation() {
  let manager = RuntimeInstallManager::temporary();
  let mut older = job(Provider::Codex, "failed");
  older.job_id = "ffffffff-ffff-4fff-8fff-ffffffffffff".into();
  let mut latest = job(Provider::Codex, "installing");
  latest.job_id = "00000000-0000-4000-8000-000000000001".into();
  let token = CancellationToken::new();
  {
    let mut jobs = manager.0.jobs.lock().unwrap();
    jobs.insert(
      older.job_id.clone(),
      JobEntry {
        job: older,
        cancel: CancellationToken::new(),
      },
    );
    jobs.insert(
      latest.job_id.clone(),
      JobEntry {
        job: latest.clone(),
        cancel: token.clone(),
      },
    );
  }
  manager
    .0
    .latest
    .lock()
    .unwrap()
    .insert("codex".into(), latest.job_id.clone());
  assert_eq!(
    manager
      .inventory()
      .await
      .unwrap()
      .agents
      .iter()
      .find(|a| a.provider == Provider::Codex)
      .unwrap()
      .job
      .as_ref()
      .unwrap()
      .job_id,
    latest.job_id
  );
  manager
    .cancel(RuntimeInstallCancelRequest { job_id: latest.job_id })
    .await
    .unwrap();
  assert!(token.is_cancelled());
  manager.shutdown().await;
  assert!(
    manager
      .start(RuntimeInstallRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        provider: Provider::Codex
      })
      .await
      .is_err()
  );
}
#[tokio::test]
async fn bad_request_and_provider_reuse_reject_before_filesystem_or_network() {
  let manager = RuntimeInstallManager::temporary();
  assert!(
    manager
      .start(RuntimeInstallRequest {
        request_id: "invalid".into(),
        provider: Provider::Codex
      })
      .await
      .is_err()
  );
  assert!(!manager.root().exists());
  let previous = job(Provider::Codex, "completed");
  manager.0.jobs.lock().unwrap().insert(
    previous.job_id.clone(),
    JobEntry {
      job: previous.clone(),
      cancel: CancellationToken::new(),
    },
  );
  assert!(
    manager
      .start(RuntimeInstallRequest {
        request_id: previous.request_id,
        provider: Provider::Dsh
      })
      .await
      .is_err()
  );
}

#[test]
fn discovery_during_update_rejects_instead_of_choosing_another_installation() {
  let root = tempfile::tempdir().unwrap();
  fixture(root.path(), Provider::Codex);
  let _update = exclusive_lock(root.path(), Provider::Codex).unwrap();
  assert!(launch_spec(root.path(), Provider::Codex).is_err());
  fs::rename(root.path().join("codex"), root.path().join(".codex-previous")).unwrap();
  assert!(launch_spec(root.path(), Provider::Codex).is_err());
}
