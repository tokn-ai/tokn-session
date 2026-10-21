use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn request(text: &str) -> SubmitSessionInputRequest {
  SubmitSessionInputRequest {
    session_key: "session-a".into(),
    request_id: uuid::Uuid::new_v4().to_string(),
    text: text.into(),
    image_ids: Vec::new(),
  }
}

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

#[tokio::test]
async fn model_control_reservation_excludes_message_admission_and_releases_on_drop() {
  let broker = InputBroker::default();
  let target = InputTarget {
    provider: ViewerProvider::Pi,
    session_id: "pi-session".into(),
    session_file: "/session.jsonl".into(),
  };
  let reservation = broker.reserve_control(&target).unwrap();
  assert!(broker.reserve_control(&target).is_err());
  let blocked = broker
    .submit_custom(&target, request("hello"), async {
      panic!("must not deliver during a model change")
    })
    .await;
  assert_eq!(blocked.status, InputDeliveryStatus::NotSent);
  drop(reservation);
  assert_eq!(
    broker
      .submit_custom(&target, request("hello"), async { Ok(Admission::Started) })
      .await
      .status,
    InputDeliveryStatus::Accepted
  );
}

#[tokio::test]
async fn repeated_requests_deliver_once_and_reject_conflicting_payloads() {
  let broker = InputBroker::default();
  let request = request("hello");
  let count = Arc::new(AtomicUsize::new(0));
  for _ in 0..2 {
    let delivered = count.clone();
    let result = broker
      .submit_with(request.clone(), async move {
        delivered.fetch_add(1, Ordering::SeqCst);
        Ok(Admission::Accepted)
      })
      .await;
    assert_eq!(result.status, InputDeliveryStatus::Accepted);
  }
  assert_eq!(count.load(Ordering::SeqCst), 1);
  let mut conflict = request.clone();
  conflict.text = "different".into();
  assert_eq!(
    broker
      .submit_with(conflict, async { panic!("must not send") })
      .await
      .status,
    InputDeliveryStatus::NotSent
  );
  let mut conflict = request;
  conflict.session_key = "session-b".into();
  assert_eq!(
    broker
      .submit_with(conflict, async { panic!("must not send") })
      .await
      .status,
    InputDeliveryStatus::NotSent
  );
}

#[tokio::test]
async fn client_cancellation_does_not_cancel_delivery_or_lose_its_receipt() {
  let broker = InputBroker::default();
  let request = request("hello");
  let (started, wait_started) = tokio::sync::oneshot::channel();
  let (complete, wait_complete) = tokio::sync::oneshot::channel();
  let task_broker = broker.clone();
  let task_request = request.clone();
  let caller = tokio::spawn(async move {
    task_broker
      .submit_with(task_request, async move {
        started.send(()).unwrap();
        wait_complete.await.unwrap();
        Ok(Admission::QueuedFollowUp)
      })
      .await
  });
  wait_started.await.unwrap();
  caller.abort();
  assert_eq!(
    broker
      .submit_with(request.clone(), async { panic!("duplicate pending delivery") })
      .await
      .status,
    InputDeliveryStatus::Pending
  );
  assert_eq!(
    broker
      .submit_with(super::tests::request("another"), async { panic!("session is busy") })
      .await
      .status,
    InputDeliveryStatus::NotSent
  );
  complete.send(()).unwrap();
  tokio::time::timeout(std::time::Duration::from_secs(1), async {
    loop {
      let result = broker
        .submit_with(request.clone(), async { panic!("must return receipt") })
        .await;
      if result.status == InputDeliveryStatus::Accepted {
        break;
      }
      tokio::task::yield_now().await;
    }
  })
  .await
  .unwrap();
}

#[tokio::test]
async fn uncertain_failures_and_panics_are_cached_without_retry() {
  let broker = InputBroker::default();
  let request = request("hello");
  assert_eq!(
    broker
      .submit_with(request.clone(), async {
        Err(DeliveryError::uncertain("connection closed"))
      })
      .await
      .status,
    InputDeliveryStatus::Unknown
  );
  assert_eq!(
    broker.submit_with(request, async { panic!("no retry") }).await.status,
    InputDeliveryStatus::Unknown
  );
  let panic_request = super::tests::request("panic");
  assert_eq!(
    broker
      .submit_with(panic_request.clone(), async { panic!("transport failed") })
      .await
      .status,
    InputDeliveryStatus::Unknown
  );
  assert_eq!(
    broker
      .submit_with(panic_request, async { panic!("no retry") })
      .await
      .status,
    InputDeliveryStatus::Unknown
  );
}

#[test]
fn multiline_unicode_messages_are_bounded_and_invalid_requests_rejected() {
  let mut valid = request("  你好\n  code\tvalue\r\n");
  validate_request(&mut valid).unwrap();
  assert_eq!(valid.text, "  你好\n  code\tvalue\r\n");
  for text in [
    "   ".to_string(),
    "text\0".into(),
    "text\u{7f}".into(),
    "🦀".repeat(MAX_INPUT_LENGTH + 1),
  ] {
    assert!(validate_request(&mut request(&text)).is_err());
  }
  assert!(validate_request(&mut request(&"🦀".repeat(MAX_INPUT_LENGTH))).is_ok());
  let mut invalid = request("hello");
  invalid.request_id = "not-a-uuid".into();
  assert!(validate_request(&mut invalid).is_err());
}

#[test]
fn image_only_messages_validate_and_ordered_attachments_bind_receipts() {
  let first = uuid::Uuid::new_v4().to_string();
  let second = uuid::Uuid::new_v4().to_string();
  let mut image_only = request("");
  image_only.image_ids = vec![first.clone(), second.clone()];
  validate_request(&mut image_only).unwrap();
  assert_ne!(
    message_fingerprint("", &image_only.image_ids),
    message_fingerprint("", &[second, first.clone()])
  );
  assert_eq!(
    message_fingerprint("text", &[]),
    <[u8; 32]>::from(Sha256::digest(b"text"))
  );
  image_only.image_ids = vec![first.clone(), first];
  assert!(validate_request(&mut image_only).is_err());
  image_only.image_ids = vec!["/client/image.png".into()];
  assert!(validate_request(&mut image_only).is_err());
}

#[tokio::test]
async fn image_receipts_reject_changed_attachments_without_a_second_delivery() {
  let broker = InputBroker::default();
  let mut original = request("caption");
  original.image_ids = vec![uuid::Uuid::new_v4().to_string()];
  let result = broker
    .submit_with(original.clone(), async {
      Err(DeliveryError::uncertain("closed after send"))
    })
    .await;
  assert_eq!(result.status, InputDeliveryStatus::Unknown);
  assert_eq!(
    broker
      .submit_with(original.clone(), async { panic!("must not retry") })
      .await
      .status,
    InputDeliveryStatus::Unknown
  );
  original.image_ids = vec![uuid::Uuid::new_v4().to_string()];
  assert_eq!(
    broker
      .submit_with(original, async { panic!("must not send conflicting images") })
      .await
      .status,
    InputDeliveryStatus::NotSent
  );
}
