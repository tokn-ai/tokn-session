use crate::*;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;
use tokn_opencode_rpc_protocol::SseDecoder;

#[tokio::test]
async fn local_model_gate_and_recording_proxy_preserve_request_causality() -> TestResult {
  let (step, gate) = ModelStep::gated_text("finished");
  let model = ModelServer::start(vec![step]).await?;
  let trace = Trace::default();
  let proxy = RecordingProxy::start(&model.url(), trace.clone()).await?;
  let response = reqwest::Client::builder()
    .no_proxy()
    .build()?
    .post(format!("{}chat/completions", proxy.url()))
    .bearer_auth("test-only-authorization-not-recorded")
    .json(&json!({"model":"rpc-test-model","messages":[{"role":"user","content":"hello"}],"stream":true}))
    .send()
    .await?;
  let mut body = response.bytes_stream();
  let mut decoder = SseDecoder::new(8192);
  loop {
    let bytes = tokio::time::timeout(Duration::from_secs(2), body.next())
      .await?
      .ok_or("model stream ended before gate")??;
    if decoder.push(&bytes)?.iter().any(|frame| frame.contains("working")) {
      break;
    }
  }
  assert_eq!(model.requests().len(), 1);
  assert!(matches!(trace.records().first(), Some(WireRecord::Http { .. })));
  gate.release();
  let mut rest = String::new();
  while let Some(bytes) = body.next().await {
    rest.push_str(std::str::from_utf8(&bytes?)?);
  }
  assert!(rest.contains("finished"));
  assert!(
    trace
      .records()
      .iter()
      .any(|record| matches!(record, WireRecord::Event { body, .. } if body.to_string().contains("finished")))
  );
  assert!(
    !serde_json::to_string(&trace.fixture("test", &"a".repeat(40), "proxy-order"))?.contains("test-only-authorization")
  );
  let exhausted = reqwest::Client::builder()
    .no_proxy()
    .build()?
    .post(format!("{}/chat/completions", model.url()))
    .json(&json!({"model":"rpc-test-model"}))
    .send()
    .await?;
  assert_eq!(exhausted.status(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);
  Ok(())
}

#[tokio::test]
async fn replay_releases_events_only_after_their_captured_mutations() -> TestResult {
  let fixture = Fixture {
    schema_version: 1,
    provider: "test".into(),
    upstream_revision: "a".repeat(40),
    scenario: "causal-replay".into(),
    records: vec![
      WireRecord::Http {
        method: "POST".into(),
        path: "/start".into(),
        request: json!({"text":"hello"}),
        status: 200,
        response: json!({"accepted":true}),
      },
      WireRecord::Event {
        channel: "/events".into(),
        body: json!({"type":"requested"}),
      },
      WireRecord::Http {
        method: "POST".into(),
        path: "/respond".into(),
        request: json!({"answer":"yes"}),
        status: 200,
        response: json!({"accepted":true}),
      },
      WireRecord::Event {
        channel: "/events".into(),
        body: json!({"type":"resolved"}),
      },
    ],
  };
  let replay = ReplayServer::start(fixture).await?;
  let client = reqwest::Client::builder().no_proxy().build()?;
  let mut events = client
    .get(format!("{}events", replay.url()))
    .send()
    .await?
    .bytes_stream();
  assert!(
    tokio::time::timeout(Duration::from_millis(40), events.next())
      .await
      .is_err()
  );
  client
    .post(format!("{}start", replay.url()))
    .json(&json!({"text":"hello"}))
    .send()
    .await?
    .error_for_status()?;
  let requested = tokio::time::timeout(Duration::from_secs(2), events.next())
    .await?
    .ok_or("missing requested frame")??;
  assert!(std::str::from_utf8(&requested)?.contains("requested"));
  assert!(
    tokio::time::timeout(Duration::from_millis(40), events.next())
      .await
      .is_err()
  );
  assert!(replay.assert_consumed().is_err());
  client
    .post(format!("{}respond", replay.url()))
    .json(&json!({"answer":"yes"}))
    .send()
    .await?
    .error_for_status()?;
  let resolved = tokio::time::timeout(Duration::from_secs(2), events.next())
    .await?
    .ok_or("missing resolved frame")??;
  assert!(std::str::from_utf8(&resolved)?.contains("resolved"));
  replay.assert_consumed()?;
  Ok(())
}

#[tokio::test]
async fn replay_rejects_changed_request_bodies_instead_of_claiming_a_match() -> TestResult {
  let fixture = Fixture {
    schema_version: 1,
    provider: "test".into(),
    upstream_revision: "a".repeat(40),
    scenario: "request-validation".into(),
    records: vec![WireRecord::Http {
      method: "POST".into(),
      path: "/send".into(),
      request: json!({"text":"captured"}),
      status: 200,
      response: Value::Null,
    }],
  };
  let replay = ReplayServer::start(fixture).await?;
  let response = reqwest::Client::builder()
    .no_proxy()
    .build()?
    .post(format!("{}send", replay.url()))
    .json(&json!({"text":"different"}))
    .send()
    .await?;
  assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
  assert!(replay.assert_consumed().is_err());
  Ok(())
}

#[test]
fn fixture_sanitization_covers_query_encoding_and_path_keys_without_losing_correlation() {
  let trace = Trace::default();
  trace.push(WireRecord::Http {method:"GET".into(),path:"/state?directory=%2Fprivate%2Ftest-home".into(),request:Value::Null,status:200,response:json!({"/private/test-home/file":{"session_id":"stable-id","path":"/private/test-home/session","apiKey":"dummy-do-not-save","authorization":"Bearer dummy-do-not-save"}})});
  let mut fixture = trace.fixture("test", &"a".repeat(40), "sanitize");
  fixture.sanitize(&[("/private/test-home", "/fixture/home")]);
  let encoded = serde_json::to_string(&fixture).unwrap();
  assert!(!encoded.contains("test-home"));
  assert!(encoded.contains("%2Ffixture%2Fhome"));
  assert!(encoded.contains("stable-id"));
  assert!(!encoded.contains("dummy-do-not-save"));
  assert!(encoded.contains("<redacted>"));
}

#[tokio::test]
async fn replay_uuid_bindings_preserve_distinct_request_identity_and_literal_text() -> TestResult {
  let first = "00000000-0000-4000-8000-000000000001";
  let second = "00000000-0000-4000-8000-000000000002";
  let fixture = Fixture {
    schema_version: 1,
    provider: "test".into(),
    upstream_revision: "a".repeat(40),
    scenario: "unique-identity".into(),
    records: vec![
      WireRecord::Http {
        method: "POST".into(),
        path: "/send".into(),
        request: json!({"id":first,"text":first}),
        status: 200,
        response: json!({"id":first}),
      },
      WireRecord::Http {
        method: "POST".into(),
        path: "/send".into(),
        request: json!({"id":second,"text":first}),
        status: 200,
        response: json!({"id":second}),
      },
    ],
  };
  let replay = ReplayServer::start(fixture).await?;
  let client = reqwest::Client::builder().no_proxy().build()?;
  let endpoint = format!("{}send", replay.url());
  client
    .post(&endpoint)
    .json(&json!({"id":first,"text":first}))
    .send()
    .await?
    .error_for_status()?;
  let reused = client
    .post(&endpoint)
    .json(&json!({"id":first,"text":first}))
    .send()
    .await?;
  assert_eq!(reused.status(), reqwest::StatusCode::CONFLICT);
  let changed_text = client
    .post(&endpoint)
    .json(&json!({"id":second,"text":second}))
    .send()
    .await?;
  assert_eq!(changed_text.status(), reqwest::StatusCode::CONFLICT);
  let invalid_id = client
    .post(&endpoint)
    .json(&json!({"id":"not-a-uuid","text":first}))
    .send()
    .await?;
  assert_eq!(invalid_id.status(), reqwest::StatusCode::CONFLICT);
  Ok(())
}
