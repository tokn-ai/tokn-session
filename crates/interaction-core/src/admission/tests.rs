use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn request(text: &str) -> SubmitSessionInputRequest {
  SubmitSessionInputRequest {
    model_selection: None,
    session_key: "session-a".into(),
    request_id: uuid::Uuid::new_v4().to_string(),
    text: text.into(),
    image_ids: Vec::new(),
  }
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

#[tokio::test]
async fn message_receipts_bind_model_intent_and_retain_confirmed_reset_on_failure() {
  use crate::managed_sessions::{MessageModelOutcome, MessageModelResult, MessageModelSelection, SessionModelState};
  let broker = InputBroker::default();
  let mut original = request("steer");
  original.model_selection = Some(MessageModelSelection {
    model_provider: Some("synthetic".into()),
    model_id: "vision".into(),
    expected_revision: "old".into(),
    runtime_generation: Some("old-runtime".into()),
  });
  let reset = MessageModelResult {
    outcome: MessageModelOutcome::IgnoredWhileRunning,
    state: SessionModelState::live(None, "runtime"),
  };
  let result = broker
    .submit_with(original.clone(), async move {
      Err(DeliveryError::not_sent("active model cannot accept images").with_model_result(reset))
    })
    .await;
  assert_eq!(result.status, InputDeliveryStatus::NotSent);
  assert_eq!(
    result.model_result.as_ref().unwrap().outcome,
    MessageModelOutcome::IgnoredWhileRunning
  );
  let receipt = broker
    .submit_with(original.clone(), async { panic!("must not replay") })
    .await;
  assert_eq!(receipt.model_result, result.model_result);
  original.model_selection.as_mut().unwrap().model_id = "different".into();
  let conflict = broker
    .submit_with(original, async { panic!("must not change a receipt") })
    .await;
  assert_eq!(conflict.status, InputDeliveryStatus::NotSent);
  assert!(conflict.model_result.is_none());
}

#[tokio::test]
async fn model_bound_steering_returns_actual_state_with_message_receipt() {
  use crate::managed_sessions::{MessageModelOutcome, MessageModelResult, SessionModelState};
  let broker = InputBroker::default();
  let result = broker
    .submit_with(request("steer"), async {
      Ok(Admission::WithModel {
        disposition: ModelInputDisposition::Steered,
        result: MessageModelResult {
          outcome: MessageModelOutcome::IgnoredWhileRunning,
          state: SessionModelState::live(None, "runtime"),
        },
      })
    })
    .await;
  assert_eq!(result.status, InputDeliveryStatus::Accepted);
  assert!(result.message.contains("steering"));
  assert_eq!(
    result.model_result.unwrap().outcome,
    MessageModelOutcome::IgnoredWhileRunning
  );
}
