use super::*;

const HOST: &str = "318cb31a-6e5a-496c-bec9-837d6221b623";
const NOW: u64 = 1_700_000_010;

fn seed() -> TotpSecret {
  TotpSecret::from_base32("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ").unwrap()
}

#[test]
fn rfc6238_sha1_vectors_and_six_digit_truncation() {
  let seed = seed();
  for (time, expected) in [
    (59, "94287082"),
    (1_111_111_109, "07081804"),
    (1_111_111_111, "14050471"),
    (1_234_567_890, "89005924"),
    (2_000_000_000, "69279037"),
    (20_000_000_000, "65353130"),
  ] {
    assert_eq!(format!("{:08}", seed.hotp(time / 30) % 100_000_000), expected);
    assert_eq!(seed.code_at(time), expected[2..]);
  }
}

#[test]
fn seed_import_generation_and_provisioning() {
  let secret = TotpSecret::generate();
  let imported = TotpSecret::from_base32(&secret.to_base32().to_lowercase()).unwrap();
  assert_eq!(secret.code_at(NOW), imported.code_at(NOW));
  assert!(TotpSecret::from_base32("short").is_err());
  assert!(TotpSecret::from_base32(&"A".repeat(257)).is_err());
  assert!(TotpSecret::from_base32("this is not a base32 authenticator secret!").is_err());
  let uri = url::Url::parse(&secret.provisioning_uri("My workstation").unwrap()).unwrap();
  assert_eq!(uri.scheme(), "otpauth");
  assert_eq!(uri.host_str(), Some("totp"));
  let query = uri.query_pairs().collect::<std::collections::HashMap<_, _>>();
  assert_eq!(query["secret"], secret.to_base32());
  assert_eq!(query["algorithm"], "SHA1");
  assert_eq!(query["digits"], "6");
  assert_eq!(query["period"], "30");
  assert!(secret.provisioning_uri("host\nname").is_err());
}

#[test]
fn matching_code_authenticates_both_noise_keys_and_final_ack() {
  let seed = seed();
  let host_identity = NoiseIdentity::generate().unwrap();
  let client_identity = NoiseIdentity::generate().unwrap();
  let (client, hello) = ClientPairing::start(HOST, &client_identity, &seed.code_at(NOW), NOW).unwrap();
  assert!(is_pairing_record(&hello));
  assert_eq!(peek_step(&hello).unwrap(), NOW / 30);
  let (host, response) = HostPairing::respond(&seed, HOST, &host_identity, &hello, NOW).unwrap();
  let (client, confirmation) = client.confirm(&response).unwrap();
  let (authorized, ack) = host.finish(&confirmation, NOW).unwrap();
  assert_eq!(authorized.host_id, HOST);
  assert_eq!(authorized.host_public_key, host_identity.public_key());
  assert_eq!(authorized.client_public_key, client_identity.public_key());
  assert_eq!(client.finish(&ack).unwrap(), authorized);
}

#[test]
fn wrong_code_never_authenticates_host() {
  let seed = seed();
  let actual = seed.code_at(NOW);
  let wrong = if actual == "000000" { "000001" } else { "000000" };
  let (client, hello) = ClientPairing::start(HOST, &NoiseIdentity::generate().unwrap(), wrong, NOW).unwrap();
  let (_, response) = HostPairing::respond(&seed, HOST, &NoiseIdentity::generate().unwrap(), &hello, NOW).unwrap();
  assert!(client.confirm(&response).is_err());
}

#[test]
fn wrong_confirmation_does_not_authorize_client() {
  let seed = seed();
  let (_, hello) = ClientPairing::start(HOST, &NoiseIdentity::generate().unwrap(), &seed.code_at(NOW), NOW).unwrap();
  let (host, _) = HostPairing::respond(&seed, HOST, &NoiseIdentity::generate().unwrap(), &hello, NOW).unwrap();
  let fabricated = write_record(&Confirmation {
    confirmation: encode(&[0; 32]),
  })
  .unwrap();
  assert!(host.finish(&fabricated, NOW).is_err());
}

#[test]
fn host_key_substitution_is_rejected() {
  let seed = seed();
  let (client, hello) =
    ClientPairing::start(HOST, &NoiseIdentity::generate().unwrap(), &seed.code_at(NOW), NOW).unwrap();
  let (_, response) = HostPairing::respond(&seed, HOST, &NoiseIdentity::generate().unwrap(), &hello, NOW).unwrap();
  let mut response: HostReply = read_record(&response).unwrap();
  response.hello.host_public_key = NoiseIdentity::generate().unwrap().public_key();
  assert!(client.confirm(&write_record(&response).unwrap()).is_err());
}

#[test]
fn client_key_substitution_is_rejected() {
  let seed = seed();
  let (client, hello) =
    ClientPairing::start(HOST, &NoiseIdentity::generate().unwrap(), &seed.code_at(NOW), NOW).unwrap();
  let mut hello: ClientHello = read_record(&hello).unwrap();
  hello.client_public_key = NoiseIdentity::generate().unwrap().public_key();
  let (_, response) = HostPairing::respond(
    &seed,
    HOST,
    &NoiseIdentity::generate().unwrap(),
    &write_record(&hello).unwrap(),
    NOW,
  )
  .unwrap();
  assert!(client.confirm(&response).is_err());
}

#[test]
fn shared_seed_cannot_cross_wire_target_host() {
  let seed = seed();
  let (_, hello) = ClientPairing::start(HOST, &NoiseIdentity::generate().unwrap(), &seed.code_at(NOW), NOW).unwrap();
  assert!(HostPairing::respond(&seed, "another-host", &NoiseIdentity::generate().unwrap(), &hello, NOW).is_err());

  let (client, hello) =
    ClientPairing::start(HOST, &NoiseIdentity::generate().unwrap(), &seed.code_at(NOW), NOW).unwrap();
  let mut modified: ClientHello = read_record(&hello).unwrap();
  modified.host_id = "another-host".into();
  let (_, response) = HostPairing::respond(
    &seed,
    "another-host",
    &NoiseIdentity::generate().unwrap(),
    &write_record(&modified).unwrap(),
    NOW,
  )
  .unwrap();
  assert!(client.confirm(&response).is_err());
}

#[test]
fn first_record_changes_are_bound_to_confirmation() {
  let seed = seed();
  let (client, mut hello) =
    ClientPairing::start(HOST, &NoiseIdentity::generate().unwrap(), &seed.code_at(NOW), NOW).unwrap();
  // Even a semantically equivalent first message has a different transcript.
  hello.push(b' ');
  let (_, response) = HostPairing::respond(&seed, HOST, &NoiseIdentity::generate().unwrap(), &hello, NOW).unwrap();
  assert!(client.confirm(&response).is_err());
}

#[test]
fn replayed_confirmation_and_ack_fail_in_fresh_exchange() {
  let seed = seed();
  let host_identity = NoiseIdentity::generate().unwrap();
  let client_identity = NoiseIdentity::generate().unwrap();
  let (client, hello) = ClientPairing::start(HOST, &client_identity, &seed.code_at(NOW), NOW).unwrap();
  let (host, response) = HostPairing::respond(&seed, HOST, &host_identity, &hello, NOW).unwrap();
  let (_, confirmation) = client.confirm(&response).unwrap();
  let (_, ack) = host.finish(&confirmation, NOW).unwrap();

  // Replaying even the same first flight yields fresh host PAKE randomness.
  let (host, _) = HostPairing::respond(&seed, HOST, &host_identity, &hello, NOW).unwrap();
  assert!(host.finish(&confirmation, NOW).is_err());

  let (client, hello) = ClientPairing::start(HOST, &client_identity, &seed.code_at(NOW), NOW).unwrap();
  let (_, response) = HostPairing::respond(&seed, HOST, &host_identity, &hello, NOW).unwrap();
  let (client, _) = client.confirm(&response).unwrap();
  assert!(client.finish(&ack).is_err());
}

#[test]
fn confirmation_roles_cannot_be_reflected() {
  let seed = seed();
  let (client, hello) =
    ClientPairing::start(HOST, &NoiseIdentity::generate().unwrap(), &seed.code_at(NOW), NOW).unwrap();
  let (host, response) = HostPairing::respond(&seed, HOST, &NoiseIdentity::generate().unwrap(), &hello, NOW).unwrap();
  let reflected: HostReply = read_record(&response).unwrap();
  let reflected = write_record(&Confirmation {
    confirmation: reflected.confirmation,
  })
  .unwrap();
  assert!(host.finish(&reflected, NOW).is_err());
  let (client, confirmation) = client.confirm(&response).unwrap();
  assert!(client.finish(&confirmation).is_err());
}

#[test]
fn time_window_is_checked_before_reply_and_at_confirmation() {
  let seed = seed();
  let host_identity = NoiseIdentity::generate().unwrap();
  let (client, hello) =
    ClientPairing::start(HOST, &NoiseIdentity::generate().unwrap(), &seed.code_at(NOW), NOW).unwrap();
  assert!(HostPairing::respond(&seed, HOST, &host_identity, &hello, NOW - 30).is_ok());
  assert!(HostPairing::respond(&seed, HOST, &host_identity, &hello, NOW + 30).is_ok());
  assert!(HostPairing::respond(&seed, HOST, &host_identity, &hello, NOW + 60).is_err());
  let (host, response) = HostPairing::respond(&seed, HOST, &host_identity, &hello, NOW).unwrap();
  let (_, confirmation) = client.confirm(&response).unwrap();
  assert!(host.finish(&confirmation, NOW + 60).is_err());
}

#[test]
fn malformed_oversized_or_wrong_role_records_are_rejected() {
  let seed = seed();
  let host = NoiseIdentity::generate().unwrap();
  assert!(peek_step(&vec![0; MAX_PAIRING_RECORD + 1]).is_err());
  assert!(peek_step(b"not a pairing record").is_err());
  assert!(ClientPairing::start(HOST, &host, "１２３４５６", NOW).is_err());
  assert!(ClientPairing::start(HOST, &host, "12345", NOW).is_err());
  assert!(ClientPairing::start("../host", &host, "123456", NOW).is_err());
  let (_, hello) = ClientPairing::start(HOST, &host, &seed.code_at(NOW), NOW).unwrap();
  let mut hello: ClientHello = read_record(&hello).unwrap();
  let mut pake = decode_pake(&hello.pake).unwrap();
  pake[0] = b'B';
  hello.pake = encode(&pake);
  assert!(HostPairing::respond(&seed, HOST, &host, &write_record(&hello).unwrap(), NOW).is_err());
}
