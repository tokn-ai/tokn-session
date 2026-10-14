use super::*;

#[test]
fn local_codex_cache_tracks_compressed_fallback_and_plain_reappearance() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("rollout-compressed.jsonl");
  let compressed_path = path.with_extension("jsonl.zst");
  let contents = format!(
    "{}\n{}\n",
    json!({"type": "session_meta", "payload": {"id": "compressed"}}),
    json!({"type": "event_msg", "payload": {"type": "user_message", "message": "compressed hello"}})
  );
  std::fs::write(
    &compressed_path,
    zstd::stream::encode_all(contents.as_bytes(), 0).unwrap(),
  )
  .unwrap();
  for source_path in [&path, &compressed_path] {
    let service = ViewerService::new(Arc::new(NativeRepository::default()));
    let locator = SessionLocator {
      version: 1,
      provider: ViewerProvider::Codex,
      source_path: source_path.clone(),
      session_id: "compressed".into(),
    };
    let initial = service.load_verified(&locator).unwrap();
    assert!(initial.events.iter().any(|event| matches!(event,
      AgentEvent::Message(message) if message.text == "compressed hello"
    )));
    assert!(Arc::ptr_eq(&initial, &service.load_verified(&locator).unwrap()));
    assert!(source_cursor(ViewerProvider::Codex, source_path).is_ok());
    assert_eq!(
      NativeRepository::default().session_body_indexing(&locator).unwrap(),
      SessionBodyIndexing::Ready
    );

    std::fs::write(&path, contents.replace("compressed hello", "plain replacement")).unwrap();
    let updated = service.load_verified(&locator).unwrap();
    assert!(!Arc::ptr_eq(&initial, &updated));
    assert!(updated.events.iter().any(|event| matches!(event,
      AgentEvent::Message(message) if message.text == "plain replacement"
    )));
    std::fs::remove_file(&path).unwrap();
  }
}

#[test]
fn local_codex_cache_tracks_inherited_history_and_rejects_missing_prefixes() {
  let directory = tempfile::tempdir().unwrap();
  let base_path = directory.path().join("rollout-base-linked.jsonl");
  let head_path = directory.path().join("rollout-head-linked.jsonl");
  let base = format!(
    "{}\n{}\n",
    json!({"ordinal": 0, "type": "session_meta", "payload": {
      "id": "linked", "history_mode": "paginated"
    }}),
    json!({"ordinal": 1, "type": "event_msg", "payload": {
      "type": "item_completed", "thread_id": "linked", "turn_id": "turn-1",
      "item": {"type": "UserMessage", "id": "message-1", "content": [{"type": "text", "text": "old hello"}]}
    }})
  );
  std::fs::write(&base_path, &base).unwrap();
  std::fs::write(
    &head_path,
    format!(
      "{}\n",
      json!({"ordinal": 2, "type": "session_meta", "payload": {
        "id": "linked", "history_mode": "paginated",
        "history_base": {"thread_id": "linked", "end_ordinal_exclusive": 2, "end_byte_offset": base.len()}
      }})
    ),
  )
  .unwrap();
  let service = ViewerService::new(Arc::new(NativeRepository::default()));
  let locator = SessionLocator {
    version: 1,
    provider: ViewerProvider::Codex,
    source_path: head_path,
    session_id: "linked".into(),
  };
  let initial = service.load_verified(&locator).unwrap();
  let discoveries = HISTORY_DEPENDENCY_DISCOVERIES.with(|count| count.get());
  for _ in 0..100 {
    assert!(Arc::ptr_eq(&initial, &service.load_verified(&locator).unwrap()));
  }
  assert_eq!(HISTORY_DEPENDENCY_DISCOVERIES.with(|count| count.get()), discoveries);
  assert!(initial.events.iter().any(|event| matches!(event,
    AgentEvent::Message(message) if message.text == "old hello"
  )));

  std::fs::write(&base_path, base.replace("old hello", "new hello")).unwrap();
  std::fs::File::open(&base_path)
    .unwrap()
    .set_modified(SystemTime::now() + std::time::Duration::from_secs(1))
    .unwrap();
  let updated = service.load_verified(&locator).unwrap();
  assert_eq!(
    HISTORY_DEPENDENCY_DISCOVERIES.with(|count| count.get()),
    discoveries + 1
  );
  assert!(!Arc::ptr_eq(&initial, &updated));
  assert!(updated.events.iter().any(|event| matches!(event,
    AgentEvent::Message(message) if message.text == "new hello"
  )));
  std::fs::remove_file(&base_path).unwrap();
  assert!(service.load_verified(&locator).is_err());
}
