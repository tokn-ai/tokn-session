use rusqlite::Connection;
use serde_json::{Value, json};
use std::sync::Arc;
use tokn_session_core::{AgentEvent, CompactionState, MessageImageSource, Phase, Role};
use tokn_session_opencode::{
  CompactRecord, OpenCodeCompactCache, OpenCodeHistoryFormat, OpenCodeSessionCache, OpenCodeSessionSource,
  media_reference_hash,
};

const HASH: &str = "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb";
fn fixture() -> (tempfile::TempDir, Connection, OpenCodeSessionSource) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("opencode.db");
  let connection = Connection::open(&path).unwrap();
  connection
    .execute_batch(include_str!("fixtures/v2-current.sql"))
    .unwrap();
  (directory, connection, OpenCodeSessionSource::new(Some(path)))
}
fn assistant_text(events: &[AgentEvent]) -> Vec<&str> {
  events
    .iter()
    .filter_map(|event| match event {
      AgentEvent::Message(message) if message.role == Role::Assistant => Some(message.text.as_str()),
      _ => None,
    })
    .collect()
}

#[test]
fn full_native_id_takes_precedence_over_longer_prefix_matches() {
  let (_directory, _connection, source) = fixture();
  assert_eq!(source.load_session("ses_current").unwrap().reference.id, "ses_current");
  assert_eq!(
    source.load_session("ses_current_e").unwrap().reference.id,
    "ses_current_empty"
  );
  assert!(source.load_session("ses_curr").unwrap_err().contains("multiple"));
}

#[test]
fn current_metadata_and_ordered_parts_keep_native_shapes_and_mixed_inline_tail() {
  let (_directory, connection, source) = fixture();
  assert_eq!(
    source.history_format_exact("ses_current_empty").unwrap(),
    OpenCodeHistoryFormat::V2
  );
  assert_eq!(source.list_sessions().unwrap().len(), 2);
  let loaded = source.load_session_records_exact("ses_current", true).unwrap();
  let answer = loaded
    .records
    .iter()
    .find(|record| record.record_id == "v2:msg_assistant")
    .unwrap();
  assert_eq!(assistant_text(&answer.events), ["First", " inline tail"]);
  let native = answer.native.as_ref().unwrap();
  assert!(native["content"][0].get("id").is_none());
  assert_eq!(native["content"].as_array().unwrap().len(), 6);
  assert_eq!(native["content"][4]["opaque"]["preserved"], true);
  assert!(answer.events.iter().any(|event| matches!(event,AgentEvent::ToolCall(tool) if tool.tool_call_id.as_deref()==Some("call_stream") && tool.phase==Phase::Started && tool.input.as_ref().is_some_and(Value::is_string) && tool.output.is_none())));
  assert!(answer.events.iter().any(|event| matches!(event,AgentEvent::ToolCall(tool) if tool.tool_call_id.as_deref()==Some("call_done") && tool.output.as_ref().unwrap()["metadata"]["fixture"]==true && matches!(&tool.images[0].source,MessageImageSource::Unavailable { reference:Some(reference) } if media_reference_hash(reference)==Some(HASH)))));
  let events: Vec<_> = loaded.records.into_iter().flat_map(|record| record.events).collect();
  assert!(events.iter().any(|event| matches!(event,AgentEvent::ToolCall(tool) if tool.tool_call_id.as_deref()==Some("sh_fixture") && tool.phase==Phase::Finished && tool.output.as_ref().unwrap()["output"]=="shell")));
  for state in [
    CompactionState::Started,
    CompactionState::Completed,
    CompactionState::Failed,
  ] {
    assert!(
      events
        .iter()
        .any(|event| matches!(event,AgentEvent::Compaction(event) if event.state==state))
    );
  }
  for kind in ["skill", "location-switched", "idle"] {
    assert!(
      events
        .iter()
        .any(|event| matches!(event,AgentEvent::Metadata(event) if event.native_type==format!("message.v2.{kind}")))
    );
  }
  connection.execute("insert into session_part values ('msg_assistant','ses_current_empty',99,'text',null,123,123,'{\"type\":\"text\",\"text\":\"foreign part\"}')",[]).unwrap();
  assert!(
    !source
      .load_session_exact("ses_current")
      .unwrap()
      .events
      .iter()
      .any(|event| matches!(event,AgentEvent::Message(message) if message.text=="foreign part"))
  );
}

#[test]
fn incremental_caches_detect_part_edits_and_type_changes_without_message_timestamp_changes() {
  let (_directory, connection, source) = fixture();
  let mut cache = OpenCodeSessionCache::default();
  let first = source
    .load_session_records_cached_exact("ses_current", true, &mut cache)
    .unwrap();
  connection.execute("update session_part set data='{\"type\":\"text\",\"text\":\"Edited\"}' where message_id='msg_assistant' and ordinal=1",[]).unwrap();
  let second = source
    .load_session_records_cached_exact("ses_current", true, &mut cache)
    .unwrap();
  let position = first
    .records
    .iter()
    .position(|record| record.record_id == "v2:msg_assistant")
    .unwrap();
  assert!(!Arc::ptr_eq(&first.records[position], &second.records[position]));
  assert!(Arc::ptr_eq(&first.records[1], &second.records[1]));
  assert_eq!(
    assistant_text(&second.records[position].events),
    ["Edited", " inline tail"]
  );
  let mut compact = OpenCodeCompactCache::default();
  source
    .load_session_records_compact_exact("ses_current", true, &mut compact)
    .unwrap();
  connection
    .execute(
      "update session_part set type='reasoning' where message_id='msg_assistant' and ordinal=1",
      [],
    )
    .unwrap();
  let changed = source
    .load_session_records_compact_exact("ses_current", true, &mut compact)
    .unwrap();
  let CompactRecord::Changed(answer) = &changed.records[position] else {
    panic!("part type is part of the snapshot fingerprint")
  };
  assert!(
    answer
      .events
      .iter()
      .any(|event| matches!(event,AgentEvent::Reasoning(reasoning) if reasoning.text.as_deref()==Some("Edited")))
  );
  assert_eq!(assistant_text(&answer.events), [" inline tail"]);
}

#[test]
fn legacy_v1_and_current_metadata_coexist_but_duplicate_native_ids_are_ambiguous() {
  let (_directory, connection, source) = fixture();
  connection.execute_batch("create table session(id text primary key,parent_id text,directory text,time_created integer,time_updated integer); create table message(id text primary key,session_id text,time_created integer,data text); create table part(id text primary key,message_id text,session_id text,time_created integer,data text); insert into session values('ses_legacy',null,'/work/legacy',1,2); insert into message values('old_user','ses_legacy',1,'{\"role\":\"user\"}'); insert into part values('old_part','old_user','ses_legacy',1,'{\"type\":\"text\",\"text\":\"legacy prompt\"}');").unwrap();
  assert_eq!(source.list_session_headers().unwrap().len(), 3);
  assert_eq!(
    source.history_format_exact("ses_legacy").unwrap(),
    OpenCodeHistoryFormat::V1
  );
  assert!(
    source
      .load_session_exact("ses_legacy")
      .unwrap()
      .events
      .iter()
      .any(|event| matches!(event,AgentEvent::Message(message) if message.text=="legacy prompt"))
  );
  connection
    .execute(
      "insert into session values('ses_current',null,'/work/ambiguous',1,2)",
      [],
    )
    .unwrap();
  assert!(source.session_header_exact("ses_current").is_err());
  assert!(source.history_format_exact("ses_current").is_err());
  assert!(source.list_session_headers().is_err());
  let observed = source.list_session_header_catalog().unwrap();
  assert!(!observed.complete);
  assert!(!observed.headers.iter().any(|header| header.id == "ses_current"));
}

#[test]
fn blob_reads_require_exact_owner_sequence_size_and_content_hash() {
  let (_directory, connection, source) = fixture();
  let reference = format!("media:{HASH}");
  let blob = source
    .read_blob_exact("ses_current", "msg_user", &reference, 1)
    .unwrap()
    .unwrap();
  assert_eq!(blob.data, b"a");
  assert_eq!(blob.mime, "image/png");
  assert_eq!(blob.width, Some(1));
  assert!(
    source
      .read_blob_exact("ses_current_empty", "msg_user", &reference, 1)
      .unwrap()
      .is_none()
  );
  assert!(
    source
      .read_blob_exact("ses_current", "missing", &reference, 1)
      .unwrap()
      .is_none()
  );
  connection
    .execute("update blob_ref set seq=99 where message_id='msg_user'", [])
    .unwrap();
  assert!(
    source
      .read_blob_exact("ses_current", "msg_user", &reference, 1)
      .unwrap()
      .is_none()
  );
  connection.execute("update blob set data=x'62'", []).unwrap();
  assert!(
    source
      .read_blob_exact("ses_current", "msg_assistant", &reference, 1)
      .is_err()
  );
  for invalid in [
    "file:///fixture/image.png".to_owned(),
    format!("media:{}", HASH.to_uppercase()),
    "media:short".into(),
  ] {
    assert!(
      source
        .read_blob_exact("ses_current", "msg_assistant", &invalid, 1)
        .is_err()
    );
  }
  connection
    .execute("update blob set bytes=16777216,data=zeroblob(16777216)", [])
    .unwrap();
  assert!(
    source
      .read_blob_exact("ses_current", "msg_assistant", &reference, 1024)
      .is_err()
  );
  let mut cache = OpenCodeSessionCache::with_max_source_bytes(16384);
  assert!(
    source
      .load_session_records_cached_exact("ses_current", false, &mut cache)
      .is_ok(),
    "snapshots must not hydrate blob bodies"
  );
}

#[test]
fn inline_user_bytes_and_unknown_current_messages_remain_visible() {
  let (_directory, connection, source) = fixture();
  connection.execute("update session_message set data=?1 where id='msg_user'",[json!({"text":"inline","files":[{"data":"Yg==","mime":"image/png","source":{"type":"inline"}}],"time":{"created":110}}).to_string()]).unwrap();
  let loaded = source.load_session_exact("ses_current").unwrap();
  assert!(loaded.events.iter().any(|event| matches!(event,AgentEvent::Message(message) if message.role==Role::User && matches!(&message.images[0].source,MessageImageSource::Base64 { data } if data=="Yg=="))));
  connection
    .execute(
      "insert into session_message values ('msg_future','ses_current','future',99,200,200,'{\"future\":true}')",
      [],
    )
    .unwrap();
  assert!(
    source
      .load_session_exact("ses_current")
      .unwrap()
      .events
      .iter()
      .any(|event| matches!(event,AgentEvent::Unknown(event) if event.native.as_ref().unwrap()["future"]==true))
  );
}
