use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use tokn_session_core::*;

pub(crate) fn session_started(reference: &SessionRef) -> AgentEvent {
  AgentEvent::SessionStarted(SessionStarted {
    provider: Provider::Qoder,
    session_id: reference.id.clone(),
    cwd: reference.cwd.clone(),
    timestamp: reference.timestamp.clone(),
  })
}

pub(crate) fn text_content(value: &Value) -> Option<String> {
  if let Some(text) = value.as_str() {
    return Some(text.to_owned());
  }
  let blocks = value.as_array()?;
  let texts: Vec<_> = blocks
    .iter()
    .filter(|block| block["type"] == "text")
    .filter_map(|block| block["text"].as_str())
    .collect();
  (!texts.is_empty()).then(|| texts.join("\n"))
}

pub(crate) fn normalize(reference: &SessionRef, rows: &[Value], active: &HashSet<usize>) -> Vec<Vec<AgentEvent>> {
  let mut normalizer = Normalizer {
    reference,
    calls: HashMap::new(),
  };
  rows
    .iter()
    .enumerate()
    .map(|(index, row)| {
      if reference.parent_session_id.is_none() && row["isSidechain"] == true {
        return Vec::new();
      }
      if reference.parent_session_id.is_some()
        && row["agentId"]
          .as_str()
          .is_some_and(|id| reference.id.rsplit('/').next() != Some(id))
      {
        return Vec::new();
      }
      let is_message = matches!(
        row["type"].as_str(),
        Some("user" | "assistant" | "system" | "attachment")
      );
      if is_message && !active.contains(&index) {
        let known_message = row["uuid"].is_string()
          && match row["type"].as_str() {
            Some("user" | "assistant") => {
              matches!(row["message"]["role"].as_str(), Some("user" | "assistant"))
                && (row["message"]["content"].is_string() || row["message"]["content"].is_array())
            }
            Some("system") => {
              row["subtype"] == "compact_boundary" || (row["subtype"].is_string() && row["content"].is_string())
            }
            _ => false,
          };
        return if known_message {
          Vec::new()
        } else {
          vec![normalizer.unknown(row.clone(), time(row))]
        };
      }
      normalizer.row(row)
    })
    .collect()
}

struct Normalizer<'a> {
  reference: &'a SessionRef,
  calls: HashMap<String, (String, Value)>,
}

impl Normalizer<'_> {
  fn row(&mut self, row: &Value) -> Vec<AgentEvent> {
    let kind = row["type"].as_str().unwrap_or("unknown");
    match kind {
      "user" | "assistant" => self.message(row),
      "system" if row["subtype"] == "compact_boundary" => {
        let mut event = CompactionEvent::new(
          Provider::Qoder,
          Some(self.reference.id.clone()),
          CompactionState::Completed,
        );
        event.compaction_id = string(row, "uuid");
        event.timestamp = time(row);
        event.trigger = row["compactMetadata"]["trigger"].as_str().map(str::to_owned);
        event.context.last_summarized_entry_id = string(row, "logicalParentUuid");
        event.source_refs = string(row, "uuid").into_iter().collect();
        vec![AgentEvent::Compaction(event)]
      }
      "system" if row["content"].is_string() && row["subtype"].is_string() => {
        vec![self.metadata(row, MetadataKind::Diagnostic, row["content"].as_str().unwrap())]
      }
      "custom-title" if row["customTitle"].is_string() => {
        vec![self.metadata(row, MetadataKind::Session, "Session title")]
      }
      "ai-title" if row["aiTitle"].is_string() => {
        vec![self.metadata(row, MetadataKind::Session, "Generated session title")]
      }
      "last-prompt" if row["lastPrompt"].is_string() => vec![self.metadata(row, MetadataKind::Session, "Last prompt")],
      "active-leaf"
        if row
          .get("leafUuid")
          .is_some_and(|leaf| leaf.is_string() || leaf.is_null())
          && row["explicit"].is_boolean() =>
      {
        vec![self.metadata(row, MetadataKind::Context, "Active conversation branch")]
      }
      "runtime-config" if row["timestamp"].is_number() => {
        vec![self.metadata(row, MetadataKind::Configuration, "Runtime configuration")]
      }
      "mode" if matches!(row["mode"].as_str(), Some("normal" | "coordinator")) => {
        vec![self.metadata(row, MetadataKind::Configuration, "Session mode")]
      }
      "relocated" if row["relocatedCwd"].is_string() => {
        vec![self.metadata(row, MetadataKind::Session, "Session directory changed")]
      }
      "summary" if row["summary"].is_string() && row["leafUuid"].is_string() => {
        vec![self.metadata(row, MetadataKind::Context, "Conversation summary")]
      }
      "tag" if row["tag"].is_string() => vec![self.metadata(row, MetadataKind::Session, "Session tag")],
      "workspace-directories"
        if row["directories"]
          .as_array()
          .is_some_and(|values| values.iter().all(Value::is_string)) =>
      {
        vec![self.metadata(row, MetadataKind::Configuration, "Workspace directories")]
      }
      "content-replacement" if row["replacements"].is_array() => {
        vec![self.metadata(row, MetadataKind::Context, "Provider content replacement")]
      }
      "file-history-snapshot" if row["snapshot"].is_object() && row["messageId"].is_string() => {
        vec![self.metadata(row, MetadataKind::Context, "File history snapshot")]
      }
      "token-stats" if row["promptTokenCount"].is_number() => {
        vec![self.metadata(row, MetadataKind::Context, "Context token estimate")]
      }
      "worktree-state" if row.get("worktreeSession").is_some() => {
        vec![self.metadata(row, MetadataKind::Session, "Worktree state")]
      }
      _ => vec![self.unknown(row.clone(), time(row))],
    }
  }

  fn message(&mut self, row: &Value) -> Vec<AgentEvent> {
    let role = match row["message"]["role"].as_str() {
      Some("user") => Role::User,
      Some("assistant") => Role::Assistant,
      _ => return vec![self.unknown(row.clone(), time(row))],
    };
    let content = &row["message"]["content"];
    let blocks = match content {
      Value::String(text) => vec![json!({"type":"text","text":text})],
      Value::Array(blocks) => blocks.clone(),
      _ => return vec![self.unknown(row.clone(), time(row))],
    };
    let mut events = Vec::new();
    if role == Role::Assistant
      && let Some(model) = row["message"]["model"].as_str().filter(|model| !model.is_empty())
    {
      events.push(AgentEvent::ProviderChanged(ProviderChanged {
        provider: Provider::Qoder,
        session_id: Some(self.reference.id.clone()),
        native_id: string(row, "uuid"),
        native_parent_id: string(row, "parentUuid"),
        model_provider: None,
        model_id: Some(model.to_owned()),
        model_origin: Some(ModelOrigin::Response),
        thinking_level: None,
        timestamp: time(row),
      }));
    }
    let mut text = Vec::new();
    let mut images = Vec::new();
    for (index, block) in blocks.into_iter().enumerate() {
      match block["type"].as_str() {
        Some("text") if block["text"].is_string() => text.push(block["text"].as_str().unwrap().to_owned()),
        Some("image") => images.push(image(&block, index)),
        _ => {
          self.flush_message(row, role, &mut text, &mut images, &mut events);
          match block["type"].as_str() {
            Some("thinking") if block["thinking"].is_string() => events.push(AgentEvent::Reasoning(ReasoningEvent {
              provenance: provenance(row),
              provider: Provider::Qoder,
              session_id: Some(self.reference.id.clone()),
              message_id: string(row, "uuid"),
              parent_id: string(row, "parentUuid"),
              phase: Phase::Finished,
              text: string(&block, "thinking"),
              summary: None,
              redacted: None,
              encrypted_content: None,
              signature: string(&block, "signature"),
              timestamp: time(row),
            })),
            Some("redacted_thinking") => events.push(AgentEvent::Reasoning(ReasoningEvent {
              provenance: provenance(row),
              provider: Provider::Qoder,
              session_id: Some(self.reference.id.clone()),
              message_id: string(row, "uuid"),
              parent_id: string(row, "parentUuid"),
              phase: Phase::Finished,
              text: None,
              summary: None,
              redacted: Some(true),
              encrypted_content: string(&block, "data"),
              signature: None,
              timestamp: time(row),
            })),
            Some("tool_use") if block["id"].is_string() && block["name"].is_string() => {
              let id = block["id"].as_str().unwrap().to_owned();
              let name = block["name"].as_str().unwrap().to_owned();
              self.calls.insert(id.clone(), (name.clone(), block["input"].clone()));
              events.push(self.tool(
                row,
                &block,
                id,
                Some(name),
                Some(block["input"].clone()),
                None,
                None,
                ToolRecordKind::Invocation,
              ));
            }
            Some("tool_result") if block["tool_use_id"].is_string() => {
              let id = block["tool_use_id"].as_str().unwrap().to_owned();
              let call = self.calls.get(&id);
              events.push(self.tool(
                row,
                &block,
                id.clone(),
                call.map(|(name, _)| name.clone()),
                call.map(|(_, input)| input.clone()),
                block.get("content").cloned(),
                block["is_error"].as_bool(),
                ToolRecordKind::Result,
              ));
            }
            _ => events.push(self.unknown(block, time(row))),
          }
        }
      }
    }
    self.flush_message(row, role, &mut text, &mut images, &mut events);
    if role == Role::Assistant
      && let Some(usage) = row["message"]["usage"].as_object()
    {
      let count = |key: &str| usage.get(key).and_then(Value::as_u64);
      if let (Some(input), Some(output)) = (count("input_tokens"), count("output_tokens")) {
        let cached = count("cache_read_input_tokens");
        let written = count("cache_creation_input_tokens");
        events.push(AgentEvent::Usage(UsageEvent {
          kind: UsageKind::ModelCall,
          provider: Provider::Qoder,
          session_id: Some(self.reference.id.clone()),
          turn_id: None,
          step_id: None,
          message_id: string(row, "uuid"),
          record_id: string(row, "uuid"),
          input_tokens: input
            .saturating_add(cached.unwrap_or(0))
            .saturating_add(written.unwrap_or(0)),
          output_tokens: output,
          total_tokens: None,
          cache_read_tokens: cached,
          cache_write_tokens: written,
          reasoning_tokens: count("thinking_tokens"),
          native: Value::Object(usage.clone()),
          timestamp: time(row),
        }));
      }
    }
    events
  }

  fn flush_message(
    &self,
    row: &Value,
    role: Role,
    text: &mut Vec<String>,
    images: &mut Vec<RecordedImage>,
    events: &mut Vec<AgentEvent>,
  ) {
    if text.is_empty() && images.is_empty() {
      return;
    }
    events.push(AgentEvent::Message(MessageEvent {
      provenance: provenance(row),
      provider: Provider::Qoder,
      session_id: Some(self.reference.id.clone()),
      message_id: string(row, "uuid"),
      parent_id: string(row, "parentUuid"),
      role,
      delivery: MessageDelivery::Unspecified,
      phase: Phase::Finished,
      text: std::mem::take(text).join("\n"),
      images: std::mem::take(images),
      timestamp: time(row),
    }));
  }
  #[allow(clippy::too_many_arguments)]
  fn tool(
    &self,
    row: &Value,
    block: &Value,
    id: String,
    name: Option<String>,
    input: Option<Value>,
    output: Option<Value>,
    is_error: Option<bool>,
    kind: ToolRecordKind,
  ) -> AgentEvent {
    let images = block["content"]
      .as_array()
      .map(|blocks| {
        blocks
          .iter()
          .enumerate()
          .filter(|(_, block)| block["type"] == "image")
          .map(|(index, block)| image(block, index))
          .collect()
      })
      .unwrap_or_default();
    AgentEvent::ToolCall(ToolCallEvent {
      provider: Provider::Qoder,
      session_id: Some(self.reference.id.clone()),
      turn_id: None,
      message_id: string(row, "uuid"),
      parent_id: string(row, "parentUuid"),
      record_kind: kind,
      tool_call_id: Some(id),
      provider_tool_name: name.clone(),
      tool_kind: tool_kind_for_optional_name(name.as_deref()),
      summary: tool_summary_for_io(name.as_deref(), input.as_ref(), output.as_ref()),
      transport: Some(ToolTransport::Native),
      tool_name: name,
      phase: if kind == ToolRecordKind::Invocation {
        Phase::Started
      } else {
        Phase::Finished
      },
      input,
      output,
      images,
      is_error,
      native: Some(block.clone()),
      timestamp: time(row),
    })
  }
  fn metadata(&self, row: &Value, kind: MetadataKind, summary: &str) -> AgentEvent {
    AgentEvent::Metadata(MetadataEvent {
      provider: Provider::Qoder,
      session_id: Some(self.reference.id.clone()),
      kind,
      native_type: row["type"].as_str().unwrap_or("unknown").to_owned(),
      summary: summary.to_owned(),
      native: row.clone(),
      timestamp: time(row),
    })
  }
  fn unknown(&self, native: Value, timestamp: Option<String>) -> AgentEvent {
    AgentEvent::Unknown(UnknownEvent {
      provider: Provider::Qoder,
      session_id: Some(self.reference.id.clone()),
      native_type: native["type"].as_str().map(str::to_owned),
      native: Some(native),
      timestamp,
    })
  }
}

fn string(value: &Value, key: &str) -> Option<String> {
  value[key].as_str().map(str::to_owned)
}
fn time(value: &Value) -> Option<String> {
  value["timestamp"]
    .as_str()
    .map(str::to_owned)
    .or_else(|| value["timestamp"].as_i64().map(|time| time.to_string()))
}
fn provenance(row: &Value) -> Option<MessageProvenance> {
  (row["isMeta"] == true).then(|| MessageProvenance {
    source: json!({"is_meta":true}),
    display: Some(false),
    native: None,
    surface_op: None,
    source_event_seqs: None,
  })
}
fn image(block: &Value, index: usize) -> RecordedImage {
  let source = &block["source"];
  let mime = source["media_type"].as_str().map(str::to_owned);
  let source = match source["type"].as_str() {
    Some("base64") if source["data"].is_string() => RecordedImageSource::Base64 {
      data: source["data"].as_str().unwrap().to_owned(),
    },
    _ => {
      return RecordedImage::from_url(
        format!("image-{index}"),
        source["url"].as_str().unwrap_or("").to_owned(),
        mime,
        None,
      );
    }
  };
  RecordedImage {
    image_id: format!("image-{index}"),
    mime_type: mime,
    name: None,
    source,
  }
}
