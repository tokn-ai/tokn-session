use std::future::Future;

use super::*;

#[cfg(any(unix, windows))]
struct FakeRouter {
  endpoint: PathBuf,
  #[cfg(unix)]
  listener: tokio::net::UnixListener,
  #[cfg(unix)]
  _directory: tempfile::TempDir,
  #[cfg(windows)]
  listener: tokio::net::windows::named_pipe::NamedPipeServer,
}

#[cfg(any(unix, windows))]
impl FakeRouter {
  fn new() -> Self {
    #[cfg(unix)]
    {
      let directory = tempfile::Builder::new().prefix("tokn-ipc-").tempdir_in("/tmp").unwrap();
      let endpoint = directory.path().join("router.sock");
      let listener = tokio::net::UnixListener::bind(&endpoint).unwrap();
      Self {
        endpoint,
        listener,
        _directory: directory,
      }
    }
    #[cfg(windows)]
    {
      let endpoint = PathBuf::from(format!(r"\\.\pipe\tokn-viewer-test-{}", Uuid::new_v4()));
      let listener = tokio::net::windows::named_pipe::ServerOptions::new()
        .first_pipe_instance(true)
        .create(&endpoint)
        .unwrap();
      Self { endpoint, listener }
    }
  }

  async fn accept(self) -> Box<dyn IpcStream> {
    #[cfg(unix)]
    {
      Box::new(self.listener.accept().await.unwrap().0)
    }
    #[cfg(windows)]
    {
      self.listener.connect().await.unwrap();
      Box::new(self.listener)
    }
  }
}

fn success(request: &Value, result: Value) -> Value {
  json!({
    "type": "response",
    "requestId": request["requestId"],
    "resultType": "success",
    "method": request["method"],
    "handledByClientId": "desktop-owner",
    "result": result
  })
}

async fn write_frame(stream: &mut Box<dyn IpcStream>, value: &Value) {
  stream.write_all(&encode_frame(value).unwrap()).await.unwrap();
}

#[cfg(any(unix, windows))]
async fn initialized_client<F, Fut>(handler: F) -> (DesktopClient, tokio::task::JoinHandle<()>)
where
  F: FnOnce(Box<dyn IpcStream>) -> Fut + Send + 'static,
  Fut: Future<Output = ()> + Send + 'static,
{
  let router = FakeRouter::new();
  let endpoint = router.endpoint.clone();
  let server = tokio::spawn(async move {
    let mut stream = router.accept().await;
    let request = read_frame(&mut stream).await.unwrap();
    assert_eq!(request["type"], "request");
    assert_eq!(request["method"], "initialize");
    assert_eq!(request["version"], 0);
    assert_eq!(request["sourceClientId"], "initializing-client");
    assert_eq!(request["params"]["clientType"], "tokn-session-viewer");
    write_frame(&mut stream, &success(&request, json!({"clientId": "viewer-client"}))).await;
    handler(stream).await;
  });
  let mut client = DesktopClient::connect(&endpoint, REQUEST_TIMEOUT).await.unwrap();
  client.request_timeout = SUBMISSION_TIMEOUT;
  (client, server)
}

#[tokio::test]
#[cfg(any(unix, windows))]
async fn sends_one_message_to_the_owner_and_declines_discovery() {
  let (mut client, server) = initialized_client(|mut stream| async move {
    let request = read_frame(&mut stream).await.unwrap();
    assert_eq!(request["method"], START_TURN_METHOD);
    assert_eq!(request["version"], 2);
    assert_eq!(request["sourceClientId"], "viewer-client");
    assert_eq!(
      request["params"],
      json!({
        "conversationId": "session-123",
        "turnStart": {
          "request": {
            "threadId": "session-123",
            "input": [{ "type": "text", "text": "First line\n第二行", "text_elements": [] }],
            "clientUserMessageId": "viewer-request-123",
            "additionalContext": null
          },
          "context": { "inheritThreadSettings": true }
        }
      })
    );
    assert_ne!(request["requestId"], "viewer-request-123");
    assert_eq!(request["timeoutMs"], 20_000);
    let discovery = encode_frame(&json!({
      "type": "client-discovery-request", "requestId": "discovery-123", "request": request
    }))
    .unwrap();
    // Split the framing header and body across writes.
    stream.write_all(&discovery[..2]).await.unwrap();
    stream.write_all(&discovery[2..7]).await.unwrap();
    stream.write_all(&discovery[7..]).await.unwrap();
    assert_eq!(
      read_frame(&mut stream).await.unwrap(),
      json!({
        "type": "client-discovery-response",
        "requestId": "discovery-123",
        "response": { "canHandle": false }
      })
    );
    let mut unrelated = success(&request, json!({}));
    unrelated["requestId"] = json!("another-request");
    let mut frames = encode_frame(&unrelated).unwrap();
    frames.extend(encode_frame(&success(&request, json!({"ok": true}))).unwrap());
    stream.write_all(&frames).await.unwrap();
  })
  .await;
  let result = client
    .submit("session-123", "viewer-request-123", "First line\n第二行")
    .await
    .unwrap();
  assert!(matches!(result, Admission::Accepted));
  server.await.unwrap();
}

#[tokio::test]
#[cfg(any(unix, windows))]
async fn submitted_text_satisfies_desktops_rendering_contract_before_normalization() {
  // Desktop 26.901.41123 keeps request.input in the local turn and reads
  // text_elements.length while projecting it. Unlike the app-server parser,
  // this contract must reject an omitted annotation array instead of defaulting.
  #[derive(serde::Deserialize)]
  #[serde(tag = "type", rename_all = "snake_case")]
  enum DesktopInput {
    Text { text: String, text_elements: Vec<Value> },
  }
  for invalid in [
    json!({ "type": "text", "text": "hello" }),
    json!({ "type": "text", "text": "hello", "text_elements": null }),
    json!({ "type": "text", "text": "hello", "text_elements": "" }),
  ] {
    assert!(serde_json::from_value::<DesktopInput>(invalid).is_err());
  }
  const TEXT: &str = "  **Hello**\n\n```ts\nconst text = '你好 👋';\n```\n";
  let (mut client, server) = initialized_client(|mut stream| async move {
    let request = read_frame(&mut stream).await.unwrap();
    let input: Vec<DesktopInput> =
      serde_json::from_value(request["params"]["turnStart"]["request"]["input"].clone()).unwrap();
    let rendered = input
      .into_iter()
      .map(|DesktopInput::Text { text, text_elements }| {
        assert!(text_elements.is_empty(), "plain text has no inline annotations");
        text
      })
      .collect::<Vec<_>>()
      .join("\n");
    assert_eq!(rendered, TEXT);
    write_frame(&mut stream, &success(&request, json!({}))).await;
  })
  .await;
  assert!(matches!(
    client.submit("session", "request", TEXT).await.unwrap(),
    Admission::Accepted
  ));
  server.await.unwrap();
}

#[tokio::test]
#[cfg(any(unix, windows))]
async fn initialization_probe_never_submits_a_message() {
  let (client, server) = initialized_client(|mut stream| async move {
    let mut byte = [0];
    assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
  })
  .await;
  drop(client);
  server.await.unwrap();
}

#[tokio::test]
#[cfg(any(unix, windows))]
async fn missing_owner_is_a_definite_non_delivery() {
  let (mut client, server) = initialized_client(|mut stream| async move {
    let request = read_frame(&mut stream).await.unwrap();
    write_frame(
      &mut stream,
      &json!({
        "type": "response", "requestId": request["requestId"],
        "resultType": "error", "error": "no-client-found"
      }),
    )
    .await;
  })
  .await;
  let error = client.submit("session", "id", "hello").await.unwrap_err();
  assert!(!error.may_have_been_sent);
  assert!(error.message.contains("No compatible Codex Desktop owner"));
  server.await.unwrap();
}

#[tokio::test]
#[cfg(any(unix, windows))]
async fn owner_errors_are_not_proof_of_non_delivery() {
  let (mut client, server) = initialized_client(|mut stream| async move {
    let request = read_frame(&mut stream).await.unwrap();
    write_frame(
      &mut stream,
      &json!({
        "type": "response", "requestId": request["requestId"],
        "resultType": "error", "error": "owner failed after accepting input"
      }),
    )
    .await;
  })
  .await;
  let error = client.submit("session", "id", "hello").await.unwrap_err();
  assert!(error.may_have_been_sent);
  assert!(error.message.contains("owner failed"));
  server.await.unwrap();
}

#[tokio::test]
#[cfg(any(unix, windows))]
async fn unsupported_versions_and_methods_are_definite_rejections() {
  for error in ["request-version-mismatch", "no-handler-for-request"] {
    let (mut client, server) = initialized_client(move |mut stream| async move {
      let request = read_frame(&mut stream).await.unwrap();
      write_frame(
        &mut stream,
        &json!({
          "type": "response", "requestId": request["requestId"], "resultType": "error", "error": error
        }),
      )
      .await;
      let mut byte = [0];
      assert_eq!(
        stream.read(&mut byte).await.unwrap(),
        0,
        "must not retry another protocol version"
      );
    })
    .await;
    let result = client.submit("session", "id", "hello").await.unwrap_err();
    assert!(!result.may_have_been_sent);
    assert!(result.message.contains(error));
    drop(client);
    server.await.unwrap();
  }
}

#[tokio::test]
#[cfg(any(unix, windows))]
async fn success_without_an_optional_result_still_confirms_admission() {
  let (mut client, server) = initialized_client(|mut stream| async move {
    let request = read_frame(&mut stream).await.unwrap();
    let mut response = success(&request, Value::Null);
    response.as_object_mut().unwrap().remove("result");
    write_frame(&mut stream, &response).await;
  })
  .await;
  assert!(matches!(
    client.submit("session", "id", "hello").await.unwrap(),
    Admission::Accepted
  ));
  server.await.unwrap();
}

#[tokio::test]
#[cfg(any(unix, windows))]
async fn incompatible_responses_do_not_claim_delivery_or_retry() {
  for variant in 0..5 {
    let (mut client, server) = initialized_client(move |mut stream| async move {
      let request = read_frame(&mut stream).await.unwrap();
      let mut response = success(&request, json!({}));
      match variant {
        0 => response["method"] = json!("a-different-method"),
        1 => response["resultType"] = json!("future-response"),
        2 => response["handledByClientId"] = Value::Null,
        3 => {
          response["resultType"] = json!("error");
          response["error"] = json!({ "code": "new-shape" });
        }
        _ => response["requestId"] = Value::Null,
      }
      write_frame(&mut stream, &response).await;
      let mut byte = [0];
      assert_eq!(
        stream.read(&mut byte).await.unwrap(),
        0,
        "must not resend after invalid response"
      );
    })
    .await;
    let error = client.submit("session", "id", "hello").await.unwrap_err();
    assert!(error.may_have_been_sent, "variant {variant}");
    drop(client);
    server.await.unwrap();
  }
}

#[tokio::test]
#[cfg(any(unix, windows))]
async fn disconnects_and_timeouts_after_sending_remain_uncertain() {
  let (mut client, server) = initialized_client(|mut stream| async move {
    read_frame(&mut stream).await.unwrap();
  })
  .await;
  assert!(
    client
      .submit("session", "id", "hello")
      .await
      .unwrap_err()
      .may_have_been_sent
  );
  server.await.unwrap();

  let (mut client, server) = initialized_client(|mut stream| async move {
    read_frame(&mut stream).await.unwrap();
    let mut byte = [0];
    assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
  })
  .await;
  client.request_timeout = Duration::from_millis(50);
  let error = client.submit("session", "id", "hello").await.unwrap_err();
  assert!(error.may_have_been_sent);
  assert!(error.message.contains("timed out"));
  drop(client);
  server.await.unwrap();
}

#[tokio::test]
#[cfg(any(unix, windows))]
async fn oversized_received_frame_is_bounded_and_uncertain() {
  let (mut client, server) = initialized_client(|mut stream| async move {
    read_frame(&mut stream).await.unwrap();
    stream
      .write_all(&((MAX_FRAME_BYTES + 1) as u32).to_le_bytes())
      .await
      .unwrap();
  })
  .await;
  let error = client.submit("session", "id", "hello").await.unwrap_err();
  assert!(error.may_have_been_sent);
  assert!(error.message.contains("frame size"));
  server.await.unwrap();
}

#[tokio::test]
#[cfg(any(unix, windows))]
async fn missing_endpoint_is_not_a_delivery_attempt() {
  let router = FakeRouter::new();
  let endpoint = router.endpoint.clone();
  drop(router);
  let error = match DesktopClient::connect(&endpoint, REQUEST_TIMEOUT).await {
    Ok(_) => panic!("missing endpoint must fail"),
    Err(error) => error,
  };
  assert!(!error.may_have_been_sent);
  assert!(error.message.contains("unavailable"));
}

#[tokio::test]
#[cfg(any(unix, windows))]
async fn invalid_initialization_is_not_a_delivery_attempt() {
  let router = FakeRouter::new();
  let endpoint = router.endpoint.clone();
  let server = tokio::spawn(async move {
    let mut stream = router.accept().await;
    let request = read_frame(&mut stream).await.unwrap();
    write_frame(&mut stream, &success(&request, json!({ "futureClientKey": "unknown" }))).await;
    let mut byte = [0];
    assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
  });
  let error = match DesktopClient::connect(&endpoint, REQUEST_TIMEOUT).await {
    Ok(_) => panic!("invalid initialization must fail"),
    Err(error) => error,
  };
  assert!(!error.may_have_been_sent);
  server.await.unwrap();
}

#[tokio::test]
#[cfg(any(unix, windows))]
async fn invalid_or_oversized_input_never_writes_a_message() {
  let (mut client, server) = initialized_client(|mut stream| async move {
    let mut byte = [0];
    assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
  })
  .await;
  for text in [String::new(), "x".repeat(MAX_FRAME_BYTES)] {
    let error = client.submit("session", "id", &text).await.unwrap_err();
    assert!(!error.may_have_been_sent);
  }
  drop(client);
  server.await.unwrap();
}

#[tokio::test]
async fn rejects_invalid_json_and_zero_length_frames() {
  for frame in [vec![0, 0, 0, 0], vec![1, 0, 0, 0, b'{']] {
    let mut bytes = frame.as_slice();
    assert!(read_frame(&mut bytes).await.is_err());
  }
}

#[tokio::test]
#[cfg(any(unix, windows))]
async fn async_answers_steer_with_display_context_and_never_start_another_turn() {
  let (mut client, server) = initialized_client(|mut stream| async move {
    let request = read_frame(&mut stream).await.unwrap();
    assert_eq!(request["method"], "thread-follower-steer-turn");
    assert_eq!(request["version"], 1);
    assert_eq!(request["params"]["conversationId"], "session");
    assert_eq!(request["params"]["input"][0]["text"], "answer envelope");
    assert_eq!(request["params"]["input"][0]["text_elements"], json!([]));
    assert_eq!(request["params"]["clientUserMessageId"], "attempt");
    assert_eq!(
      request["params"]["restoreMessage"]["context"]["commentAttachments"],
      json!([])
    );
    write_frame(&mut stream, &success(&request, json!({"result":{"turnId":"active"}}))).await;
  })
  .await;
  assert!(matches!(
    client
      .submit_question("session", "attempt", "answer envelope")
      .await
      .unwrap(),
    Admission::Accepted
  ));
  server.await.unwrap();
}
