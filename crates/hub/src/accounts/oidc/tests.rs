use super::*;

use jsonwebtoken::{
  EncodingKey, Header,
  jwk::{Jwk, JwkSet, KeyAlgorithm, PublicKeyUse},
};
use rand::{RngCore, rngs::OsRng};
use serde_json::{Value, json};

const ISSUER_URL: &str = "https://account.example.test/oidc";
const HUB_URL: &str = "https://hub.example.test";
const CLIENT: &str = "example-client";

fn fixture() -> Oidc {
  Oidc::new(
    ISSUER_URL,
    HUB_URL,
    CLIENT.into(),
    Zeroizing::new("fixture-secret".into()),
  )
  .unwrap()
}

fn discovery() -> Value {
  json!({
    "issuer": ISSUER_URL,
    "authorization_endpoint": format!("{ISSUER_URL}/auth"),
    "token_endpoint": format!("{ISSUER_URL}/token"),
    "jwks_uri": format!("{ISSUER_URL}/jwks"),
    "userinfo_endpoint": format!("{ISSUER_URL}/me"),
    "id_token_signing_alg_values_supported": ["ES384"],
    "code_challenge_methods_supported": ["S256"],
    "response_types_supported": ["code", "code id_token"],
    "token_endpoint_auth_methods_supported": ["client_secret_basic"],
  })
}

#[tokio::test]
async fn authorization_binds_the_discovered_issuer_callback_nonce_and_pkce() {
  let oidc = fixture();
  let metadata = oidc
    .verify_discovery(serde_json::from_value(discovery()).unwrap())
    .unwrap();
  *oidc.discovery.lock().await = Some((metadata, Instant::now()));
  assert_eq!(
    oidc.discovery_url.as_str(),
    format!("{ISSUER_URL}/.well-known/openid-configuration")
  );
  let url = oidc
    .authorization_url("fixture-state", "fixture-verifier", "fixture-nonce")
    .await
    .unwrap();
  assert_eq!(url.path(), "/oidc/auth");
  let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
  assert_eq!(query.get("scope").unwrap(), "openid profile email");
  assert_eq!(query.get("response_type").unwrap(), "code");
  assert_eq!(query.get("response_mode").unwrap(), "query");
  assert_eq!(query.get("state").unwrap(), "fixture-state");
  assert_eq!(query.get("nonce").unwrap(), "fixture-nonce");
  assert_eq!(query.get("code_challenge_method").unwrap(), "S256");
  assert_eq!(
    query.get("code_challenge").unwrap(),
    &URL_SAFE_NO_PAD.encode(Sha256::digest(b"fixture-verifier"))
  );
  assert_eq!(
    query.get("redirect_uri").unwrap(),
    &format!("{HUB_URL}{APELL_CALLBACK}")
  );
  assert!(!url.as_str().contains("fixture-secret"));
}

#[test]
fn issuer_and_discovery_do_not_allow_changing_origin_or_dropping_security_features() {
  for issuer in [
    "http://account.example.test/oidc",
    "https://account.example.test",
    "https://account.example.test/oidc/",
    "https://account.example.test/oidc?extra=1",
    "https://account.example.test/oidc#fragment",
    "https://@account.example.test/oidc",
    "https://account.example.test/elsewhere/../oidc",
  ] {
    assert!(
      Oidc::new(issuer, HUB_URL, CLIENT.into(), Zeroizing::new("fixture-secret".into())).is_err(),
      "{issuer}"
    );
  }
  let oidc = fixture();
  for (key, rejected) in [
    ("issuer", json!("https://account.example.test/other")),
    ("token_endpoint", json!("https://provider.example.test/oidc/token")),
    (
      "authorization_endpoint",
      json!("https://account.example.test/oidc-sibling/auth"),
    ),
    ("userinfo_endpoint", json!("https://account.example.test/oidc/../me")),
    ("jwks_uri", json!("http://account.example.test/oidc/jwks")),
    ("token_endpoint_auth_methods_supported", json!(["none"])),
    ("code_challenge_methods_supported", json!(["plain"])),
    ("response_types_supported", json!(["id_token"])),
    ("id_token_signing_alg_values_supported", json!(["HS256"])),
  ] {
    let mut invalid = discovery();
    invalid[key] = rejected;
    assert!(
      oidc.verify_discovery(serde_json::from_value(invalid).unwrap()).is_err(),
      "{key}"
    );
  }
}

fn elliptic_signing_key() -> EncodingKey {
  // PKCS#8 wrapping a randomly generated SEC1 P-384 scalar. The first byte
  // places it in range without rejection; there is no checked-in private key.
  let mut der = Zeroizing::new(vec![
    0x30, 0x4e, 0x02, 0x01, 0x00, 0x30, 0x10, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x05, 0x2b,
    0x81, 0x04, 0x00, 0x22, 0x04, 0x37, 0x30, 0x35, 0x02, 0x01, 0x01, 0x04, 0x30,
  ]);
  let mut scalar = Zeroizing::new([0; 48]);
  OsRng.fill_bytes(&mut *scalar);
  scalar[0] = 1;
  der.extend_from_slice(&*scalar);
  EncodingKey::from_ec_der(&der)
}

#[tokio::test]
async fn es384_identity_requires_the_signed_issuer_audience_time_and_nonce() {
  let oidc = fixture();
  let provider = oidc
    .verify_discovery(serde_json::from_value(discovery()).unwrap())
    .unwrap();
  let signing_key = elliptic_signing_key();
  let mut jwk = Jwk::from_encoding_key(&signing_key, Algorithm::ES384).unwrap();
  jwk.common.key_id = Some("fixture-key".into());
  jwk.common.public_key_use = Some(PublicKeyUse::Signature);
  let mut wrongly_labeled = jwk.clone();
  wrongly_labeled.common.key_algorithm = Some(KeyAlgorithm::ES256);
  assert!(
    keys::Cache::seeded(
      provider.jwks_uri.clone(),
      JwkSet {
        keys: vec![wrongly_labeled]
      }
    )
    .is_err()
  );
  *oidc.keys.lock().await = keys::Cache::seeded(provider.jwks_uri.clone(), JwkSet { keys: vec![jwk] }).unwrap();
  let mut header = Header::new(Algorithm::ES384);
  header.kid = Some("fixture-key".into());
  let now = crate::secure_client::unix_time().unwrap();
  let claims = json!({
    "iss": ISSUER_URL, "sub": "fixture-subject", "aud": CLIENT, "iat": now, "exp": now + 300,
    "nonce": "fixture-nonce", "at_hash": access_token_hash(Algorithm::ES384, "fixture-access-token"),
  });
  let sign = |claims: &Value| jsonwebtoken::encode(&header, claims, &signing_key).unwrap();
  let verify = |token: String| {
    let (oidc, provider) = (&oidc, &provider);
    async move {
      oidc
        .verify_identity(provider, &token, "fixture-access-token", "fixture-nonce")
        .await
    }
  };
  assert_eq!(verify(sign(&claims)).await.unwrap(), "fixture-subject");
  for (field, value) in [
    ("iss", json!("https://another.example.test/oidc")),
    ("aud", json!("another-client")),
    ("aud", json!([CLIENT, "another-client"])),
    ("azp", json!("another-client")),
    ("exp", json!(now - 1)),
    ("iat", json!(now + 120)),
    ("nbf", json!(now + 120)),
    ("nonce", json!("another-nonce")),
    ("at_hash", json!("another-token-hash")),
    ("sub", json!("")),
  ] {
    let mut invalid = claims.clone();
    invalid[field] = value;
    assert!(verify(sign(&invalid)).await.is_err(), "{field}");
  }
  let foreign = jsonwebtoken::encode(&header, &claims, &elliptic_signing_key()).unwrap();
  assert!(verify(foreign).await.is_err());
  header.kid = Some("unknown-key".into());
  assert!(
    verify(jsonwebtoken::encode(&header, &claims, &signing_key).unwrap())
      .await
      .is_err()
  );
}

#[test]
fn userinfo_must_match_the_token_subject_and_only_verified_email_is_returned() {
  let oidc = fixture();
  let identity = |claims: Value, subject: &str| {
    serde_json::from_value::<UserInfo>(claims)
      .unwrap()
      .verified(&oidc.issuer, subject.into())
  };
  let claims =
    json!({"sub":"fixture-subject", "username":"fixture", "email":"alice@example.test", "email_verified":true});
  let matched = identity(claims.clone(), "fixture-subject").unwrap();
  assert_eq!(matched.subject, "fixture-subject");
  assert_eq!(matched.login.as_deref(), Some("fixture"));
  assert_eq!(matched.verified_email.as_deref(), Some("alice@example.test"));
  assert!(identity(claims.clone(), "wrong-subject").is_err());
  let mut unverified = claims;
  unverified["email_verified"] = json!(false);
  assert!(
    identity(unverified, "fixture-subject")
      .unwrap()
      .verified_email
      .is_none()
  );
}
