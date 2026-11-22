use super::*;
use tokio::{
  io::{AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf},
  sync::oneshot,
};

struct Peer {
  reader: framing::RpcReader<ReadHalf<DuplexStream>>,
  writer: WriteHalf<DuplexStream>,
}
impl Peer {
  async fn next(&mut self) -> Value {
    tokio::time::timeout(Duration::from_secs(2), self.reader.read_frame())
      .await
      .unwrap()
      .unwrap()
      .unwrap()
  }
  async fn send(&mut self, frame: Value) {
    let mut bytes = serde_json::to_vec(&frame).unwrap();
    bytes.push(b'\n');
    self.writer.write_all(&bytes).await.unwrap();
  }
  async fn quiet(&mut self) {
    assert!(
      tokio::time::timeout(Duration::from_millis(30), self.reader.read_frame())
        .await
        .is_err()
    );
  }
}

type Review = (HumanCommandApproval, oneshot::Sender<HumanCommandDecision>);
fn reviewer() -> (HumanCommandReviewer, mpsc::Receiver<Review>) {
  let (send, receive) = mpsc::channel(32);
  (
    Arc::new(move |request| {
      let send = send.clone();
      Box::pin(async move {
        let (answer, receive) = oneshot::channel();
        if send.send((request, answer)).await.is_err() {
          return HumanCommandDecision::Decline;
        }
        receive.await.unwrap_or(HumanCommandDecision::Decline)
      })
    }),
    receive,
  )
}
async fn managed(reviewer: Option<HumanCommandReviewer>, ownership: Ownership) -> (CodexAdapter, Peer) {
  let (adapter, native) = tokio::io::duplex(1024 * 1024);
  let (reader, writer) = tokio::io::split(adapter);
  let (native_reader, native_writer) = tokio::io::split(native);
  let mut peer = Peer {
    reader: framing::RpcReader::new(native_reader),
    writer: native_writer,
  };
  let expected = ownership == Ownership::Managed && reviewer.is_some();
  let initialize = tokio::spawn(async move {
    let options = EndpointOptions::new("human-fixture");
    let (transport, incoming) = Transport::stdio(reader, writer, None, options.request_timeout);
    CodexAdapter::initialize(options, ownership, transport, incoming, reviewer).await
  });
  let request = peer.next().await;
  assert_eq!(
    request["params"]["capabilities"]["requiredHumanApproval"] == true,
    expected
  );
  if !expected {
    assert!(request["params"]["capabilities"].get("requiredHumanApproval").is_none());
  }
  peer
    .send(json!({"id":request["id"],"result":{"userAgent":"fixture"}}))
    .await;
  assert_eq!(peer.next().await["method"], "initialized");
  (initialize.await.unwrap().unwrap(), peer)
}
fn human(id: &str) -> Value {
  json!({"id":id,"method":"item/commandExecution/requestApproval","params":{
    "kind":"humanCommand","threadId":"thread-one","turnId":"original-turn","itemId":"command-item",
    "approvalId":format!("approval-{id}"),"command":"printf 'exact fixture'","cwd":"/workspace",
    "reason":"Managed human checkpoint","environmentId":"native-environment",
    "additionalPermissions":{"fileSystem":{"read":["/workspace"]}},
    "availableDecisions":["accept","decline","cancel"]
  }})
}
async fn received(reviews: &mut mpsc::Receiver<Review>) -> Review {
  tokio::time::timeout(Duration::from_secs(2), reviews.recv())
    .await
    .unwrap()
    .unwrap()
}

#[tokio::test]
async fn managed_checkpoints_use_only_host_reviewer_and_one_time_native_decisions() {
  let (reviewer, mut reviews) = reviewer();
  let (adapter, mut peer) = managed(Some(reviewer), Ownership::Managed).await;
  let mut events = adapter.subscribe();
  for (index, decision, native) in [
    (0, HumanCommandDecision::AllowOnce, "accept"),
    (1, HumanCommandDecision::Decline, "decline"),
    (2, HumanCommandDecision::Cancel, "cancel"),
  ] {
    let id = format!("human-{index}");
    let request = human(&id);
    peer.send(request.clone()).await;
    let (approval, answer) = received(&mut reviews).await;
    assert_eq!(approval.command, "printf 'exact fixture'");
    assert_eq!(approval.cwd, "/workspace");
    assert_eq!(approval.reason.as_deref(), Some("Managed human checkpoint"));
    assert_eq!(approval.environment_id.as_deref(), Some("native-environment"));
    assert_eq!(
      approval.additional_permissions,
      Some(json!({"fileSystem":{"read":["/workspace"]}}))
    );
    assert_eq!(approval.thread_id, "thread-one");
    assert_eq!(approval.turn_id, "original-turn");
    assert_eq!(approval.item_id, "command-item");
    assert_eq!(approval.approval_id, format!("approval-{id}"));
    assert!(!approval.cancellation.is_cancelled());
    let Reply::Interactions(pending) = adapter
      .execute(super::tests::request(
        &adapter,
        "pending",
        Some("thread-one"),
        Command::PendingInteractions,
      ))
      .await
      .unwrap()
    else {
      panic!()
    };
    assert!(pending.is_empty());
    answer.send(decision).unwrap();
    assert_eq!(peer.next().await, json!({"id":id,"result":{"decision":native}}));
    peer.send(request).await;
    peer.quiet().await;
    assert!(
      reviews.try_recv().is_err(),
      "A resolved callback must never be replayed or re-reviewed"
    );
  }
  assert!(
    events.try_recv().is_err(),
    "Human request details must not enter generic runtime events"
  );
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn missing_reviewer_and_attached_transport_never_advertise_or_review() {
  for ownership in [Ownership::Managed, Ownership::Attached] {
    let (reviewer, mut reviews) = reviewer();
    let (adapter, mut peer) = managed((ownership == Ownership::Attached).then_some(reviewer), ownership).await;
    peer.send(human("unadvertised")).await;
    assert_eq!(
      peer.next().await,
      json!({"id":"unadvertised","result":{"decision":"decline"}})
    );
    assert!(reviews.try_recv().is_err());
    adapter.close().await.unwrap();
  }
}

#[tokio::test]
async fn malformed_requests_decline_without_review_or_generic_leak() {
  let (reviewer, mut reviews) = reviewer();
  let (adapter, mut peer) = managed(Some(reviewer), Ownership::Managed).await;
  let mut events = adapter.subscribe();
  for (index, field, value) in [
    (0, "approvalId", Value::Null),
    (1, "command", json!("")),
    (2, "cwd", json!("relative")),
    (
      3,
      "availableDecisions",
      json!(["accept", "acceptForSession", "decline"]),
    ),
  ] {
    let id = format!("malformed-{index}");
    let mut request = human(&id);
    request["params"][field] = value;
    peer.send(request).await;
    assert_eq!(peer.next().await, json!({"id":id,"result":{"decision":"decline"}}));
  }
  assert!(reviews.try_recv().is_err());
  assert!(events.try_recv().is_err());
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn exact_reannouncement_and_duplicate_business_identity_never_redirect_human_grant() {
  let (reviewer, mut reviews) = reviewer();
  let (adapter, mut peer) = managed(Some(reviewer), Ownership::Managed).await;
  let request = human("original");
  peer.send(request.clone()).await;
  let (_, answer) = received(&mut reviews).await;
  peer.send(request.clone()).await;
  let mut duplicate = request;
  duplicate["id"] = json!("new-envelope");
  peer.send(duplicate).await;
  assert_eq!(
    peer.next().await,
    json!({"id":"new-envelope","result":{"decision":"decline"}})
  );
  assert!(reviews.try_recv().is_err());
  answer.send(HumanCommandDecision::AllowOnce).unwrap();
  assert_eq!(
    peer.next().await,
    json!({"id":"original","result":{"decision":"accept"}})
  );
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn native_resolution_uses_exact_thread_and_request_and_cancels_reviewer_without_reply() {
  let (reviewer, mut reviews) = reviewer();
  let (adapter, mut peer) = managed(Some(reviewer), Ownership::Managed).await;
  peer.send(human("pending")).await;
  let (mut approval, answer) = received(&mut reviews).await;
  peer
    .send(json!({"method":"serverRequest/resolved","params":{"threadId":"other-thread","requestId":"pending"}}))
    .await;
  peer.send(json!({"method":"turn/completed","params":{"threadId":"thread-one","turn":{"id":"newer-turn","status":"completed"}}})).await;
  peer.quiet().await;
  assert!(!approval.cancellation.is_cancelled());
  peer
    .send(json!({"method":"serverRequest/resolved","params":{"threadId":"thread-one","requestId":"pending"}}))
    .await;
  tokio::time::timeout(Duration::from_secs(2), approval.cancellation.cancelled())
    .await
    .unwrap();
  let _ = answer.send(HumanCommandDecision::AllowOnce);
  peer.quiet().await;
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn close_cancels_human_reviewer_and_never_grants() {
  let (reviewer, mut reviews) = reviewer();
  let (adapter, mut peer) = managed(Some(reviewer), Ownership::Managed).await;
  peer.send(human("pending")).await;
  let (mut approval, answer) = received(&mut reviews).await;
  adapter.close().await.unwrap();
  tokio::time::timeout(Duration::from_secs(2), approval.cancellation.cancelled())
    .await
    .unwrap();
  let _ = answer.send(HumanCommandDecision::AllowOnce);
  assert!(peer.reader.read_frame().await.unwrap().is_none());
}

#[tokio::test]
async fn malformed_human_notification_never_enters_generic_events() {
  let (reviewer, mut reviews) = reviewer();
  let (adapter, mut peer) = managed(Some(reviewer), Ownership::Managed).await;
  let mut events = adapter.subscribe();
  let mut request = human("notification");
  request.as_object_mut().unwrap().remove("id");
  peer.send(request).await;
  let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
    .await
    .unwrap()
    .unwrap();
  assert!(matches!(event.kind, RuntimeEventKind::Disconnected { .. }));
  assert!(events.try_recv().is_err());
  assert!(reviews.try_recv().is_err());
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn human_request_id_cannot_be_reused_for_a_generic_interaction() {
  let (reviewer, mut reviews) = reviewer();
  let (adapter, mut peer) = managed(Some(reviewer), Ownership::Managed).await;
  peer.send(human("same-id")).await;
  let (mut approval, answer) = received(&mut reviews).await;
  peer
    .send(
      json!({"id":"same-id","method":"item/fileChange/requestApproval","params":{
        "threadId":"thread-one","turnId":"other-turn","itemId":"file-item"
      }}),
    )
    .await;
  tokio::time::timeout(Duration::from_secs(2), approval.cancellation.cancelled())
    .await
    .unwrap();
  let _ = answer.send(HumanCommandDecision::AllowOnce);
  assert!(peer.reader.read_frame().await.unwrap().is_none());
  adapter.close().await.unwrap();
}

#[tokio::test]
async fn bounded_review_capacity_declines_excess_requests() {
  let (reviewer, mut reviews) = reviewer();
  let (adapter, mut peer) = managed(Some(reviewer), Ownership::Managed).await;
  let mut active = Vec::new();
  for index in 0..16 {
    peer.send(human(&format!("active-{index}"))).await;
    active.push(received(&mut reviews).await);
  }
  peer.send(human("overflow")).await;
  assert_eq!(
    peer.next().await,
    json!({"id":"overflow","result":{"decision":"decline"}})
  );
  assert!(reviews.try_recv().is_err());
  adapter.close().await.unwrap();
}
