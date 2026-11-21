use super::*;
use serde_json::json;

fn event(kind: RuntimeEventKind) -> RuntimeEvent {
  RuntimeEvent {
    runtime_id: "runtime".into(),
    instance_id: "instance".into(),
    session_id: Some("session".into()),
    sequence: 1,
    kind,
  }
}
fn state(status: ExecutionState) -> RuntimeEvent {
  event(RuntimeEventKind::State(tokn_interaction_core::SessionState {
    session_id: Some("session".into()),
    status,
    model: None,
    pending_inputs: None,
    revision: None,
  }))
}
fn chunk(prompt: &str, update: serde_json::Value) -> RuntimeEvent {
  event(RuntimeEventKind::Native(
    json!({"protocol":"acp", "prompt_id":prompt, "update":update}),
  ))
}
fn text(prompt: &str, value: &str) -> RuntimeEvent {
  chunk(
    prompt,
    json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text", "text":value}}),
  )
}
fn target(instance: &str) -> SessionTarget {
  SessionTarget {
    runtime_id: "runtime".into(),
    instance_id: instance.into(),
    session_id: Some("session".into()),
  }
}

#[test]
fn transient_output_is_prompt_scoped_and_terminal_state_never_synthesizes_saved_rows() {
  let mut projection = LiveOutputProjection::default();
  assert!(!projection.apply(&text("one", "not active")));
  projection.apply(&state(ExecutionState::Running));
  projection.apply(&text("one", "First"));
  projection.apply(&text("one", " response"));
  projection.apply(&chunk("one", json!({"sessionUpdate":"tool_call", "toolCallId":"tool-one", "title":"Read file", "status":"in_progress", "rawInput":{"secret":"private-input"}})));
  projection.apply(&chunk("one", json!({"sessionUpdate":"tool_call_update", "toolCallId":"tool-one", "status":"completed", "rawOutput":"private-output"})));
  let output = projection.get(&target("instance")).unwrap();
  assert_eq!(output.parts[0].text, "First response");
  assert_eq!(output.parts[1].status.as_deref(), Some("completed"));
  assert!(!serde_json::to_string(&output).unwrap().contains("private-"));
  assert!(projection.get(&target("replacement")).is_none());
  projection.apply(&text("two", "New turn"));
  assert_eq!(projection.get(&target("instance")).unwrap().parts[0].text, "New turn");
  projection.apply(&state(ExecutionState::Idle));
  assert!(projection.get(&target("instance")).is_none());
  assert!(!projection.apply(&text("two", "late chunk")));
}

#[test]
fn transient_projection_bounds_text_parts_sessions_and_clears_on_loss() {
  let mut projection = LiveOutputProjection::default();
  projection.apply(&state(ExecutionState::Running));
  projection.apply(&text("one", &"界".repeat(MAX_BYTES)));
  let output = projection.get(&target("instance")).unwrap();
  assert!(output.truncated);
  assert!(output.parts[0].text.len() <= MAX_BYTES);
  for index in 0..MAX_PARTS + 10 {
    projection.apply(&chunk(
      "one",
      json!({"sessionUpdate":"tool_call", "toolCallId":format!("tool-{index}"), "title":"Tool"}),
    ));
  }
  assert!(projection.get(&target("instance")).unwrap().parts.len() <= MAX_PARTS);
  for index in 0..MAX_SESSIONS + 10 {
    let mut state = state(ExecutionState::Running);
    state.session_id = Some(format!("session-{index}"));
    projection.apply(&state);
  }
  assert!(projection.entries.len() <= MAX_SESSIONS);
  projection.apply(&event(RuntimeEventKind::Disconnected {
    reason: "closed".into(),
  }));
  assert!(projection.entries.is_empty());
  projection.apply(&state(ExecutionState::Running));
  projection.apply(&text("one", "Visible"));
  projection.clear();
  assert!(projection.get(&target("instance")).is_none());
}
