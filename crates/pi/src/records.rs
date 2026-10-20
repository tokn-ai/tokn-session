//! Non-conversation session records and explicitly scoped token accounting.
use serde::Deserialize;
use serde_json::{Value, json};
use tokn_session_core::{
  AgentEvent, CompactionEvent, CompactionState, CompactionTokenScope, MessageDelivery, MessageEvent, MessageProvenance,
  MetadataEvent, MetadataKind, Phase, Provider, Role, UnknownEvent, UsageEvent, UsageKind,
};

pub(crate) fn normalize(session_id: Option<String>, native: Value, timestamp: Option<String>) -> Vec<AgentEvent> {
  let Some((kind, summary)) = classify(&native) else {
    return vec![unknown(session_id, native, timestamp)];
  };
  if native["type"] == "custom_message" {
    let Some(text) = custom_text(&native["content"]) else {
      return vec![unknown(session_id, native, timestamp)];
    };
    return vec![AgentEvent::Message(MessageEvent {
      images: native["content"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, block)| {
          if block["type"] != "image" {
            return None;
          }
          Some(tokn_session_core::MessageImage {
            image_id: format!("image.v1.{index}"),
            mime_type: block["mimeType"].as_str().map(str::to_owned),
            name: None,
            source: block["data"]
              .as_str()
              .filter(|data| !data.is_empty())
              .map(|data| tokn_session_core::MessageImageSource::Base64 { data: data.into() })
              .unwrap_or(tokn_session_core::MessageImageSource::Unavailable { reference: None }),
          })
        })
        .collect(),
      provider: Provider::Pi,
      session_id,
      message_id: native["id"].as_str().map(str::to_owned),
      parent_id: native["parentId"].as_str().map(str::to_owned),
      // This is model-context text from an extension, not a human prompt.
      role: Role::System,
      delivery: MessageDelivery::Unspecified,
      phase: Phase::Finished,
      text,
      timestamp,
      provenance: Some(MessageProvenance {
        source: json!({"kind": "extension", "custom_type": native["customType"]}),
        display: native["display"].as_bool(),
        native: Some(native),
        surface_op: None,
        source_event_seqs: None,
      }),
    })];
  }
  let accounting = native
    .get("usage")
    .filter(|usage| !usage.is_null() && matches!(native["type"].as_str(), Some("compaction" | "branch_summary")))
    .map(|raw| {
      usage(
        session_id.clone(),
        native["id"].as_str().map(str::to_owned),
        false,
        UsageKind::OperationTotal,
        raw.clone(),
        timestamp.clone(),
      )
    });
  let observation = if native["type"] == "compaction" {
    let mut event = CompactionEvent::new(Provider::Pi, session_id.clone(), CompactionState::Completed);
    event.compaction_id = native["id"].as_str().map(str::to_owned);
    event.source_refs = event.compaction_id.iter().cloned().collect();
    event.timestamp = timestamp.clone();
    event.summary = native["summary"].as_str().map(str::to_owned);
    event.context.first_kept_entry_id = native["firstKeptEntryId"].as_str().map(str::to_owned);
    event.tokens(
      CompactionTokenScope::ContextBefore,
      native["tokensBefore"].as_u64().unwrap(),
      None,
    );
    AgentEvent::Compaction(event)
  } else {
    AgentEvent::Metadata(MetadataEvent {
      provider: Provider::Pi,
      session_id,
      kind,
      native_type: native["type"].as_str().unwrap().into(),
      summary,
      native,
      timestamp,
    })
  };
  let mut events = vec![observation];
  events.extend(accounting);
  events
}

fn classify(native: &Value) -> Option<(MetadataKind, String)> {
  // Optional wire fields keep decoding lossless; display classification is stricter.
  native["id"].as_str()?;
  native["timestamp"].as_str()?;
  if !native
    .get("parentId")
    .is_some_and(|value| value.is_null() || value.is_string())
  {
    return None;
  }
  Some(match native["type"].as_str()? {
    "compaction" => {
      native["summary"].as_str()?;
      native["firstKeptEntryId"].as_str()?;
      let tokens = native["tokensBefore"].as_u64()?;
      (
        MetadataKind::Context,
        format!("context compacted ({tokens} tokens before)"),
      )
    }
    "branch_summary" => {
      native["summary"].as_str()?;
      native["fromId"].as_str()?;
      (MetadataKind::Context, "branch summary".into())
    }
    "custom" => {
      let name = native["customType"].as_str()?;
      (MetadataKind::Session, format!("extension state: {name}"))
    }
    "custom_message" => {
      native["customType"].as_str()?;
      native["display"].as_bool()?;
      (MetadataKind::Context, "extension message".into())
    }
    "label" => {
      native["targetId"].as_str()?;
      optional_string(native, "label")?;
      (MetadataKind::Session, "entry label updated".into())
    }
    "session_info" => {
      optional_string(native, "name")?;
      (MetadataKind::Session, "session name updated".into())
    }
    "leaf" => {
      native["targetId"].as_str()?;
      (MetadataKind::Context, "active leaf changed".into())
    }
    "active_tools_change" => {
      let tools = native["activeToolNames"].as_array()?;
      if !tools.iter().all(Value::is_string) {
        return None;
      }
      (MetadataKind::Configuration, format!("active tools: {}", tools.len()))
    }
    _ => return None,
  })
}

fn optional_string(native: &Value, key: &str) -> Option<()> {
  match native.get(key) {
    None | Some(Value::String(_)) => Some(()),
    _ => None,
  }
}

fn custom_text(content: &Value) -> Option<String> {
  if let Some(text) = content.as_str() {
    return Some(text.into());
  }
  content
    .as_array()?
    .iter()
    .map(|block| match block["type"].as_str()? {
      "text" => block["text"].as_str().map(str::to_owned),
      "image" => {
        block["mimeType"].as_str()?;
        block["data"].as_str()?;
        Some(String::new())
      }
      _ => None,
    })
    .collect::<Option<Vec<_>>>()
    .map(|parts| {
      parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
    })
}

pub(crate) fn usage(
  session_id: Option<String>,
  record_id: Option<String>,
  is_message: bool,
  kind: UsageKind,
  native: Value,
  timestamp: Option<String>,
) -> AgentEvent {
  let counters = serde_json::from_value::<Usage>(native.clone()).ok();
  let input = counters.as_ref().and_then(|usage| {
    usage
      .input
      .checked_add(usage.cache_read.unwrap_or(0))?
      .checked_add(usage.cache_write.unwrap_or(0))
  });
  let (Some(counters), Some(input_tokens)) = (counters, input) else {
    return AgentEvent::Unknown(UnknownEvent {
      provider: Provider::Pi,
      session_id,
      native_type: Some("usage".into()),
      native: Some(native),
      timestamp,
    });
  };
  AgentEvent::Usage(UsageEvent {
    kind,
    provider: Provider::Pi,
    session_id,
    turn_id: None,
    step_id: None,
    message_id: if is_message { record_id.clone() } else { None },
    record_id,
    input_tokens,
    output_tokens: counters.output,
    total_tokens: counters.total_tokens,
    cache_read_tokens: counters.cache_read,
    cache_write_tokens: counters.cache_write,
    reasoning_tokens: counters.reasoning,
    native,
    timestamp,
  })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Usage {
  input: u64,
  output: u64,
  cache_read: Option<u64>,
  cache_write: Option<u64>,
  // Already included in cache_write, but still validate the optional counter.
  #[serde(rename = "cacheWrite1h")]
  _cache_write_1h: Option<u64>,
  reasoning: Option<u64>,
  total_tokens: Option<u64>,
}

fn unknown(session_id: Option<String>, native: Value, timestamp: Option<String>) -> AgentEvent {
  AgentEvent::Unknown(UnknownEvent {
    provider: Provider::Pi,
    session_id,
    native_type: native["type"].as_str().map(str::to_owned),
    native: Some(native),
    timestamp,
  })
}
