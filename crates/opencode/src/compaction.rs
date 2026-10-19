use crate::row::OpenCodeMessageRow;
use serde_json::Value;
use std::collections::BTreeSet;
use tokn_session_core::{
  AgentEvent, CompactionEvent, CompactionState as State, CompactionTokenScope as Scope, Provider, UnknownEvent,
};

fn string(value: &Value, key: &str) -> Option<String> {
  value[key].as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

pub(crate) fn normalize(
  provider: Provider,
  session_id: &str,
  row: &OpenCodeMessageRow,
  requests: &mut BTreeSet<String>,
) -> Option<Vec<AgentEvent>> {
  let raw = row.data.native();
  let summary_message = raw["role"] == "assistant" && raw["summary"] == true;
  let zcode_summary =
    provider == Provider::ZCode && raw.pointer("/semantics/kind").and_then(Value::as_str) == Some("compact_summary");
  let markers: Vec<_> = row
    .parts
    .iter()
    .filter(|part| {
      let native = part.data.native();
      native["type"] == "compaction"
        || (provider == Provider::ZCode
          && native["type"] == "timeline"
          && native["timelineType"] == "context_compaction")
    })
    .collect();
  if !summary_message && !zcode_summary && markers.is_empty() {
    return None;
  }
  let unknown = |native: Value, native_type: &str| {
    AgentEvent::Unknown(UnknownEvent {
      provider,
      session_id: Some(session_id.into()),
      native_type: Some(native_type.into()),
      native: Some(native),
      timestamp: row.time_created.map(|time| time.to_string()),
    })
  };
  if (summary_message && !matches!(row.data.item(), tokn_opencode_protocol::v1::MessageItem::Assistant(_)))
    || (zcode_summary && markers.is_empty())
  {
    return Some(vec![unknown(serde_json::to_value(row).unwrap(), "compaction.summary")]);
  }
  let summary = row
    .parts
    .iter()
    .filter_map(|part| {
      let raw = part.data.native();
      (raw["type"] == "text").then(|| raw["text"].as_str()).flatten()
    })
    .collect::<Vec<_>>()
    .join("\n");
  let mut events = Vec::new();
  if summary_message && markers.is_empty() {
    let error = raw.get("error").filter(|error| !error.is_null());
    let detail = error.and_then(crate::errors::classify);
    if error.is_some() && detail.is_none() {
      // The IR has no unknown compaction outcome. Preserve the complete row
      // without claiming completion/failure from an unsupported error value.
      return Some(vec![unknown(serde_json::to_value(row).unwrap(), "compaction.summary")]);
    }
    let state = if detail.as_ref().is_some_and(|error| error.cancelled) {
      State::Interrupted
    } else if detail.is_some() {
      State::Failed
    } else if raw["finish"].as_str().is_some_and(|s| !s.is_empty()) {
      State::Completed
    } else {
      State::SummaryGenerated
    };
    let mut event = CompactionEvent::new(provider, Some(session_id.into()), state);
    event.compaction_id = string(raw, "parentID")
      .filter(|id| requests.contains(id))
      .or_else(|| Some(row.id.clone()));
    event.summary = (!summary.is_empty()).then_some(summary);
    event.model_provider = string(raw, "providerID");
    event.model_id = string(raw, "modelID");
    event.timestamp = row.time_created.map(|time| time.to_string());
    event.source_refs.push(row.id.clone());
    event.context.summary_message_ids.push(row.id.clone());
    if let Some(error) = &detail {
      event.reason = Some(error.message.clone());
    }
    events.push(AgentEvent::Compaction(event));
    if let Some(error) = detail {
      events.push(crate::errors::diagnostic(
        provider,
        Some(session_id.into()),
        &error,
        "message.error",
        serde_json::json!({"id":row.id, "data":raw}),
        row.time_created.map(|time| time.to_string()),
      ));
    }
    return Some(events);
  }
  for part in markers {
    let native = part.data.native();
    let boundary = &native["compactBoundary"];
    let status = native["timelineStatus"].as_str().or_else(|| native["status"].as_str());
    if matches!(part.data.item(), tokn_opencode_protocol::v1::PartItem::Unknown(_)) && native["type"] == "compaction" {
      events.push(unknown(native.clone(), "compaction"));
      continue;
    }
    let state = if provider == Provider::ZCode {
      match status {
        Some("started" | "retrying") => State::Started,
        Some("completed") => State::Completed,
        Some("failed") => State::Failed,
        Some("interrupted") => State::Interrupted,
        Some("skipped") => State::Skipped,
        Some(_) => {
          events.push(unknown(native.clone(), "compaction"));
          continue;
        }
        None if boundary["boundaryId"].as_str().is_some() => State::Completed,
        None if native["auto"].is_boolean() => State::Requested,
        None => {
          events.push(unknown(native.clone(), "compaction"));
          continue;
        }
      }
    } else {
      // Persisted V1 markers are requests, not starts or completed summaries.
      if !native["auto"].is_boolean() {
        events.push(unknown(native.clone(), "compaction"));
        continue;
      }
      State::Requested
    };
    let mut event = CompactionEvent::new(provider, Some(session_id.into()), state);
    event.compaction_id = string(native, "operationId")
      .or_else(|| string(boundary, "boundaryId"))
      .or_else(|| Some(row.id.clone()));
    event.timestamp = native
      .pointer("/time/end")
      .or_else(|| native.pointer("/time/start"))
      .and_then(Value::as_u64)
      .map(|time| time.to_string())
      .or_else(|| part.time_created.map(|time| time.to_string()));
    event.source_refs = vec![row.id.clone(), part.id.clone()];
    event.trigger = string(native, "trigger")
      .or_else(|| string(boundary, "trigger"))
      .or_else(|| {
        native["auto"]
          .as_bool()
          .map(|auto| if auto { "auto" } else { "manual" }.into())
      });
    event.reason = string(native, "compactReason")
      .or_else(|| string(boundary, "compactReason"))
      .or_else(|| string(native, "reason"))
      .or_else(|| (native["overflow"] == true).then(|| "context_overflow".into()));
    event.provider_phase = string(native, "phase").or_else(|| string(boundary, "phase"));
    event.turn_id = string(boundary, "turnId");
    // ZCode writes lastSummarizedMessageId into the same V1 field for boundary
    // rows. Only OpenCode establishes it as the first retained entry.
    if provider == Provider::OpenCode {
      event.context.first_kept_entry_id = string(native, "tail_start_id");
    }
    event.context.last_summarized_entry_id = string(boundary, "lastSummarizedMessageId");
    event.context.summarized_message_count = boundary["summarizedMessageCount"].as_u64();
    event.context.kept_message_count = boundary["keptMessageCount"].as_u64();
    event.context.summary_message_ids = boundary["summaryMessageIds"]
      .as_array()
      .map(|ids| ids.iter().filter_map(Value::as_str).map(str::to_owned).collect())
      .unwrap_or_else(|| string(native, "summaryMessageId").into_iter().collect());
    for (field, scope, estimated) in [
      ("preCompactTokenCount", Scope::ContextBefore, None),
      ("truePostCompactTokenCount", Scope::ContextAfter, Some(true)),
    ] {
      if let Some(tokens) = native[field].as_u64().or_else(|| boundary[field].as_u64()) {
        event.tokens(scope, tokens, estimated);
      }
    }
    // postCompactTokenCount is summarizer usage, NOT rebuilt context size.
    if boundary.is_object() || zcode_summary {
      event.summary = (!summary.is_empty()).then(|| summary.clone());
    }
    requests.insert(row.id.clone());
    events.push(AgentEvent::Compaction(event));
  }
  (!events.is_empty()).then_some(events)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{normalize::OpenCodeNormalizer, row::OpenCodePartRow};
  use serde_json::json;
  use tokn_session_core::compaction_operations;

  fn row(id: &str, message: Value, parts: Vec<Value>) -> OpenCodeMessageRow {
    OpenCodeMessageRow {
      id: id.into(),
      time_created: Some(100),
      data: serde_json::from_value(message).unwrap(),
      parts: parts
        .into_iter()
        .enumerate()
        .map(|(i, part)| OpenCodePartRow {
          id: format!("{id}-part-{i}"),
          time_created: Some(100),
          data: serde_json::from_value(part).unwrap(),
        })
        .collect(),
    }
  }

  #[test]
  fn opencode_request_partial_summary_and_completion_are_not_conversation_messages() {
    let mut normalizer = OpenCodeNormalizer::new("session".into());
    let mut events = normalizer.normalize_message(row(
      "request",
      json!({"role":"user"}),
      vec![json!({"type":"compaction","auto":true,"overflow":true})],
    ));
    assert!(matches!(&events[..], [AgentEvent::Compaction(e)] if e.state == State::Requested));
    let mut summary =
      json!({"role":"assistant","summary":true,"parentID":"request","modelID":"summarizer","providerID":"provider"});
    let parts = vec![json!({"type":"text","text":"## Kept context\nDecisions"})];
    events.extend(normalizer.normalize_message(row("summary", summary.clone(), parts.clone())));
    assert_eq!(compaction_operations(&events)[0].event.state, State::SummaryGenerated);
    summary["finish"] = json!("stop");
    events.extend(normalizer.normalize_message(row("summary", summary, parts)));
    let operations = compaction_operations(&events);
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].event.state, State::Completed);
    assert_eq!(operations[0].event.trigger.as_deref(), Some("auto"));
    assert_eq!(operations[0].event.reason.as_deref(), Some("context_overflow"));
    assert_eq!(operations[0].event.model_id.as_deref(), Some("summarizer"));
    assert!(events.iter().all(|e| matches!(e, AgentEvent::Compaction(_))));
  }

  #[test]
  fn failed_summary_and_malformed_markers_are_never_final_replies() {
    let mut normalizer = OpenCodeNormalizer::new("session".into());
    let events = normalizer.normalize_message(row(
      "summary",
      json!({"role":"assistant","summary":true,
      "error":{"data":{"message":"cancelled"}},"finish":"stop"}),
      vec![],
    ));
    assert!(
      matches!(&events[..], [AgentEvent::Compaction(e), AgentEvent::Metadata(_)] if e.state == State::Failed && e.reason.as_deref() == Some("cancelled"))
    );
    for bad in [json!({"type":"compaction"}), json!({"type":"compaction","auto":"yes"})] {
      let events = normalizer.normalize_message(row("broken", json!({"role":"user"}), vec![bad]));
      assert!(matches!(&events[..], [AgentEvent::Unknown(_)]));
    }
    let events = normalizer.normalize_message(row(
      "broken",
      json!({"role":"assistant","summary":true,"finish":123}),
      vec![json!({"type":"text","text":"not a reply"})],
    ));
    assert!(matches!(&events[..], [AgentEvent::Unknown(_)]));
  }

  #[test]
  fn zcode_timeline_and_boundary_keep_operation_identity_and_correct_token_scope() {
    // Based on the installed ZCode 3.7.3 bundle. Lifecycle phase is distinct
    // from timing phase; postCompactTokenCount is NOT context-after usage.
    let mut normalizer = OpenCodeNormalizer::with_provider("session".into(), Provider::ZCode);
    let mut events = normalizer.normalize_message(row(
      "request",
      json!({"role":"user"}),
      vec![
        json!({"type":"compaction","auto":true,"operationId":"op","timelineStatus":"completed",
        "trigger":"reactive","phase":"mid_turn","compactReason":"provider_overflow",
        "preCompactTokenCount":1000,"postCompactTokenCount":800,"truePostCompactTokenCount":200}),
        json!({"type":"timeline","timelineType":"context_compaction","operationId":"op","timelineStatus":"completed"}),
      ],
    ));
    events.extend(normalizer.normalize_message(row(
      "summary",
      json!({"role":"user","semantics":{"kind":"compact_summary"}}),
      vec![
        json!({"type":"text","text":"preserved summary"}),
        json!({"type":"compaction","auto":true,"operationId":"op","tail_start_id":"last","compactBoundary":{
        "boundaryId":"boundary","summarizedMessageCount":10,"keptMessageCount":2,"lastSummarizedMessageId":"last",
        "summaryMessageIds":["summary"]}}),
      ],
    )));
    let operations = compaction_operations(&events);
    assert_eq!(operations.len(), 1);
    let e = &operations[0].event;
    assert_eq!(e.provider, Provider::ZCode);
    assert_eq!(e.state, State::Completed);
    assert_eq!(e.provider_phase.as_deref(), Some("mid_turn"));
    assert_eq!(e.summary.as_deref(), Some("preserved summary"));
    assert_eq!(e.context.last_summarized_entry_id.as_deref(), Some("last"));
    assert!(e.context.first_kept_entry_id.is_none());
    assert_eq!(e.measurements.len(), 2);
    assert!(
      e.measurements
        .iter()
        .any(|m| m.scope == Scope::ContextAfter && m.tokens == 200 && m.estimated == Some(true))
    );
    assert!(events.iter().all(|e| matches!(e, AgentEvent::Compaction(_))));
  }

  #[test]
  fn zcode_supported_terminal_states_and_retry_are_explicit() {
    for (native_state, state) in [
      ("started", State::Started),
      ("retrying", State::Started),
      ("skipped", State::Skipped),
      ("interrupted", State::Interrupted),
      ("failed", State::Failed),
    ] {
      let mut normalizer = OpenCodeNormalizer::with_provider("session".into(), Provider::ZCode);
      let events = normalizer.normalize_message(row(
        "request",
        json!({"role":"user"}),
        vec![json!({"type":"compaction","operationId":"op","timelineStatus":native_state,"phase":"pre_request"})],
      ));
      assert!(
        matches!(&events[..], [AgentEvent::Compaction(e)] if e.state == state && e.provider_phase.as_deref() == Some("pre_request"))
      );
    }
  }
}
