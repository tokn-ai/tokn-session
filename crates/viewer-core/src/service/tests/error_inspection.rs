use super::*;

fn pi_error_record(id: &str) -> Value {
  json!({
    "type":"message", "id":id, "parentId":format!("parent-{id}"),
    "timestamp":"2026-10-19T00:00:02.000Z",
    "message":{"role":"assistant", "content":[], "stopReason":"error",
      "errorMessage":"Connection error.", "provider":"zai", "model":"glm-5.2",
      "diagnostics":{"code":"CONNECTION_RESET", "requestId":format!("request-{id}")}}
  })
}

fn write_jsonl(path: &Path, records: &[Value]) {
  std::fs::write(
    path,
    records.iter().map(|record| format!("{record}\n")).collect::<String>(),
  )
  .unwrap();
}

fn inspection_request(locator: &SessionLocator, index: usize) -> LoadEventDetailRequest {
  LoadEventDetailRequest {
    session_key: encode_session_key(locator).unwrap(),
    event_key: encode_event_key(index),
  }
}

/// Every fixture repeats the visible error text and timestamp, so neither can
/// stand in for the exact source record when resolving Inspector details.
fn error_history(provider: ViewerProvider) -> (tempfile::TempDir, SessionLocator, Vec<Value>) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join(
    if matches!(provider, ViewerProvider::OpenCode | ViewerProvider::ZCode) {
      "errors.db"
    } else {
      "fixture.jsonl"
    },
  );
  let mut expected = Vec::new();
  match provider {
    ViewerProvider::OpenCode | ViewerProvider::ZCode => {
      let connection = rusqlite::Connection::open(&path).unwrap();
      connection.execute_batch(
        "CREATE TABLE session(id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER);
         CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
         CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);
         INSERT INTO session VALUES('fixture', NULL, '/synthetic/project', 'Errors', 100, 101);",
      ).unwrap();
      for id in ["first", "second"] {
        let data = json!({"role":"assistant", "parentID":format!("parent-{id}"),
          "error":{"name":"APIError", "data":{"message":"Connection error.",
            "code":"CONNECTION_RESET", "requestId":format!("request-{id}")}}});
        connection
          .execute(
            "INSERT INTO message VALUES(?1, 'fixture', 101, ?2)",
            rusqlite::params![id, data.to_string()],
          )
          .unwrap();
        expected.push(json!({"id":id, "data":data}));
      }
    }
    _ => {
      let mut records = match provider {
        ViewerProvider::Codex => vec![json!({"type":"session_meta", "payload":{"id":"fixture"}})],
        ViewerProvider::Pi => vec![json!({"type":"session", "id":"fixture", "cwd":"/synthetic/project"})],
        ViewerProvider::Dsh => vec![json!({"type":"session", "version":0, "id":"fixture",
          "createdAt":0, "delegationDepth":0})],
        ViewerProvider::WorkBuddy => vec![],
        _ => unreachable!(),
      };
      for (index, id) in ["first", "second"].into_iter().enumerate() {
        let native = match provider {
          ViewerProvider::Codex => json!({"type":"event_msg",
            "timestamp":"2026-10-19T00:00:02.000Z", "payload":{"type":"error", "id":id,
              "parent_id":format!("parent-{id}"), "message":"Connection error.",
              "details":{"code":"CONNECTION_RESET", "requestId":format!("request-{id}")}}}),
          ViewerProvider::Pi => pi_error_record(id),
          ViewerProvider::Dsh => json!({"type":"turn/end", "seq":index + 1, "time":101,
            "data":{"turn":index + 1, "reason":{"kind":"error", "error":{"message":"Connection error.",
              "code":"CONNECTION_RESET", "requestId":format!("request-{id}")}}}}),
          ViewerProvider::WorkBuddy => json!({"type":"message", "id":id, "parentId":format!("parent-{id}"),
            "sessionId":"fixture", "timestamp":101, "role":"assistant", "status":"incomplete", "content":[],
            "providerData":{"error":{"message":"Connection error.", "details":{"code":"CONNECTION_RESET",
              "requestId":format!("request-{id}")}}}}),
          _ => unreachable!(),
        };
        records.push(native.clone());
        expected.push(if provider == ViewerProvider::Codex {
          native["payload"].clone()
        } else {
          native
        });
      }
      write_jsonl(&path, &records);
    }
  }
  let locator = SessionLocator {
    version: 1,
    provider,
    session_id: "fixture".into(),
    source_path: path,
  };
  (directory, locator, expected)
}

#[test]
fn error_inspection_uses_each_provider_record_without_optional_native_loading() {
  for provider in [
    ViewerProvider::Codex,
    ViewerProvider::Pi,
    ViewerProvider::OpenCode,
    ViewerProvider::ZCode,
    ViewerProvider::WorkBuddy,
    ViewerProvider::Dsh,
  ] {
    let (_directory, locator, expected) = error_history(provider);
    let service = ViewerService::new(Arc::new(NativeRepository::default()));
    assert!(!service.relay.status().settings.include_native);
    assert!(!service.relay.covers(provider));
    let loaded = service.load_verified(&locator).unwrap();
    let indices = loaded
      .events
      .iter()
      .enumerate()
      .filter_map(|(index, event)| matches!(event, AgentEvent::Error(_)).then_some(index))
      .collect::<Vec<_>>();
    assert_eq!(indices.len(), expected.len(), "{provider:?}");
    for (index, native) in indices.into_iter().zip(expected) {
      let detail = service
        .inspect_session_event(inspection_request(&locator, index))
        .unwrap();
      assert_eq!(detail.event_key, encode_event_key(index));
      assert_eq!(detail.event["type"], "error");
      assert_eq!(detail.event["message"], "Connection error.");
      assert!(detail.event.get("native").is_none());
      assert_eq!(detail.native, Some(native), "{provider:?}");
      assert!(!detail.is_hidden);
    }
  }
}

#[test]
fn legacy_error_without_native_never_borrows_an_adjacent_diagnostic() {
  let diagnostic = |id: &str| {
    AgentEvent::Metadata(MetadataEvent {
      provider: Provider::Pi,
      session_id: Some("fixture".into()),
      kind: MetadataKind::Diagnostic,
      native_type: "message".into(),
      summary: "Connection error.".into(),
      native: pi_error_record(id),
      timestamp: None,
    })
  };
  let service = service_with_session(loaded_session(vec![
    diagnostic("unrelated-before"),
    AgentEvent::Error(ErrorEvent {
      provider: Provider::Pi,
      session_id: Some("fixture".into()),
      message: "Connection error.".into(),
      native: None,
      timestamp: None,
    }),
    diagnostic("unrelated-after"),
  ]));
  let detail = service
    .inspect_session_event(LoadEventDetailRequest {
      session_key: key_for("fixture"),
      event_key: encode_event_key(1),
    })
    .unwrap();
  assert_eq!(detail.event["message"], "Connection error.");
  assert!(detail.event.get("native").is_none());
  assert!(detail.native.is_none());
}

#[test]
fn oversized_error_native_keeps_the_existing_inspector_bound() {
  let service = service_with_session(loaded_session(vec![AgentEvent::Error(ErrorEvent {
    provider: Provider::Pi,
    session_id: Some("fixture".into()),
    message: "Connection error.".into(),
    native: Some(json!({"id":"oversized", "details":"x".repeat(MAX_DETAIL_VALUE_BYTES + 1)})),
    timestamp: None,
  })]));
  let detail = service
    .inspect_session_event(LoadEventDetailRequest {
      session_key: key_for("fixture"),
      event_key: encode_event_key(0),
    })
    .unwrap();
  assert_eq!(detail.event["type"], "error");
  assert_eq!(detail.event["message"], "Connection error.");
  assert!(detail.event.get("native").is_none());
  let native = detail.native.as_ref().unwrap();
  assert_eq!(native["truncated"], true);
  assert_eq!(native["representation"], "provider_native");
  assert_eq!(native["limit_bytes"], MAX_DETAIL_VALUE_BYTES);
  assert!(native["original_size_bytes"].as_u64().unwrap() > MAX_DETAIL_VALUE_BYTES as u64);
  assert!(serde_json::to_vec(&detail).unwrap().len() < 4 * 1024);
}

#[tokio::test]
async fn pi_relay_error_inspection_keeps_native_and_the_displayed_snapshot() {
  use std::{io::Write, time::Duration};
  use tokn_session_relay::{ProviderRoot, RelayConfig};

  for include_native in [false, true] {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("errors.jsonl");
    let first = pi_error_record("first");
    let second = pi_error_record("second");
    write_jsonl(
      &path,
      &[
        json!({"type":"session", "id":"fixture", "timestamp":"2026-10-19T00:00:00Z", "cwd":"/tmp"}),
        first.clone(),
      ],
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("tcp://{}", listener.local_addr().unwrap());
    let mut config = RelayConfig::new(vec![ProviderRoot::new(Provider::Pi, directory.path().into())]);
    config.include_native = include_native;
    config.poll_interval = Duration::from_millis(10);
    let server = tokio::spawn(crate::service_server::serve_listener(listener, config));
    let service = ViewerService::new(Arc::new(NativeRepository::default()));
    let mut changes = service.relay.changes.subscribe();
    service
      .relay
      .configure(crate::relay::RelaySettings {
        mode: crate::relay::RelayMode::External,
        endpoint,
        include_native,
      })
      .unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
      while !service.relay.has_catalog() {
        changes.recv().await.unwrap();
      }
    })
    .await
    .unwrap();
    let locator = SessionLocator {
      version: 1,
      provider: ViewerProvider::Pi,
      session_id: "fixture".into(),
      source_path: path.clone(),
    };
    let session_key = encode_session_key(&locator).unwrap();
    let load_service = service.clone();
    let load_locator = locator.clone();
    let initial = tokio::task::spawn_blocking(move || load_service.load_verified(&load_locator))
      .await
      .unwrap()
      .unwrap();
    let first_index = initial
      .events
      .iter()
      .position(|event| matches!(event, AgentEvent::Error(_)))
      .unwrap();
    let second_index = initial.events.len();
    // load() can return between ready.notify_all() and the first publication.
    tokio::time::timeout(Duration::from_secs(4), async {
      while changes.recv().await.unwrap().session_key.as_deref() != Some(session_key.as_str()) {}
    })
    .await
    .unwrap();
    while changes.try_recv().is_ok() {}
    std::fs::OpenOptions::new()
      .append(true)
      .open(&path)
      .unwrap()
      .write_all(format!("{second}\n").as_bytes())
      .unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
      while changes.recv().await.unwrap().session_key.as_deref() != Some(session_key.as_str()) {}
    })
    .await
    .unwrap();
    assert!(service.relay.follow_error(&locator).is_none());
    let inspect = |index| {
      let service = service.clone();
      let request = inspection_request(&locator, index);
      async move {
        tokio::task::spawn_blocking(move || service.inspect_session_event(request))
          .await
          .unwrap()
      }
    };
    let pinned = inspect(first_index).await.unwrap();
    assert_eq!(pinned.native, Some(first.clone()));
    assert!(pinned.event.get("native").is_none());
    assert!(inspect(second_index).await.is_err());
    assert!(Arc::ptr_eq(&initial, &service.relay.load(&locator).unwrap()));
    let advance_relay = service.relay.clone();
    let advance_locator = locator.clone();
    let latest = tokio::task::spawn_blocking(move || advance_relay.advance(&advance_locator))
      .await
      .unwrap()
      .unwrap();
    assert!(!Arc::ptr_eq(&initial, &latest));
    assert_eq!(inspect(second_index).await.unwrap().native, Some(second));
    assert_eq!(inspect(first_index).await.unwrap().native, Some(first));
    server.abort();
    let _ = server.await;
  }
}
