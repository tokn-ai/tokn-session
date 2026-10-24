use super::*;
use crate::{
  model::InputDeliveryStatus,
  rpc_profiles::{RpcRuntimeMode, RpcRuntimeProfileInput, RpcSaveProfileRequest},
  rpc_runtime::tests::CodexPeer,
};

fn target(directory: &std::path::Path) -> VerifiedSession {
  VerifiedSession {
    session_key: "verified-codex-session".into(),
    input: InputTarget {
      provider: ViewerProvider::Codex,
      session_id: "supervisor-native-thread".into(),
      session_file: directory.join("history.jsonl"),
    },
    cwd: directory.to_path_buf(),
  }
}

fn configure(supervisor: &SessionSupervisor, endpoint: String) {
  supervisor
    .runtimes
    .profiles()
    .unwrap()
    .save(RpcSaveProfileRequest {
      expected_revision: 0,
      profile: RpcRuntimeProfileInput {
        profile_id: None,
        name: "Supervisor test Codex".into(),
        provider: Provider::Codex,
        mode: RpcRuntimeMode::Attached,
        executable: None,
        arguments: Vec::new(),
        working_directory: None,
        endpoint: Some(endpoint),
        environment: None,
        credentials: None,
        is_default: true,
      },
    })
    .unwrap();
}

fn request() -> SubmitSessionInputRequest {
  SubmitSessionInputRequest {
    session_key: "verified-codex-session".into(),
    request_id: uuid::Uuid::new_v4().to_string(),
    text: "one supervisor prompt".into(),
    image_ids: Vec::new(),
    model_selection: None,
  }
}

async fn send(
  broker: &input::InputBroker,
  supervisor: &SessionSupervisor,
  directory: &std::path::Path,
  request: SubmitSessionInputRequest,
) -> crate::model::SubmitSessionInputResponse {
  let target = target(directory);
  let identity = InputTarget {
    provider: target.input.provider,
    session_id: target.input.session_id.clone(),
    session_file: target.input.session_file.clone(),
  };
  let supervisor = supervisor.clone();
  let delivery = request.clone();
  broker
    .submit_custom(&identity, request, async move {
      supervisor
        .submit(target, delivery, Vec::new(), SessionTransport::Rpc)
        .await
    })
    .await
}

#[tokio::test]
async fn codex_default_rpc_inspection_does_not_connect_and_first_send_delivers_once() {
  let peer = CodexPeer::start(false).await;
  let directory = tempfile::tempdir().unwrap();
  let supervisor = SessionSupervisor::new(Some(directory.path().to_path_buf()), InteractionService::default()).unwrap();
  configure(&supervisor, peer.endpoint.clone());
  assert_eq!(
    supervisor.transport(&target(directory.path())).await.unwrap(),
    SessionTransport::Rpc
  );
  assert!(supervisor.input_status(&target(directory.path())).await.is_ok());
  assert!(peer.methods().is_empty(), "inspection activated a native runtime");
  let broker = input::InputBroker::default();
  let request = request();
  assert_eq!(
    send(&broker, &supervisor, directory.path(), request.clone())
      .await
      .status,
    InputDeliveryStatus::Accepted
  );
  assert_eq!(
    send(&broker, &supervisor, directory.path(), request).await.status,
    InputDeliveryStatus::Accepted
  );
  let methods = peer.methods();
  assert_eq!(
    methods
      .iter()
      .filter(|method| method.as_str() == "thread/resume")
      .count(),
    1
  );
  assert_eq!(
    methods.iter().filter(|method| method.as_str() == "turn/start").count(),
    1
  );
  assert_eq!(
    supervisor.transport(&target(directory.path())).await.unwrap(),
    SessionTransport::Rpc
  );
  assert!(supervisor.desktop_owners.lock().unwrap().is_empty());
  supervisor.shutdown().await;
}

#[tokio::test]
async fn lost_rpc_acknowledgement_stays_unknown_without_replay_or_desktop_fallback() {
  let peer = CodexPeer::start(false).await;
  peer.drop_prompt_acknowledgements();
  let directory = tempfile::tempdir().unwrap();
  let supervisor = SessionSupervisor::new(Some(directory.path().to_path_buf()), InteractionService::default()).unwrap();
  configure(&supervisor, peer.endpoint.clone());
  let broker = input::InputBroker::default();
  let request = request();
  assert_eq!(
    send(&broker, &supervisor, directory.path(), request.clone())
      .await
      .status,
    InputDeliveryStatus::Unknown
  );
  assert_eq!(
    send(&broker, &supervisor, directory.path(), request).await.status,
    InputDeliveryStatus::Unknown
  );
  assert!(broker.has_unresolved(&target(directory.path()).input));
  assert_eq!(
    peer
      .methods()
      .iter()
      .filter(|method| method.as_str() == "turn/start")
      .count(),
    1
  );
  assert_eq!(
    supervisor.transport(&target(directory.path())).await.unwrap(),
    SessionTransport::Rpc
  );
  assert!(supervisor.desktop_owners.lock().unwrap().is_empty());
  supervisor.shutdown().await;
}

#[tokio::test]
async fn control_receipt_cannot_alias_an_identical_message_request() {
  for control_first in [false, true] {
    let broker = input::InputBroker::default();
    let target = target(std::path::Path::new("/tmp")).input;
    let mut message = request();
    message.text = "session-control:cancel-turn".into();
    let first = if control_first {
      broker
        .submit_control(&target, message.clone(), async { Ok(Admission::Accepted) })
        .await
    } else {
      broker
        .submit_custom(&target, message.clone(), async { Ok(Admission::Accepted) })
        .await
    };
    assert_eq!(first.status, InputDeliveryStatus::Accepted);
    let other = if control_first {
      broker
        .submit_custom(&target, message, async {
          panic!("a control receipt must not authorize a message")
        })
        .await
    } else {
      broker
        .submit_control(&target, message, async {
          panic!("a message receipt must not authorize cancellation")
        })
        .await
    };
    assert_eq!(other.status, InputDeliveryStatus::NotSent);
  }
}

#[tokio::test]
async fn a_control_can_unblock_native_message_admission_without_waiting_for_its_receipt() {
  let broker = input::InputBroker::default();
  let (started, wait_started) = tokio::sync::oneshot::channel();
  let (answered, wait_answered) = tokio::sync::oneshot::channel();
  let worker = {
    let broker = broker.clone();
    tokio::spawn(async move {
      broker
        .submit_custom(&target(std::path::Path::new("/tmp")).input, request(), async move {
          started.send(()).unwrap();
          wait_answered.await.unwrap();
          Ok(Admission::Accepted)
        })
        .await
    })
  };
  wait_started.await.unwrap();
  let mut answer = request();
  answer.text = "recorded-control-answer".into();
  let result = tokio::time::timeout(
    std::time::Duration::from_secs(1),
    broker.submit_control(&target(std::path::Path::new("/tmp")).input, answer, async move {
      answered.send(()).unwrap();
      Ok(Admission::Accepted)
    }),
  )
  .await
  .expect("answer waited behind the message it must unblock");
  assert_eq!(result.status, InputDeliveryStatus::Accepted);
  assert_eq!(worker.await.unwrap().status, InputDeliveryStatus::Accepted);
  assert!(!broker.has_unresolved(&target(std::path::Path::new("/tmp")).input));
}

#[tokio::test]
async fn pending_control_blocks_new_message_and_route_change_without_duplicate_control_delivery() {
  let broker = input::InputBroker::default();
  let (started, wait_started) = tokio::sync::oneshot::channel();
  let (complete, wait_complete) = tokio::sync::oneshot::channel();
  let mut control = request();
  control.text = "held-control".into();
  let worker = {
    let broker = broker.clone();
    let control = control.clone();
    tokio::spawn(async move {
      broker
        .submit_control(&target(std::path::Path::new("/tmp")).input, control, async move {
          started.send(()).unwrap();
          wait_complete.await.unwrap();
          Ok(Admission::Accepted)
        })
        .await
    })
  };
  wait_started.await.unwrap();
  let target = target(std::path::Path::new("/tmp")).input;
  assert!(broker.has_unresolved(&target));
  assert_eq!(
    broker
      .submit_custom(&target, request(), async {
        panic!("new message raced pending cancellation")
      })
      .await
      .status,
    InputDeliveryStatus::NotSent
  );
  assert_eq!(
    broker
      .submit_control(&target, control, async { panic!("duplicate control was delivered") })
      .await
      .status,
    InputDeliveryStatus::Pending
  );
  complete.send(()).unwrap();
  assert_eq!(worker.await.unwrap().status, InputDeliveryStatus::Accepted);
  assert!(!broker.has_unresolved(&target));
}
