use super::*;

fn tool_group(events: &[AgentEvent]) -> TrajectoryCardSummary {
  timeline_entries(events)
    .into_iter()
    .filter_map(|entry| match entry {
      TimelineEntry::Trajectory { trajectory } => Some(trajectory_card_summary(&trajectory, events)),
      _ => None,
    })
    .find(|group| group.tool_count > 0)
    .expect("fixture includes a group with tools")
}

#[test]
fn saved_dsh_and_workbuddy_calls_show_recorded_targets_in_closed_groups() {
  let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
  let dsh = tokn_session_dsh::DshSessionSource::new(None)
    .load_session_path(&fixtures.join("dsh/basic/session.jsonl"))
    .unwrap();
  let group = tool_group(&dsh.events);
  assert_eq!(group.status, "complete");
  assert_eq!(group.tool_count, 1);
  assert_eq!(
    serde_json::to_value(group.tool_previews).unwrap(),
    json!([{"kind":"file_read", "label":"guide.md", "status":"completed"}])
  );

  let workbuddy = tokn_session_workbuddy::WorkBuddySessionSource::new(None)
    .load_session_path(&fixtures.join("workbuddy/projects/fixture-workspace/wb-shell-command.jsonl"))
    .unwrap();
  let group = tool_group(&workbuddy.events);
  assert_eq!(group.status, "complete");
  assert_eq!(group.tool_count, 2);
  assert_eq!(
    serde_json::to_value(group.tool_previews).unwrap(),
    json!([
      {"kind":"shell", "label":"pwd", "status":"completed"},
      {"kind":"shell", "label":"uname -s", "status":"completed"},
    ])
  );
}

#[test]
fn codex_wrapper_uses_the_full_duration_of_one_logical_tool() {
  let records = [
    json!({"type":"session_meta", "payload":{"id":"fixture"}}),
    json!({"type":"response_item", "timestamp":"2026-08-31T12:32:55.026Z", "payload":{
      "type":"custom_tool_call", "name":"exec", "call_id":"call-write", "status":"completed",
      "input":"const r = await tools.write_stdin({session_id: 21, chars: \"\", yield_time_ms: 30000});\ntext(r);\n"
    }}),
    json!({"type":"response_item", "timestamp":"2026-08-31T12:33:25.037Z", "payload":{
      "type":"custom_tool_call_output", "call_id":"call-write",
      "output":[{"type":"input_text","text":"Script completed\nWall time 30.0 seconds\nOutput:\n"},
                {"type":"input_text","text":r#"{"session_id":21,"output":"Waiting"}"#}]
    }}),
  ];
  let mut normalizer = tokn_session_codex::normalize::CodexNormalizer::new();
  let events: Vec<_> = records
    .into_iter()
    .flat_map(|record| normalizer.normalize(serde_json::from_value(record).unwrap()))
    .collect();
  let group = tool_group(&events);
  assert_eq!(group.tool_count, 1);
  assert_eq!(group.tool_previews[0].kind, "terminal");
  assert!(group.tool_previews[0].label.starts_with("Wait for terminal"));
  assert_eq!(group.started_at.as_deref(), Some("2026-08-31T12:32:55.026Z"));
  assert_eq!(group.ended_at.as_deref(), Some("2026-08-31T12:33:25.037Z"));
  assert_eq!(group.duration_ms.as_deref(), Some("30011"));
}

#[test]
fn previews_bound_text_and_count_and_time_overlapping_calls_in_source_order() {
  let shell = |id: &str, command: Option<String>, phase: Phase, exit_code: Option<i64>, time| {
    with_timestamp(
      tool_call(
        Provider::Codex,
        "exec_command",
        id,
        ToolKind::Shell,
        Some(ToolSummary::Shell {
          command,
          cwd: None,
          exit_code,
        }),
        phase,
        exit_code.map(|_| json!({"output":"private log contents"})),
      ),
      time,
    )
  };
  let events = vec![
    shell(
      "slow",
      Some("test".into()),
      Phase::Started,
      None,
      "2026-01-01T12:00:00Z",
    ),
    shell(
      "quick",
      Some("check".into()),
      Phase::Started,
      None,
      "2026-01-01T12:00:01Z",
    ),
    shell("quick", None, Phase::Finished, Some(0), "2026-01-01T12:00:02Z"),
    shell(
      "third",
      Some("build\n   project".into()),
      Phase::Finished,
      Some(1),
      "2026-01-01T12:00:03Z",
    ),
    shell(
      "fourth",
      Some("界".repeat(120)),
      Phase::Started,
      None,
      "2026-01-01T12:00:04Z",
    ),
    shell("slow", None, Phase::Finished, Some(0), "2026-01-01T12:00:10Z"),
  ];
  let group = tool_group(&events);
  assert_eq!(group.duration_ms.as_deref(), Some("10000"));
  assert_eq!(group.tool_count, 4);
  assert_eq!(group.tool_previews.len(), 3);
  assert_eq!(group.tool_previews[0].label, "build project");
  assert_eq!(group.tool_previews[0].status, "failed");
  assert_eq!(group.tool_previews[1].label, format!("{}…", "界".repeat(95)));
  assert_eq!(group.tool_previews[1].status, "pending");
  assert_eq!(group.tool_previews[2].label, "test");
}
