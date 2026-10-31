use super::*;
use serde_json::json;

fn plan() -> RuntimePlan {
  RuntimePlan {
    provider: Provider::Codex,
    configuration: RuntimeConfigurationIdentity {
      id: "12345678-1234-4234-8234-123456789012".into(),
      revision: 0,
    },
    ownership: Ownership::Managed,
    reuse_scope: RuntimeReuseScope::Project,
    strategy: RuntimeStrategy::SharedRuntime,
    working_directory: std::env::temp_dir().join("runtime-plan-project"),
  }
}

fn descriptor(plan: &RuntimePlan) -> NativeSessionDescriptor {
  NativeSessionDescriptor {
    native_id: "opaque-native-id".into(),
    working_directory: plan.working_directory.clone(),
    // Native histories can live outside their working directory.
    history_path: Some(std::env::temp_dir().join("runtime-plan-history.jsonl")),
  }
}

#[test]
fn shared_selection_preserves_the_explicit_configuration_fence() {
  let mut plan = plan();
  let initial = plan.shared_runtime_selection().unwrap();
  assert_eq!(initial.profile_id, plan.configuration.id);
  assert_eq!(initial.expected_revision, 0);
  plan.configuration.revision = 39;
  assert_eq!(plan.shared_runtime_selection().unwrap().expected_revision, 39);
  plan.configuration.id = "factory-owned-opaque-identity".into();
  assert!(plan.validate().is_ok());
  assert!(plan.shared_runtime_selection().unwrap_err().contains("profile ID"));
}

#[test]
fn session_worker_constraints_do_not_depend_on_provider_name() {
  for provider in [Provider::Pi, Provider::Codex, Provider::OpenCode, Provider::Dsh] {
    let mut plan = plan();
    plan.provider = provider;
    plan.configuration.id = "opaque-worker-configuration".into();
    plan.strategy = RuntimeStrategy::SessionWorker;
    assert!(plan.validate().is_err(), "project scope cannot share a session worker");
    plan.reuse_scope = RuntimeReuseScope::Session;
    assert!(plan.validate().is_ok());
    assert!(plan.shared_runtime_selection().is_err());
    plan.ownership = Ownership::Attached;
    assert!(
      plan.validate().is_err(),
      "an owned worker cannot be an external runtime"
    );
    plan.strategy = RuntimeStrategy::SharedRuntime;
    for scope in [
      RuntimeReuseScope::Session,
      RuntimeReuseScope::Project,
      RuntimeReuseScope::Runtime,
    ] {
      plan.reuse_scope = scope;
      assert!(
        plan.validate().is_ok(),
        "shared factory policy determines its supported reuse scope"
      );
    }
  }
}

#[test]
fn malformed_plan_identity_and_paths_fail_before_activation() {
  for invalid in ["", " padded", "line\nbreak", &"x".repeat(MAX_IDENTITY_BYTES + 1)] {
    let mut plan = plan();
    plan.configuration.id = invalid.into();
    assert!(plan.validate().is_err());
    assert!(plan.fingerprint().is_err());
  }
  let mut plan = plan();
  plan.working_directory = "relative/project".into();
  assert!(plan.validate().is_err());
  plan.working_directory = std::env::temp_dir().join("invalid\0path");
  assert!(plan.validate().is_err());
  #[cfg(unix)]
  {
    use std::os::unix::ffi::OsStringExt;
    plan.working_directory = PathBuf::from(std::ffi::OsString::from_vec(b"/invalid/\xff".to_vec()));
    assert!(plan.validate().is_err());
  }
}

#[test]
fn resume_validation_preserves_provider_directory_and_file_evidence() {
  let plan = plan();
  let native = descriptor(&plan);
  let verified = VerifiedSessionDescriptor::from_verified_history(plan.provider, native.clone()).unwrap();
  assert_eq!(verified.provider(), plan.provider);
  assert_eq!(verified.native(), &native);
  assert_eq!(verified.history_path(), native.history_path.as_deref());
  assert!(plan.validate_resume(&verified).is_ok());
  let wrong_provider = VerifiedSessionDescriptor::from_verified_history(Provider::Pi, native.clone()).unwrap();
  assert!(
    plan
      .validate_resume(&wrong_provider)
      .unwrap_err()
      .contains("another provider")
  );
  let mut other = native;
  other.working_directory = plan.working_directory.join("different");
  let other = VerifiedSessionDescriptor::from_verified_history(plan.provider, other).unwrap();
  assert!(
    plan
      .validate_resume(&other)
      .unwrap_err()
      .contains("another working directory")
  );
}

#[test]
fn verification_requires_valid_structure_but_leaves_native_path_policy_to_the_factory() {
  let plan = plan();
  for provider in [Provider::Pi, Provider::Codex, Provider::OpenCode, Provider::Dsh] {
    let mut native = descriptor(&plan);
    native.history_path = None;
    assert!(VerifiedSessionDescriptor::from_verified_history(provider, native).is_ok());
  }
  for invalid in ["", " spaced ", "\0", &"x".repeat(MAX_IDENTITY_BYTES + 1)] {
    let mut native = descriptor(&plan);
    native.native_id = invalid.into();
    assert!(VerifiedSessionDescriptor::from_verified_history(plan.provider, native).is_err());
  }
  let mut native = descriptor(&plan);
  native.working_directory = "relative".into();
  assert!(VerifiedSessionDescriptor::from_verified_history(plan.provider, native).is_err());
  let mut native = descriptor(&plan);
  native.history_path = Some("relative.jsonl".into());
  assert!(VerifiedSessionDescriptor::from_verified_history(plan.provider, native).is_err());
}

#[test]
fn plan_wire_values_round_trip_without_launch_details() {
  let plan = plan();
  let encoded = serde_json::to_value(&plan).unwrap();
  assert_eq!(encoded["reuse_scope"], "project");
  assert_eq!(encoded["strategy"], "shared_runtime");
  assert_eq!(
    encoded["configuration"],
    json!({"id":plan.configuration.id,"revision":0})
  );
  assert_eq!(encoded.as_object().unwrap().len(), 6);
  assert_eq!(serde_json::from_value::<RuntimePlan>(encoded.clone()).unwrap(), plan);
  let mut unexpected = encoded;
  unexpected["environment"] = json!({"secret":"must stay in host configuration"});
  assert!(serde_json::from_value::<RuntimePlan>(unexpected).is_err());
  assert!(
    serde_json::from_value::<RuntimeConfigurationIdentity>(json!({
      "id":"configured", "revision":0, "executable":"unexpected"
    }))
    .is_err()
  );
}

#[test]
fn fingerprints_include_every_resolution_dimension_and_remain_stable_across_round_trip() {
  let plan = plan();
  let fingerprint = plan.fingerprint().unwrap();
  assert_eq!(plan.plan_id().unwrap(), fingerprint);
  validate_plan_id(&fingerprint).unwrap();
  assert_eq!(fingerprint.len(), 64);
  let decoded: RuntimePlan = serde_json::from_slice(&serde_json::to_vec(&plan).unwrap()).unwrap();
  assert_eq!(decoded.fingerprint().unwrap(), fingerprint);
  let mut alternatives = Vec::new();
  let mut changed = plan.clone();
  changed.provider = Provider::OpenCode;
  alternatives.push(changed);
  let mut changed = plan.clone();
  changed.configuration.id = "different configuration".into();
  alternatives.push(changed);
  let mut changed = plan.clone();
  changed.configuration.revision += 1;
  alternatives.push(changed);
  let mut changed = plan.clone();
  changed.ownership = Ownership::Attached;
  alternatives.push(changed);
  let mut changed = plan.clone();
  changed.reuse_scope = RuntimeReuseScope::Runtime;
  alternatives.push(changed);
  let mut changed = plan.clone();
  changed.working_directory.push("another-project");
  alternatives.push(changed);
  for changed in alternatives {
    assert_ne!(changed.fingerprint().unwrap(), fingerprint);
  }
  let mut shared = plan;
  shared.reuse_scope = RuntimeReuseScope::Session;
  let mut worker = shared.clone();
  worker.strategy = RuntimeStrategy::SessionWorker;
  assert_ne!(shared.fingerprint().unwrap(), worker.fingerprint().unwrap());
}
