use crate::normalize::CodexNormalizer;
use serde_json::{Value, json};
use tokn_session_core::AgentEvent;

fn record(kind: &str, payload: Value) -> Value {
  json!({"type":kind,"timestamp":"2026-01-01T12:00:00Z","payload":payload})
}

fn goal(status: &str) -> Value {
  json!({"threadId":"thread","objective":"Check the parser","createdAt":20,"status":status,"tokensUsed":42})
}

fn start() -> Value {
  record("event_msg", json!({"type":"task_started","turn_id":"work-turn"}))
}

fn native_goal(goal: Value) -> Value {
  record(
    "event_msg",
    json!({"type":"thread_goal_updated","threadId":"thread","goal":goal}),
  )
}

fn call() -> Value {
  record(
    "response_item",
    json!({
      "type":"custom_tool_call","status":"completed","name":"exec","input":"text(await tools.update_goal({status:\"complete\"}))\n",
      "call_id":"action","internal_chat_message_metadata_passthrough":{"turn_id":"work-turn"}
    }),
  )
}

fn output(goal: Value) -> Value {
  record(
    "response_item",
    json!({
      "type":"custom_tool_call_output","call_id":"action","output":[
        {"type":"input_text","text":"Script completed\nWall time 0.1 seconds\nOutput:\n"},
        {"type":"input_text","text":json!({"goal":goal,"remainingTokens":null}).to_string()}],
      "internal_chat_message_metadata_passthrough":{"turn_id":"work-turn"}
    }),
  )
}

fn events(records: impl IntoIterator<Item = Value>, paginated: bool) -> Vec<AgentEvent> {
  let mut normalizer = CodexNormalizer::new_historical();
  let mut header = json!({"id":"thread"});
  if paginated {
    header["history_mode"] = "paginated".into();
  }
  std::iter::once(record("session_meta", header))
    .chain(records)
    .flat_map(|value| normalizer.normalize(serde_json::from_value(value).unwrap()))
    .collect()
}

fn confirmed(records: Vec<Value>) -> usize {
  events([native_goal(goal("active")),start()].into_iter().chain(records), true).iter()
    .filter(|event| matches!(event,AgentEvent::GoalUpdated(change) if change.goal.as_ref().is_some_and(|g|g["status"]=="complete"))).count()
}

#[test]
fn user_goal_preserves_only_the_original_objective_in_legacy_and_paginated_histories() {
  let objective = "Check \"two\" cases:\n  preserve *this* spacing.";
  let marker = record(
    "response_item",
    json!({
      "type":"message","role":"user","id":"request","content":[{"type":"input_text","text":format!(
        "<codex_internal_context source=\"user_goal\">\nUser set the goal: \"{objective}\"\nUser set goal status: \"active\".\n</codex_internal_context>")}],
      "internal_chat_message_metadata_passthrough":{"turn_id":"goal-input","content_item_kinds":["user.goal"],
        "content_item_metadata":[{"provenance":{"type":"harness","hook":false}}]}
    }),
  );
  let mut continuation = marker.clone();
  continuation["payload"]["internal_chat_message_metadata_passthrough"]["content_item_kinds"][0] =
    "goal.internal_context".into();
  let mut quoted = marker.clone();
  quoted["payload"]["internal_chat_message_metadata_passthrough"] = Value::Null;
  let mut hook = marker.clone();
  hook["payload"]["internal_chat_message_metadata_passthrough"]["content_item_metadata"][0]["provenance"]["hook"] =
    true.into();
  for paginated in [false, true] {
    let result = events(
      [marker.clone(), continuation.clone(), quoted.clone(), hook.clone()],
      paginated,
    );
    let messages: Vec<_> = result
      .iter()
      .filter_map(|e| if let AgentEvent::Message(m) = e { Some(m) } else { None })
      .collect();
    assert_eq!(messages.len(), 1);
    assert!(messages[0].is_user_goal());
    assert_eq!(messages[0].text, objective);
    assert_eq!(messages[0].message_id.as_deref(), Some("request"));
  }
}

#[test]
fn correlated_completion_carries_the_real_receipt_and_execution_turn() {
  let receipt = output(goal("complete"));
  for paginated in [true, false] {
    let result = events(
      [
        native_goal(goal("active")),
        start(),
        call(),
        // The same lifecycle may be announced twice while the tool is in flight.
        start(),
        native_goal(goal("active")),
        receipt.clone(),
        receipt.clone(),
      ],
      paginated,
    );
    let completed: Vec<_> = result
      .iter()
      .filter_map(|e| match e {
        AgentEvent::GoalUpdated(change) if change.goal.as_ref().is_some_and(|g| g["status"] == "complete") => {
          Some(change)
        }
        _ => None,
      })
      .collect();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].turn_id.as_deref(), Some("work-turn"));
    assert_eq!(completed[0].session_id.as_deref(), Some("thread"));
    assert_eq!(completed[0].goal.as_ref(), Some(&goal("complete")));
  }
}

#[test]
fn rejects_unrelated_tool_results_wrong_origins_and_stale_calls() {
  let good = output(goal("complete"));
  for (path, replacement) in [
    ("/payload/input", json!("text(await tools.get_goal({}));")),
    (
      "/payload/input",
      json!("text(await tools.update_goal({status:\"complete\"})); text({goal:{status:\"complete\"}})"),
    ),
    (
      "/payload/input",
      json!("text(await tools.update_goal({status:computed}))"),
    ),
    ("/payload/name", json!("other")),
    (
      "/payload/internal_chat_message_metadata_passthrough/turn_id",
      json!("old-turn"),
    ),
  ] {
    let mut fake = call();
    *fake.pointer_mut(path).unwrap() = replacement;
    assert_eq!(confirmed(vec![fake, good.clone()]), 0, "{path}");
  }
  for (path, replacement) in [
    ("/payload/call_id", json!("different")),
    ("/payload/output/0/text", json!("Script failed\nOutput:\n")),
    (
      "/payload/internal_chat_message_metadata_passthrough/turn_id",
      json!("old-turn"),
    ),
  ] {
    let mut fake = good.clone();
    *fake.pointer_mut(path).unwrap() = replacement;
    assert_eq!(confirmed(vec![call(), fake]), 0, "{path}");
  }
  for (key, replacement) in [
    ("threadId", json!("other")),
    ("objective", json!("Other objective")),
    ("createdAt", json!(21)),
    ("status", json!("active")),
  ] {
    let mut wrong = goal("complete");
    wrong[key] = replacement;
    assert_eq!(confirmed(vec![call(), output(wrong)]), 0, "{key}");
  }
  let mut shell = call();
  shell["payload"]["input"] = "text(await tools.exec_command({cmd:\"echo completion\"}))".into();
  for records in [
    vec![good.clone()],
    vec![shell.clone(), call(), good.clone()],
    vec![call(), shell, good.clone()],
    vec![call(), native_goal(Value::Null), good.clone()],
    vec![
      call(),
      record("event_msg", json!({"type":"task_started","turn_id":"later-turn"})),
      good.clone(),
    ],
    // A repeated start must also preserve duplicate-call tombstones.
    vec![call(), call(), start(), call(), good.clone()],
    vec![
      call(),
      record("event_msg", json!({"type":"turn_aborted","turn_id":"work-turn"})),
      good,
    ],
  ] {
    assert_eq!(confirmed(records), 0);
  }
}

#[test]
fn direct_builtin_requires_its_own_function_output() {
  let mut direct = call();
  let payload = &mut direct["payload"];
  payload["type"] = "function_call".into();
  payload["name"] = "update_goal".into();
  payload["arguments"] = json!({"status":"complete"}).to_string().into();
  let mut result = output(goal("complete"));
  result["payload"]["type"] = "function_call_output".into();
  result["payload"]["output"] = json!({"goal":goal("complete")}).to_string().into();
  assert_eq!(confirmed(vec![direct.clone(), result.clone()]), 1);
  assert_eq!(confirmed(vec![call(), result.clone()]), 0);
  direct["payload"]["namespace"] = "external".into();
  assert_eq!(confirmed(vec![direct, result]), 0);
}
