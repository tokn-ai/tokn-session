use super::*;
use crate::{pairing::TotpSecret, secure::NoiseIdentity};
use std::path::Path;
use webauthn_authenticator_rs::{WebauthnAuthenticator, softpasskey::SoftPasskey};

const HOST: &str = "11111111-1111-4111-8111-111111111111";
const OTHER_HOST: &str = "22222222-2222-4222-8222-222222222222";
const ORIGIN: &str = "https://hub.example.com";
const BINDING: &str = "authenticated-handshake-transcript";

fn setup(path: &Path) -> (HostPasskeys, String) {
  onboarding::initialize_host_access(path, &TotpSecret::generate()).unwrap();
  let device = NoiseIdentity::generate().unwrap().public_key();
  onboarding::authorize_device(path, &device, 30, 900).unwrap();
  (
    HostPasskeys::new(path.into(), HOST, "Workstation", ORIGIN).unwrap(),
    device,
  )
}

fn enroll(host: &HostPasskeys, device: &str, authenticator: &mut WebauthnAuthenticator<SoftPasskey>) -> Value {
  let (pending, start) = host
    .start(HostAuthOperation::RegisterStart, json!({}), device, BINDING, 900)
    .unwrap();
  let credential = authenticator
    .do_registration(
      Url::parse(ORIGIN).unwrap(),
      serde_json::from_value(start["options"].clone()).unwrap(),
    )
    .unwrap();
  let payload = json!({"credential": credential});
  host
    .finish(
      pending,
      HostAuthOperation::RegisterFinish,
      payload.clone(),
      device,
      BINDING,
      901,
    )
    .unwrap();
  payload
}

fn assertion(
  host: &HostPasskeys,
  device: &str,
  binding: &str,
  authenticator: &mut WebauthnAuthenticator<SoftPasskey>,
) -> (Pending, Value) {
  let (pending, start) = host
    .start(HostAuthOperation::LoginStart, json!({}), device, binding, 902)
    .unwrap();
  let credential = authenticator
    .do_authentication(
      Url::parse(ORIGIN).unwrap(),
      serde_json::from_value(start["options"].clone()).unwrap(),
    )
    .unwrap();
  (pending, json!({"credential": credential}))
}

#[test]
fn only_an_authorized_device_can_enroll_and_revocation_interrupts_enrollment() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("host-access.json");
  let (host, device) = setup(&path);
  let unpaired = NoiseIdentity::generate().unwrap().public_key();
  assert!(
    host
      .start(HostAuthOperation::RegisterStart, json!({}), &unpaired, BINDING, 900)
      .is_err()
  );
  assert!(
    host
      .start(
        HostAuthOperation::RegisterStart,
        json!({"device_public_key": unpaired}),
        &device,
        BINDING,
        900
      )
      .is_err()
  );
  assert!(
    host
      .start(HostAuthOperation::LoginFinish, json!({}), &device, BINDING, 900)
      .is_err()
  );
  let (pending, start) = host
    .start(HostAuthOperation::RegisterStart, json!({}), &device, BINDING, 900)
    .unwrap();
  let mut authenticator = WebauthnAuthenticator::new(SoftPasskey::new(true));
  let credential = authenticator
    .do_registration(
      Url::parse(ORIGIN).unwrap(),
      serde_json::from_value(start["options"].clone()).unwrap(),
    )
    .unwrap();
  onboarding::remove_device(&path, &device).unwrap();
  assert!(
    host
      .finish(
        pending,
        HostAuthOperation::RegisterFinish,
        json!({"credential": credential}),
        &device,
        BINDING,
        901
      )
      .is_err()
  );
  assert!(onboarding::host_passkeys(&path, ORIGIN).unwrap().is_empty());
}

#[test]
fn host_passkey_authorizes_a_new_device_and_persists_independently_of_hub_admin() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("host-access.json");
  let (host, paired) = setup(&path);
  let mut authenticator = WebauthnAuthenticator::new(SoftPasskey::new(true));
  enroll(&host, &paired, &mut authenticator);
  let new_device = NoiseIdentity::generate().unwrap().public_key();
  assert!(!onboarding::is_authorized(&path, &new_device).unwrap());
  let (pending, payload) = assertion(&host, &new_device, BINDING, &mut authenticator);
  let result = host
    .finish(
      pending,
      HostAuthOperation::LoginFinish,
      payload,
      &new_device,
      BINDING,
      903,
    )
    .unwrap();
  assert_eq!(
    result,
    json!({"authorized": true, "registered": false, "device_public_key": new_device})
  );
  assert!(onboarding::is_authorized(&path, &new_device).unwrap());
  let reopened = HostPasskeys::new(path.clone(), HOST, "Workstation", ORIGIN).unwrap();
  assert_eq!(onboarding::host_passkeys(&path, ORIGIN).unwrap().len(), 1);
  let (pending, payload) = assertion(&reopened, &new_device, "new-handshake", &mut authenticator);
  reopened
    .finish(
      pending,
      HostAuthOperation::LoginFinish,
      payload,
      &new_device,
      "new-handshake",
      903,
    )
    .unwrap();
  assert!(onboarding::validate_host_passkey_origin(&path, Some(ORIGIN)).is_ok());
  assert!(onboarding::validate_host_passkey_origin(&path, None).is_err());
  assert!(onboarding::validate_host_passkey_origin(&path, Some("https://other.example.com")).is_err());
  assert!(HostPasskeys::new(path, HOST, "Workstation", "https://other.example.com").is_err());
}

#[test]
fn assertions_cannot_move_between_devices_channels_hosts_or_nonces() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("host-access.json");
  let (host, paired) = setup(&path);
  let mut authenticator = WebauthnAuthenticator::new(SoftPasskey::new(true));
  enroll(&host, &paired, &mut authenticator);
  let new_device = NoiseIdentity::generate().unwrap().public_key();
  let other_device = NoiseIdentity::generate().unwrap().public_key();
  for (device, binding, now) in [
    (other_device.as_str(), BINDING, 903),
    (new_device.as_str(), "another-channel", 903),
    (new_device.as_str(), BINDING, 1202),
    (new_device.as_str(), BINDING, 901),
  ] {
    let (pending, payload) = assertion(&host, &new_device, BINDING, &mut authenticator);
    assert!(
      host
        .finish(pending, HostAuthOperation::LoginFinish, payload, device, binding, now)
        .is_err()
    );
  }
  let other_host = HostPasskeys::new(path.clone(), OTHER_HOST, "Another machine", ORIGIN).unwrap();
  let (pending, payload) = assertion(&host, &new_device, BINDING, &mut authenticator);
  assert!(
    other_host
      .finish(
        pending,
        HostAuthOperation::LoginFinish,
        payload,
        &new_device,
        BINDING,
        903
      )
      .is_err()
  );
  let (_old_pending, old_payload) = assertion(&host, &new_device, BINDING, &mut authenticator);
  let (pending, _) = host
    .start(HostAuthOperation::LoginStart, json!({}), &new_device, BINDING, 902)
    .unwrap();
  assert!(
    host
      .finish(
        pending,
        HostAuthOperation::LoginFinish,
        old_payload,
        &new_device,
        BINDING,
        903
      )
      .is_err()
  );
  assert!(!onboarding::is_authorized(&path, &new_device).unwrap());
}

#[test]
fn valid_assertions_for_a_different_origin_are_rejected() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("host-access.json");
  let (host, paired) = setup(&path);
  let mut authenticator = WebauthnAuthenticator::new(SoftPasskey::new(true));
  enroll(&host, &paired, &mut authenticator);
  let new_device = NoiseIdentity::generate().unwrap().public_key();
  let (pending, start) = host
    .start(HostAuthOperation::LoginStart, json!({}), &new_device, BINDING, 902)
    .unwrap();
  let credential = authenticator
    .do_authentication(
      Url::parse("https://hub.example.com:8443").unwrap(),
      serde_json::from_value(start["options"].clone()).unwrap(),
    )
    .unwrap();
  assert!(
    host
      .finish(
        pending,
        HostAuthOperation::LoginFinish,
        json!({"credential": credential}),
        &new_device,
        BINDING,
        903
      )
      .is_err()
  );
  assert!(!onboarding::is_authorized(&path, &new_device).unwrap());
}

#[test]
fn only_stable_https_origins_or_localhost_are_accepted() {
  for origin in [
    "http://hub.example.com",
    "https://127.0.0.1",
    "https://hub.example.com/path",
    "https://user@hub.example.com",
    "https://hub.example.com/?q=1",
  ] {
    assert!(validate_origin(origin).is_err(), "{origin}");
  }
  assert!(validate_origin(ORIGIN).is_ok());
  assert!(validate_origin("http://localhost:5559").is_ok());
}
