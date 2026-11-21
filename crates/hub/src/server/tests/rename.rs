use super::*;
use ed25519_dalek::{Signer, SigningKey};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};

fn account(directory: &std::path::Path, username: &str, seed: u8) -> (String, String) {
  let user_id = uuid::Uuid::new_v4().to_string();
  let token = format!("tkna_{}", protocol::encode(&[seed; 32]));
  let token_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
  let connection = rusqlite::Connection::open(directory.join("hub.sqlite")).unwrap();
  connection
    .execute(
      "INSERT INTO hub_accounts (user_id, username, email) VALUES (?1, ?2, ?3)",
      (&user_id, username, format!("{username}@example.invalid")),
    )
    .unwrap();
  connection
    .execute(
      "INSERT INTO hub_account_credentials (credential_id, token_hash, user_id, kind, scopes, expires_at)
       VALUES (?1, ?2, ?3, 'cli', 3, ?4)",
      (
        uuid::Uuid::new_v4().to_string(),
        &token_hash,
        &user_id,
        crate::secure_client::unix_time().unwrap() as i64 + 3600,
      ),
    )
    .unwrap();
  (user_id, token)
}

fn request(key: &SigningKey, user_id: &str, host_id: &str, old_address: &str, new_name: &str) -> Value {
  let public_key = protocol::encode(key.verifying_key().as_bytes());
  let proof = serde_json::to_vec(&(
    "tokn-hub-account-host-rename-v1",
    format!("{ORIGIN}/"),
    user_id,
    host_id,
    &public_key,
    old_address,
    new_name,
  ))
  .unwrap();
  json!({
    "host_id": host_id, "public_key": public_key,
    "expected_machine_address": old_address, "machine_name": new_name,
    "signature": protocol::encode(&key.sign(&proof).to_bytes()),
  })
}

#[tokio::test]
async fn signed_rename_preserves_identity_and_discovery_and_rejects_stale_or_unowned_changes() {
  let directory = tempfile::tempdir().unwrap();
  let state = state(directory.path());
  let (user_id, token) = account(directory.path(), "alice", 1);
  let (other_user, other_token) = account(directory.path(), "bob", 2);
  let key = SigningKey::generate(&mut OsRng);
  let host = crate::store::HostRecord {
    host_id: uuid::Uuid::new_v4().to_string(),
    public_key: protocol::encode(key.verifying_key().as_bytes()),
    name: "Workstation".into(),
    access: "view".into(),
  };
  let old = state
    .store
    .claim_account_host(host.clone(), &user_id, "laptop")
    .unwrap();
  let app = router(state.clone());
  let path = "/hub/v1/account/machines/rename";
  let rename = request(&key, &user_id, &host.host_id, "alice:laptop", "studio");
  assert_eq!(
    post_json(app.clone(), path, None, None, rename.clone()).await.0,
    StatusCode::UNAUTHORIZED
  );
  let mut tampered = rename.clone();
  tampered["machine_name"] = json!("injected");
  assert_eq!(
    post_json(app.clone(), path, None, Some(&token), tampered).await.0,
    StatusCode::FORBIDDEN
  );
  let other = request(&key, &other_user, &host.host_id, "alice:laptop", "studio");
  assert_eq!(
    post_json(app.clone(), path, None, Some(&other_token), other).await.0,
    StatusCode::FORBIDDEN
  );
  let (status, renamed) = post_json(app.clone(), path, None, Some(&token), rename.clone()).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(renamed["host_id"], host.host_id);
  assert_eq!(renamed["machine_address"], "alice:studio");
  assert_eq!(renamed["group"], serde_json::to_value(old.group).unwrap());
  assert_eq!(state.store.host(&host.host_id).unwrap(), Some(host.clone()));
  assert_eq!(
    get(app.clone(), "/hub/v1/resolve/alice/laptop", None).await.status(),
    StatusCode::NOT_FOUND
  );
  assert_eq!(
    json_body(get(app.clone(), "/hub/v1/resolve/alice/studio", None).await).await["host_id"],
    host.host_id
  );
  assert_eq!(
    post_json(app.clone(), path, None, Some(&token), rename).await,
    (StatusCode::OK, renamed)
  );
  let stale = request(&key, &user_id, &host.host_id, "alice:laptop", "office");
  assert_eq!(
    post_json(app.clone(), path, None, Some(&token), stale).await.0,
    StatusCode::CONFLICT
  );
  state.store.remove_host(&host.host_id).unwrap();
  let revoked = request(&key, &user_id, &host.host_id, "alice:studio", "office");
  assert_eq!(
    post_json(app, path, None, Some(&token), revoked).await.0,
    StatusCode::FORBIDDEN
  );
}
