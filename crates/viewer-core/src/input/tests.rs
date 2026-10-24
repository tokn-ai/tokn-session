use super::*;

#[test]
fn delivery_identity_uses_the_resolved_owner_not_the_client_key() {
  let mut target = InputTarget {
    provider: ViewerProvider::Codex,
    session_id: "thread".into(),
    session_file: "/first.jsonl".into(),
  };
  let owner = target.identity();
  target.session_file = "/another-copy.jsonl".into();
  assert_eq!(target.identity(), owner, "Codex routes both copies to one thread owner");
  target.provider = ViewerProvider::Pi;
  let pi_owner = target.identity();
  target.session_file = "/first.jsonl".into();
  assert_ne!(
    target.identity(),
    pi_owner,
    "Pi ownership includes the exact descriptor path"
  );
}
