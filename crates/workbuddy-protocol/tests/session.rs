use serde_json::{Value, json};
use tokn_workbuddy_protocol::{ContentBlock, WorkBuddySessionItem, WorkBuddySessionLine};

const FIXTURES: &[(&str, &str)] = &[
  (
    "wb-chat-basic",
    include_str!("fixtures/workbuddy/projects/fixture-workspace/wb-chat-basic.jsonl"),
  ),
  (
    "wb-file-read-local",
    include_str!("fixtures/workbuddy/projects/fixture-workspace/wb-file-read-local.jsonl"),
  ),
  (
    "wb-file-read",
    include_str!("fixtures/workbuddy/projects/fixture-workspace/wb-file-read.jsonl"),
  ),
  (
    "wb-multiturn",
    include_str!("fixtures/workbuddy/projects/fixture-workspace/wb-multiturn.jsonl"),
  ),
  (
    "wb-shell-command",
    include_str!("fixtures/workbuddy/projects/fixture-workspace/wb-shell-command.jsonl"),
  ),
];

#[test]
fn decodes_every_checked_in_fixture_losslessly() {
  let mut record_count = 0;
  let mut variant_counts = [0; 6];

  for (name, fixture) in FIXTURES {
    for (index, raw) in fixture.lines().filter(|line| !line.trim().is_empty()).enumerate() {
      let native: Value = serde_json::from_str(raw)
        .unwrap_or_else(|error| panic!("{name} line {} should be valid JSON: {error}", index + 1));
      let line: WorkBuddySessionLine =
        serde_json::from_str(raw).unwrap_or_else(|error| panic!("{name} line {} should decode: {error}", index + 1));

      assert_eq!(line.native(), &native, "{name} line {} native value", index + 1);
      assert_eq!(
        serde_json::to_value(&line).expect("record should serialize"),
        native,
        "{name} line {} round trip",
        index + 1
      );
      match line.item() {
        WorkBuddySessionItem::Message(_) => variant_counts[0] += 1,
        WorkBuddySessionItem::FunctionCall(_) => variant_counts[1] += 1,
        WorkBuddySessionItem::FunctionCallResult(_) => variant_counts[2] += 1,
        WorkBuddySessionItem::Reasoning(_) => variant_counts[3] += 1,
        WorkBuddySessionItem::FileHistorySnapshot(_) => variant_counts[4] += 1,
        WorkBuddySessionItem::AiTitle(_) => variant_counts[5] += 1,
        WorkBuddySessionItem::Unknown(item) => {
          panic!(
            "{name} line {} unexpectedly decoded as unknown: {:?}",
            index + 1,
            item.parse_error
          )
        }
      }
      record_count += 1;
    }
  }

  assert_eq!(record_count, 31);
  assert_eq!(variant_counts, [12, 3, 3, 3, 6, 4]);
}

#[test]
fn decodes_message_reasoning_and_tool_records() {
  let records = decode_fixture(FIXTURES[1].1);

  let WorkBuddySessionItem::Message(user) = records[0].item() else {
    panic!("expected user message");
  };
  assert_eq!(user.role.as_deref(), Some("user"));
  assert_eq!(
    user.content.first().and_then(ContentBlock::text),
    Some(
      "Use the Read tool to inspect inventory.txt. Calculate the total item count and identify the fruit with the largest count. Keep the final answer concise."
    )
  );
  assert_eq!(records[0].session_id(), Some("wb-file-read-local"));
  assert_eq!(records[0].cwd(), Some("/fixture/workspace"));

  let WorkBuddySessionItem::FunctionCall(call) = records[3].item() else {
    panic!("expected function call");
  };
  assert_eq!(call.name.as_deref(), Some("Read"));
  assert_eq!(call.call_id.as_deref(), Some("call_00_ET_QRXue3bsxdLblbRzA9Fp3295"));
  assert_eq!(
    call.arguments.as_ref().and_then(Value::as_str),
    Some("{\"file_path\": \"/fixture/workspace/inventory.txt\"}")
  );

  let WorkBuddySessionItem::FunctionCallResult(result) = records[4].item() else {
    panic!("expected function call result");
  };
  assert_eq!(result.status.as_deref(), Some("completed"));
  assert!(matches!(
    result.output.as_ref(),
    Some(ContentBlock::Text(content)) if content.text.as_deref().is_some_and(|text| text.contains("banana 5"))
  ));

  let WorkBuddySessionItem::Reasoning(reasoning) = records[5].item() else {
    panic!("expected reasoning");
  };
  assert!(reasoning.content.is_empty());
  assert!(matches!(
    reasoning.raw_content.first(),
    Some(ContentBlock::ReasoningText(content)) if content.text.as_deref().is_some_and(|text| text.contains("Total item count"))
  ));

  let WorkBuddySessionItem::Message(assistant) = records[6].item() else {
    panic!("expected assistant message");
  };
  assert_eq!(assistant.role.as_deref(), Some("assistant"));
  assert_eq!(assistant.status.as_deref(), Some("completed"));
  assert_eq!(
    assistant.content.first().and_then(ContentBlock::text),
    Some("Total count: **10**. Largest: **banana (5)**.")
  );
}

#[test]
fn decodes_snapshot_title_and_error_message_metadata() {
  let basic = decode_fixture(FIXTURES[0].1);

  let WorkBuddySessionItem::FileHistorySnapshot(snapshot) = basic[1].item() else {
    panic!("expected file history snapshot");
  };
  assert_eq!(snapshot.is_snapshot_update, Some(false));
  assert_eq!(
    snapshot
      .snapshot
      .as_ref()
      .and_then(|snapshot| snapshot.message_id.as_deref()),
    Some("8c8cdf7f-bc62-46f0-b026-e208864751b2")
  );

  let WorkBuddySessionItem::AiTitle(title) = basic[2].item() else {
    panic!("expected AI title");
  };
  assert_eq!(
    title.ai_title.as_deref(),
    Some("Explain provider-agnostic agent session layer")
  );

  let failed = decode_fixture(FIXTURES[2].1);
  let WorkBuddySessionItem::Message(message) = failed[2].item() else {
    panic!("expected failed assistant message");
  };
  assert_eq!(message.status.as_deref(), Some("incomplete"));
  assert_eq!(
    message
      .provider_data
      .as_ref()
      .and_then(|value| value["skipRun"].as_bool()),
    Some(true)
  );
  assert_eq!(
    message
      .provider_data
      .as_ref()
      .and_then(|value| value["error"]["isRetryable"].as_bool()),
    Some(false)
  );
}

#[test]
fn accepts_duplicate_record_ids_without_coalescing_parallel_calls() {
  let records = decode_fixture(FIXTURES[4].1);

  assert_eq!(records[3].id(), records[4].id());
  let WorkBuddySessionItem::FunctionCall(first) = records[3].item() else {
    panic!("expected first function call");
  };
  let WorkBuddySessionItem::FunctionCall(second) = records[4].item() else {
    panic!("expected second function call");
  };
  assert_ne!(first.call_id, second.call_id);
  assert_eq!(first.name.as_deref(), Some("Bash"));
  assert_eq!(second.name.as_deref(), Some("Bash"));
}

#[test]
fn preserves_unknown_records_and_content_variants() {
  let native = json!({
    "id": "future-1",
    "timestamp": 1788265737432_u64,
    "type": "future_record",
    "sessionId": "wb-future",
    "payload": {"answer": 42}
  });
  let line: WorkBuddySessionLine = serde_json::from_value(native.clone()).expect("future record should decode");
  let WorkBuddySessionItem::Unknown(item) = line.item() else {
    panic!("expected unknown record");
  };
  assert_eq!(item.native_type.as_deref(), Some("future_record"));
  assert_eq!(item.native["payload"]["answer"], json!(42));
  assert!(item.parse_error.is_none());
  assert_eq!(serde_json::to_value(line).expect("record should serialize"), native);

  let line: WorkBuddySessionLine = serde_json::from_value(json!({
    "type": "message",
    "role": "assistant",
    "content": [{"type": "future_content", "answer": 42}]
  }))
  .expect("message with future content should decode");
  let WorkBuddySessionItem::Message(message) = line.item() else {
    panic!("expected message");
  };
  let ContentBlock::Unknown(item) = &message.content[0] else {
    panic!("expected unknown content");
  };
  assert_eq!(item.native_type.as_deref(), Some("future_content"));
  assert_eq!(item.native["answer"], json!(42));
  assert!(item.parse_error.is_none());
}

#[test]
fn malformed_known_shapes_fall_back_without_losing_native_json() {
  let native = json!({
    "type": "message",
    "role": 42,
    "content": [],
    "future": true
  });
  let line: WorkBuddySessionLine = serde_json::from_value(native.clone()).expect("record should decode tolerantly");
  let WorkBuddySessionItem::Unknown(item) = line.item() else {
    panic!("expected tolerant fallback");
  };
  assert_eq!(item.native_type.as_deref(), Some("message"));
  assert!(item.parse_error.is_some());
  assert_eq!(item.native, native);
}

fn decode_fixture(fixture: &str) -> Vec<WorkBuddySessionLine> {
  fixture
    .lines()
    .filter(|line| !line.trim().is_empty())
    .map(|line| serde_json::from_str(line).expect("fixture record should decode"))
    .collect()
}
