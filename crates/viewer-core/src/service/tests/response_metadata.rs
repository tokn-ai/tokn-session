use super::*;

#[test]
fn only_visible_finished_final_replies_expose_recorded_response_metadata() {
  let raw = json!({
    "type":"message", "provider":"codex", "role":"assistant", "delivery":"final", "phase":"finished",
    "text":"Fixture reply",
    "response_metadata":{
      "model":{"model":{"model_provider":"fixture-provider","model_id":"fixture-model"},"origin":"request"},
      "fast_mode":"requested"
    }
  });
  let summary = |raw| {
    let event = serde_json::from_value::<AgentEvent>(raw).unwrap();
    serde_json::to_value(event_summary(std::slice::from_ref(&event), 0, &event)).unwrap()
  };
  assert_eq!(summary(raw.clone())["response_metadata"], raw["response_metadata"]);

  for (field, value) in [
    ("role", json!("user")),
    ("phase", json!("delta")),
    ("delivery", json!("commentary")),
    ("provenance", json!({"source":{},"display":false})),
    ("response_metadata", Value::Null),
  ] {
    let mut alternative = raw.clone();
    alternative[field] = value;
    assert!(summary(alternative).get("response_metadata").is_none(), "{field}");
  }
}
