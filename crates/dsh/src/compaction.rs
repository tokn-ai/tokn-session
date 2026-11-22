use serde::Deserialize;
use serde_json::Value;
use tokn_dsh_protocol::{ContentBlock, EventRecord, TokenUsage};
use tokn_session_core::{
  AgentEvent, CompactionEvent, CompactionState as State, CompactionTokenScope, Provider, UsageEvent, UsageKind,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Summary {
  compaction_id: String,
  summary: Vec<ContentBlock>,
  shadowed_seqs: Vec<u64>,
  shadowed_range: Range,
  shadowed_token_count: u64,
  provider: String,
  model: String,
  usage: Option<TokenUsage>,
  llm_stream_call: Option<bool>,
  raw_output: Option<Vec<ContentBlock>>,
}
#[derive(Deserialize)]
struct Range {
  start: u64,
  end: u64,
}

/// None means unrelated. Some(None) is a malformed known record, which the
/// caller must retain as Unknown instead of mistaking it for a user message.
pub(crate) fn normalize(session_id: &str, raw: &Value) -> Option<Option<Vec<AgentEvent>>> {
  let kind = raw["type"].as_str()?;
  let checkpoint = kind == "user/message"
    && (raw.pointer("/data/source/kind").and_then(Value::as_str) == Some("compact-checkpoint")
      || raw.pointer("/data/source/kind").and_then(Value::as_str) == Some("plugin")
        && raw.pointer("/data/source/plugin").and_then(Value::as_str) == Some("compact"));
  if !checkpoint && !matches!(kind, "compaction/start" | "compaction/summary" | "compaction/end") {
    return None;
  }
  Some((|| {
    let record: EventRecord<Value> = serde_json::from_value(raw.clone()).ok()?;
    let data = &record.data;
    let id = if checkpoint {
      data.pointer("/source/compactionId")?
    } else {
      data.get("compactionId")?
    }
    .as_str()
    .filter(|id| !id.is_empty())?;
    let state = match kind {
      "compaction/start" => State::Started,
      "compaction/end" if data.get("error").is_some() => State::Failed,
      "compaction/end" => State::Completed,
      _ => State::SummaryGenerated,
    };
    let mut event = CompactionEvent::new(Provider::Dsh, Some(session_id.into()), state);
    event.compaction_id = Some(id.into());
    event.timestamp = Some(record.time.to_string());
    event.source_refs.push(record.seq.to_string());
    let mut usage_event = None;
    if matches!(kind, "compaction/start" | "compaction/end") {
      let turn = data.get("turn")?;
      if !turn.is_null() {
        event.turn_id = Some(turn.as_u64()?.to_string());
      }
      if let Some(error) = data.get("error") {
        event.reason = Some(error.as_str()?.into());
      }
    } else if kind == "compaction/summary" {
      let summary: Summary = serde_json::from_value(data.clone()).ok()?;
      if summary.llm_stream_call == Some(false)
        || (summary.llm_stream_call == Some(true) && summary.raw_output.is_none())
      {
        return None;
      }
      if summary.compaction_id != id
        || summary.shadowed_seqs.first() != Some(&summary.shadowed_range.start)
        || summary.shadowed_seqs.last() != Some(&summary.shadowed_range.end)
      {
        return None;
      }
      // Surface order is authoritative; numeric seqs can be non-monotonic.
      event.context.replaced_entry_ids = summary.shadowed_seqs.iter().map(u64::to_string).collect();
      event.summary = Some(text(&summary.summary)?);
      event.model_provider = Some(summary.provider);
      event.model_id = Some(summary.model);
      event.tokens(
        CompactionTokenScope::ReplacedContext,
        summary.shadowed_token_count,
        Some(true),
      );
      if let Some(usage) = summary.usage {
        let input = usage
          .input_tokens
          .checked_add(usage.cache_read_tokens.unwrap_or(0))?
          .checked_add(usage.cache_write_tokens.unwrap_or(0))?;
        usage_event = Some(AgentEvent::Usage(UsageEvent {
          provider: Provider::Dsh,
          session_id: Some(session_id.into()),
          kind: if summary.llm_stream_call == Some(true) {
            UsageKind::ModelCall
          } else {
            UsageKind::OperationTotal
          },
          turn_id: None,
          step_id: None,
          message_id: None,
          record_id: Some(record.seq.to_string()),
          input_tokens: input,
          output_tokens: usage.output_tokens,
          total_tokens: input.checked_add(usage.output_tokens),
          cache_read_tokens: usage.cache_read_tokens,
          cache_write_tokens: usage.cache_write_tokens,
          reasoning_tokens: usage.reasoning_tokens,
          native: data["usage"].clone(),
          timestamp: event.timestamp.clone(),
        }));
      }
    } else {
      let content: Vec<ContentBlock> = serde_json::from_value(data["content"].clone()).ok()?;
      event.summary = Some(text(&content)?);
      let id = data["id"].as_str()?;
      event.context.summary_message_ids.push(id.into());
      // A checkpoint is not a successful end. A later failed end must remain
      // distinguishable from an operation that completed successfully.
      let surface = raw.get("surfaceOp")?;
      if surface["op"] != "replace" || !surface["start"].is_u64() || !surface["end"].is_u64() {
        return None;
      }
    }
    let mut events = vec![AgentEvent::Compaction(event)];
    events.extend(usage_event);
    Some(events)
  })())
}

fn text(blocks: &[ContentBlock]) -> Option<String> {
  let mut result = Vec::new();
  for block in blocks {
    let raw = serde_json::to_value(block).ok()?;
    match raw["type"].as_str()? {
      "text" => result.push(raw["text"].as_str()?.to_owned()),
      "image" => result.push("[image]".into()),
      _ => return None,
    }
  }
  Some(result.join("\n"))
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;
  use tokn_session_core::compaction_operations;

  fn record(seq: u64, kind: &str, data: Value) -> Value {
    json!({"seq":seq,"time":100+seq,"type":kind,"data":data})
  }
  fn summary() -> Value {
    record(
      4,
      "compaction/summary",
      json!({"compactionId":"op","summary":[{"type":"text","text":"summary"}],
      "shadowedRange":{"start":8,"end":2},"shadowedSeqs":[8,2],"shadowedTokenCount":900,
      "provider":"provider","model":"summarizer","llmStreamCall":true,"rawOutput":[],
      "usage":{"inputTokens":100,"outputTokens":10,"cacheReadTokens":20}}),
    )
  }
  fn events(raw: Value) -> Vec<AgentEvent> {
    normalize("session", &raw).unwrap().unwrap()
  }

  #[test]
  fn summary_and_replacement_do_not_complete_the_operation_until_end() {
    let mut stream = events(record(3, "compaction/start", json!({"compactionId":"op","turn":null})));
    stream.extend(events(summary()));
    let mut checkpoint = record(
      5,
      "user/message",
      json!({"id":"replacement","content":[{"type":"text","text":"summary"}],
      "source":{"kind":"plugin","plugin":"compact","compactionId":"op"}}),
    );
    checkpoint["surfaceOp"] = json!({"op":"replace","start":8,"end":2});
    stream.extend(events(checkpoint));
    assert_eq!(compaction_operations(&stream)[0].event.state, State::SummaryGenerated);
    stream.extend(events(record(
      6,
      "compaction/end",
      json!({"compactionId":"op","turn":null}),
    )));
    let operations = compaction_operations(&stream);
    assert_eq!(operations.len(), 1);
    let e = &operations[0].event;
    assert_eq!(e.state, State::Completed);
    assert_eq!(e.context.replaced_entry_ids, ["8", "2"]);
    assert_eq!(e.context.summary_message_ids, ["replacement"]);
    assert_eq!(e.measurements[0].scope, CompactionTokenScope::ReplacedContext);
    assert_eq!(e.measurements[0].estimated, Some(true));
    assert!(
      stream
        .iter()
        .any(|e| matches!(e, AgentEvent::Usage(u) if u.kind == UsageKind::ModelCall && u.input_tokens == 120))
    );
    assert!(
      !stream
        .iter()
        .any(|e| matches!(e, AgentEvent::Message(_) | AgentEvent::Lifecycle(_)))
    );
  }

  #[test]
  fn failed_end_retains_generated_summary_and_unmarked_usage_is_not_a_model_call() {
    let mut raw = summary();
    raw["data"].as_object_mut().unwrap().remove("llmStreamCall");
    let mut stream = events(raw);
    assert!(
      stream
        .iter()
        .any(|e| matches!(e, AgentEvent::Usage(u) if u.kind == UsageKind::OperationTotal))
    );
    stream.extend(events(record(
      6,
      "compaction/end",
      json!({"compactionId":"op","turn":1,"error":"checkpoint failed"}),
    )));
    let operations = compaction_operations(&stream);
    assert_eq!(operations[0].event.state, State::Failed);
    assert_eq!(operations[0].event.summary.as_deref(), Some("summary"));
  }

  #[test]
  fn malformed_compaction_is_unknown_and_pruning_is_separate() {
    for (pointer, value) in [
      ("/data/shadowedRange/start", json!(2)),
      ("/data/llmStreamCall", json!(false)),
      ("/data/rawOutput", Value::Null),
      ("/data/usage/inputTokens", json!(-1)),
    ] {
      let mut raw = summary();
      *raw.pointer_mut(pointer).unwrap() = value;
      assert!(normalize("session", &raw).unwrap().is_none(), "{pointer}");
    }
    assert!(normalize("session", &record(1, "compaction/prune", json!({}))).is_none());
  }
}
