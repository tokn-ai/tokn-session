use serde_json::{Value, json};
use tokn_codex_protocol::{ResponseItem, RolloutItem, RolloutLine};

#[test]
fn history_base_is_typed_and_preserves_extensions() {
  let native = json!({"type":"session_meta","ordinal":42,"payload":{
    "id":"thread","history_mode":"paginated","history_base":{
      "thread_id":"thread","end_ordinal_exclusive":42,"end_byte_offset":1234,"future":true
    }
  }});
  let line: RolloutLine = serde_json::from_value(native.clone()).unwrap();
  let RolloutItem::SessionMeta(meta) = line.item() else {
    panic!("expected session metadata")
  };
  let base = meta.history_base().unwrap().unwrap();
  assert_eq!(base.thread_id, "thread");
  assert_eq!(base.end_ordinal_exclusive, 42);
  assert_eq!(base.end_byte_offset, 1234);
  assert_eq!(base.extra["future"], true);
  assert_eq!(meta.extra["history_base"], native["payload"]["history_base"]);
  assert_eq!(
    serde_json::to_value(meta).unwrap()["history_base"],
    native["payload"]["history_base"]
  );
  assert_eq!(serde_json::to_value(line).unwrap(), native);
}

fn usage_counters() -> Value {
  json!({
    "input_tokens": 30,
    "cached_input_tokens": 20,
    "cache_write_input_tokens": 0,
    "output_tokens": 5,
    "reasoning_output_tokens": 2,
    "total_tokens": 35
  })
}

#[test]
fn decodes_token_usage_records_without_losing_extensions() {
  let mut usage = usage_counters();
  usage["future_counter"] = json!({"tokens": 3});
  let native = json!({
    "timestamp": "2026-09-13T00:00:00Z",
    "ordinal": 8,
    "type": "token_usage_record",
    "future_envelope": true,
    "payload": {
      "thread_id": "thread-1",
      "turn_id": "turn-1",
      "session_id": "process-1",
      "root_turn_id": "root-turn-1",
      "response_id": "resp-1",
      "usage": usage,
      "turn_token_usage": usage_counters(),
      "thread_token_usage": usage_counters(),
      "future_field": [1, 2]
    }
  });
  let line: RolloutLine = serde_json::from_value(native.clone()).expect("usage record should decode");
  assert_eq!(line.item().native_type(), Some("token_usage_record"));
  let RolloutItem::Unknown(unknown) = line.item() else {
    panic!("new accounting retains the published enum's unknown representation");
  };
  assert!(unknown.parse_error.is_none());
  let item = line.token_usage_record().expect("typed token usage record");
  assert_eq!(item.thread_id.as_deref(), Some("thread-1"));
  assert_eq!(item.turn_id.as_deref(), Some("turn-1"));
  assert_eq!(item.session_id.as_deref(), Some("process-1"));
  assert_eq!(item.root_turn_id.as_deref(), Some("root-turn-1"));
  assert_eq!(item.response_id.as_deref(), Some("resp-1"));
  let usage = item.usage.as_ref().expect("usage counters");
  assert_eq!(usage.input_tokens, 30);
  assert_eq!(usage.cached_input_tokens, 20);
  assert_eq!(usage.cache_write_input_tokens, Some(0));
  assert_eq!(usage.output_tokens, 5);
  assert_eq!(usage.reasoning_output_tokens, 2);
  assert_eq!(usage.total_tokens, 35);
  assert_eq!(usage.extra["future_counter"], json!({"tokens": 3}));
  assert_eq!(item.turn_token_usage.as_ref().unwrap().total_tokens, 35);
  assert_eq!(item.thread_token_usage.as_ref().unwrap().total_tokens, 35);
  assert_eq!(item.extra["future_field"], json!([1, 2]));
  assert_eq!(serde_json::to_value(line).unwrap(), native);
}

#[test]
fn accepts_missing_optional_token_usage_fields() {
  for payload in [
    json!({}),
    json!({"usage": null}),
    json!({"usage": {
      "input_tokens": 30,
      "cached_input_tokens": 20,
      "output_tokens": 5,
      "reasoning_output_tokens": 2,
      "total_tokens": 35
    }}),
  ] {
    let native = json!({"type": "token_usage_record", "payload": payload});
    let line: RolloutLine = serde_json::from_value(native.clone()).unwrap();
    let item = line
      .token_usage_record()
      .expect("missing optional fields must remain decodable");
    assert!(item.response_id.is_none());
    assert!(item.turn_token_usage.is_none());
    assert!(item.thread_token_usage.is_none());
    if let Some(usage) = &item.usage {
      assert_eq!(usage.cache_write_input_tokens, None);
    }
    assert_eq!(serde_json::to_value(line).unwrap(), native);
  }
}

#[test]
fn malformed_token_usage_counters_remain_unknown_and_lossless() {
  let required_counters = [
    "input_tokens",
    "cached_input_tokens",
    "output_tokens",
    "reasoning_output_tokens",
    "total_tokens",
  ];
  for counter in required_counters {
    let mut counters = usage_counters();
    counters.as_object_mut().unwrap().remove(counter);
    assert_unknown_usage(counters);
  }
  for counter in required_counters.into_iter().chain(["cache_write_input_tokens"]) {
    for invalid in [json!(-1), json!(1.5), json!("5"), json!(true), json!({})] {
      let mut counters = usage_counters();
      counters[counter] = invalid;
      assert_unknown_usage(counters);
    }
  }
}

fn assert_unknown_usage(counters: Value) {
  // Aggregates are optional, but when present they must also contain counters.
  for field in ["usage", "turn_token_usage", "thread_token_usage"] {
    let mut payload = json!({"response_id": "resp-1", "usage": usage_counters()});
    payload[field] = counters.clone();
    let native = json!({"type": "token_usage_record", "payload": payload});
    let line: RolloutLine = serde_json::from_value(native.clone()).unwrap();
    let RolloutItem::Unknown(item) = line.item() else {
      panic!("malformed {field} must remain unknown");
    };
    assert_eq!(item.native_type.as_deref(), Some("token_usage_record"));
    assert!(item.parse_error.is_some());
    assert!(line.token_usage_record().is_none());
    assert_eq!(serde_json::to_value(line).unwrap(), native);
  }
}

#[test]
fn decodes_current_rollout_control_records() {
  let world_state: RolloutLine = serde_json::from_value(json!({
    "timestamp": "2026-07-28T00:00:00Z",
    "ordinal": 7,
    "type": "world_state",
    "payload": {
      "full": false,
      "state": {
        "environments": {
          "subagents": ["reviewer"]
        }
      },
      "future_field": true
    }
  }))
  .expect("world state should decode");

  assert_eq!(world_state.timestamp(), Some("2026-07-28T00:00:00Z"));
  assert_eq!(world_state.ordinal(), Some(7));
  let RolloutItem::WorldState(item) = world_state.item() else {
    panic!("expected world state");
  };
  assert_eq!(item.full, Some(false));
  assert_eq!(item.extra.get("future_field"), Some(&Value::Bool(true)));

  let metadata: RolloutLine = serde_json::from_value(json!({
    "type": "inter_agent_communication_metadata",
    "payload": {
      "trigger_turn": true
    }
  }))
  .expect("communication metadata should decode");
  let RolloutItem::InterAgentCommunicationMetadata(item) = metadata.item() else {
    panic!("expected communication metadata");
  };
  assert_eq!(item.trigger_turn, Some(true));
}

#[test]
fn accepts_new_turn_context_values_without_schema_failure() {
  let line: RolloutLine = serde_json::from_value(json!({
    "type": "turn_context",
    "payload": {
      "turn_id": "turn-1",
      "cwd": "/tmp/project",
      "workspace_roots": ["/tmp/project"],
      "approval_policy": "on-request",
      "approvals_reviewer": "auto_review",
      "sandbox_policy": {"type": "workspace-write"},
      "model": "gpt-5.6-sol",
      "effort": "ultra",
      "collaboration_mode": {
        "mode": "default",
        "settings": {
          "future_setting": true
        }
      }
    }
  }))
  .expect("turn context should decode");

  let RolloutItem::TurnContext(item) = line.item() else {
    panic!("expected turn context");
  };
  assert_eq!(item.effort.as_deref(), Some("ultra"));
  assert_eq!(item.approvals_reviewer.as_deref(), Some("auto_review"));
  assert_eq!(
    item
      .workspace_roots
      .as_deref()
      .and_then(|roots| roots.first())
      .map(String::as_str),
    Some("/tmp/project")
  );
}

#[test]
fn preserves_canonical_item_lifecycle_payloads_losslessly() {
  let native = json!({
    "timestamp": "2026-08-28T07:49:14Z",
    "type": "event_msg",
    "payload": {
      "type": "item_completed",
      "thread_id": "thread-1",
      "turn_id": "turn-1",
      "item": {
        "type": "SubAgentActivity",
        "id": "subagent-1",
        "kind": "completed",
        "agent_thread_id": "child-1",
        "agent_path": "/root/reviewer",
        "future_field": true
      },
      "started_at_ms": 1000,
      "completed_at_ms": 1001
    }
  });
  let line: RolloutLine = serde_json::from_value(native.clone()).expect("item lifecycle should decode");
  let RolloutItem::EventMessage(event) = line.item() else {
    panic!("expected event message");
  };
  assert_eq!(event.event_type.as_deref(), Some("item_completed"));
  assert_eq!(event.native["item"]["future_field"], json!(true));
  assert_eq!(serde_json::to_value(line).expect("line should serialize"), native);
}

#[test]
fn session_history_mode_remains_tolerant_of_future_values() {
  for history_mode in ["legacy", "paginated", "future_mode"] {
    let line: RolloutLine = serde_json::from_value(json!({
      "type": "session_meta",
      "payload": {
        "id": "thread-1",
        "history_mode": history_mode
      }
    }))
    .expect("history mode should decode");
    let RolloutItem::SessionMeta(item) = line.item() else {
      panic!("expected session metadata");
    };
    assert_eq!(item.history_mode().unwrap(), Some(history_mode));
    assert_eq!(item.extra["history_mode"], history_mode);
  }
}

#[test]
fn accepts_null_optional_reasoning_content() {
  let line: RolloutLine = serde_json::from_value(json!({
    "type": "response_item",
    "payload": {
      "type": "reasoning",
      "summary": [],
      "content": null,
      "encrypted_content": "ciphertext"
    }
  }))
  .expect("reasoning should decode");

  let RolloutItem::ResponseItem(ResponseItem::Reasoning(item)) = line.item() else {
    panic!("expected reasoning item");
  };
  assert!(item.content.is_none());
  assert_eq!(item.encrypted_content.as_deref(), Some("ciphertext"));
}

#[test]
fn decodes_agent_messages_without_erasing_identity() {
  let line: RolloutLine = serde_json::from_value(json!({
    "type": "response_item",
    "payload": {
      "type": "agent_message",
      "id": "amsg_1",
      "author": "/root",
      "recipient": "/root/reviewer",
      "content": [
        {
          "type": "input_text",
          "text": "Please review this."
        },
        {
          "type": "encrypted_content",
          "encrypted_content": "ciphertext"
        }
      ]
    }
  }))
  .expect("agent message should decode");

  let RolloutItem::ResponseItem(ResponseItem::AgentMessage(item)) = line.item() else {
    panic!("expected agent message");
  };
  assert_eq!(item.id.as_deref(), Some("amsg_1"));
  assert_eq!(item.author.as_deref(), Some("/root"));
  assert_eq!(item.recipient.as_deref(), Some("/root/reviewer"));
  assert_eq!(item.content[0].text.as_deref(), Some("Please review this."));
  assert_eq!(item.content[1].encrypted_content.as_deref(), Some("ciphertext"));
}

#[test]
fn accepts_string_and_structured_custom_tool_outputs() {
  let string_output = decode_response(json!({
    "type": "custom_tool_call_output",
    "call_id": "call-old",
    "output": "done"
  }));
  let ResponseItem::CustomToolCallOutput(item) = string_output else {
    panic!("expected custom tool output");
  };
  assert_eq!(item.output, json!("done"));

  let structured_output = decode_response(json!({
    "type": "custom_tool_call_output",
    "call_id": "call-new",
    "name": "exec",
    "output": [
      {
        "type": "input_text",
        "text": "Script completed"
      }
    ]
  }));
  let ResponseItem::CustomToolCallOutput(item) = structured_output else {
    panic!("expected structured custom tool output");
  };
  assert_eq!(item.name.as_deref(), Some("exec"));
  assert!(item.output.is_array());
}

#[test]
fn preserves_unknown_rollout_and_response_types() {
  let response_payload = json!({
    "type": "future_response",
    "id": "future-1",
    "data": {
      "answer": 42
    }
  });
  let response = decode_response(response_payload.clone());
  let ResponseItem::Unknown(item) = response else {
    panic!("expected unknown response");
  };
  assert_eq!(item.native_type.as_deref(), Some("future_response"));
  assert_eq!(item.payload, response_payload);
  assert!(item.parse_error.is_none());

  let rollout_payload = json!({
    "enabled": true
  });
  let line: RolloutLine = serde_json::from_value(json!({
    "type": "future_rollout",
    "payload": rollout_payload
  }))
  .expect("future rollout should decode");
  let RolloutItem::Unknown(item) = line.item() else {
    panic!("expected unknown rollout");
  };
  assert_eq!(item.native_type.as_deref(), Some("future_rollout"));
  assert_eq!(item.payload, rollout_payload);
}

#[test]
fn serializing_a_line_returns_the_unchanged_native_record() {
  let native = json!({
    "timestamp": "2026-07-28T00:00:00Z",
    "type": "future_rollout",
    "payload": {
      "nested": [1, 2, 3]
    },
    "top_level_extension": "preserved"
  });
  let line: RolloutLine = serde_json::from_value(native.clone()).expect("line should decode");
  assert_eq!(serde_json::to_value(line).expect("line should serialize"), native);
}

fn decode_response(payload: Value) -> ResponseItem {
  let line: RolloutLine = serde_json::from_value(json!({
    "type": "response_item",
    "payload": payload
  }))
  .expect("response item should decode");
  let RolloutItem::ResponseItem(item) = line.into_item() else {
    panic!("expected response item");
  };
  item
}
