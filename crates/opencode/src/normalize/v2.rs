//! V2 projections are a distinct wire model; normalization never rewrites them
//! into V1 parts, so Inspector and unknown records retain the actual source.
use super::*;
use tokn_opencode_protocol::v2::{ContentItem, FileAttachment, MessageData, MessageItem, ModelRef};
use tokn_session_core::{CompactionEvent, CompactionState, ModelOrigin};

impl OpenCodeNormalizer {
  pub(crate) fn normalize_v2_message(&mut self, row: &MessageData) -> Vec<AgentEvent> {
    let native = row.native();
    let time = native.pointer("/time/created").and_then(Value::as_i64);
    let id = string_field(native, "id").unwrap_or_default();
    let mut events = Vec::new();
    match row.item() {
      MessageItem::User(message) => {
        events.push(self.v2_text(
          &id,
          Role::User,
          message.text.clone(),
          v2_images(&message.files, &id),
          Phase::Finished,
          time,
        ));
      }
      MessageItem::Assistant(message) => {
        events.extend(self.v2_model(&message.model, ModelOrigin::Response, None, time));
        let phase = if message.time.completed.is_some() || message.finish.is_some() || message.error.is_some() {
          Phase::Finished
        } else {
          Phase::Delta
        };
        for content in &message.content {
          match content.item() {
            ContentItem::Text(text) if content.native()["type"] == "reasoning" => {
              events.push(AgentEvent::Reasoning(ReasoningEvent {
                provenance: None,
                provider: self.provider,
                session_id: Some(self.session_id.clone()),
                message_id: Some(id.clone()),
                parent_id: None,
                phase,
                text: Some(text.text.clone()),
                summary: None,
                redacted: None,
                encrypted_content: None,
                signature: content
                  .native()
                  .pointer("/providerMetadata/anthropic/signature")
                  .and_then(Value::as_str)
                  .map(str::to_owned),
                timestamp: timestamp(time),
              }));
            }
            ContentItem::Text(text) => {
              events.push(self.v2_text(&id, Role::Assistant, text.text.clone(), Vec::new(), phase, time))
            }
            ContentItem::Tool(tool) => {
              let state = &tool.state;
              let status = state["status"].as_str();
              let input = state.get("input").cloned();
              let text = state
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|part| (part["type"] == "text").then(|| part["text"].as_str()).flatten())
                .collect::<Vec<_>>()
                .join("\n");
              let output = if status == Some("pending") {
                None
              } else {
                Some(json!({"output":text,"metadata":state.get("structured"),"error":state.get("error")}))
              };
              let mut images = state
                .get("attachments")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|file| serde_json::from_value::<FileAttachment>(file.clone()).ok())
                .collect::<Vec<_>>();
              images.extend(
                state
                  .get("content")
                  .and_then(Value::as_array)
                  .into_iter()
                  .flatten()
                  .filter(|file| file["type"] == "file")
                  .filter_map(|file| serde_json::from_value::<FileAttachment>(file.clone()).ok()),
              );
              events.push(AgentEvent::ToolCall(ToolCallEvent {
                provider: self.provider,
                session_id: Some(self.session_id.clone()),
                turn_id: None,
                message_id: Some(id.clone()),
                parent_id: None,
                record_kind: match status {
                  Some("pending") => ToolRecordKind::Invocation,
                  Some("running") => ToolRecordKind::Progress,
                  _ => ToolRecordKind::Snapshot,
                },
                tool_call_id: Some(tool.id.clone()),
                provider_tool_name: Some(tool.name.clone()),
                tool_name: Some(tool.name.clone()),
                tool_kind: tool_kind_for_optional_name(Some(&tool.name)),
                transport: Some(ToolTransport::Native),
                summary: tool_summary_for_io(Some(&tool.name), input.as_ref(), output.as_ref()),
                phase: match status {
                  Some("pending") => Phase::Started,
                  Some("running") => Phase::Updated,
                  Some("completed" | "error") => Phase::Finished,
                  _ => Phase::Updated,
                },
                input,
                output,
                images: v2_images(&images, &tool.id),
                is_error: match status {
                  Some("error") => Some(true),
                  Some("completed") => Some(metadata_exit_is_error(state.get("structured"))),
                  _ => None,
                },
                native: Some(content.native().clone()),
                timestamp: Some(tool.time.created.to_string()),
              }));
              if !matches!(status, Some("pending" | "running" | "completed" | "error")) {
                events.push(self.v2_unknown("tool.state", content.native(), time));
              }
            }
            ContentItem::Unknown(_) => events.push(self.v2_unknown("content", content.native(), time)),
          }
        }
        if let Some(native_tokens) = &message.tokens {
          let usage = serde_json::from_value::<tokn_opencode_protocol::v1::TokenUsage>(native_tokens.clone())
            .ok()
            .and_then(|tokens| {
              usage_event_for_provider(
                self.provider,
                Some(self.session_id.clone()),
                Some(id.clone()),
                None,
                Some(id.clone()),
                &tokens,
                native_tokens.clone(),
                timestamp(time),
              )
            });
          if let Some(usage) = usage {
            events.push(usage);
          } else {
            events.push(self.v2_unknown("usage", native, time));
          }
        }
        if let Some(error) = &message.error {
          events.extend(self.normalize_error(&id, native, error, time));
        }
      }
      MessageItem::ModelSwitched(message) => {
        events.extend(self.v2_model(&message.model, ModelOrigin::Selection, Some(id), time))
      }
      MessageItem::AgentSwitched(message) => events.push(AgentEvent::Metadata(MetadataEvent {
        provider: self.provider,
        session_id: Some(self.session_id.clone()),
        kind: MetadataKind::Configuration,
        native_type: "message.v2.agent-switched".into(),
        summary: format!("agent: {}", message.agent),
        native: native.clone(),
        timestamp: timestamp(time),
      })),
      MessageItem::Text(message) => events.push(self.v2_text(
        &id,
        Role::System,
        message.text.clone(),
        Vec::new(),
        Phase::Finished,
        time,
      )),
      MessageItem::Shell(message) => {
        let input = json!({"command":message.command});
        let output = json!({"output":message.output});
        events.push(AgentEvent::ToolCall(ToolCallEvent {
          provider: self.provider,
          session_id: Some(self.session_id.clone()),
          turn_id: None,
          message_id: Some(id),
          parent_id: None,
          record_kind: ToolRecordKind::Snapshot,
          tool_call_id: Some(message.call_id.clone()),
          provider_tool_name: Some("shell".into()),
          tool_name: Some("shell".into()),
          tool_kind: tool_kind_for_optional_name(Some("shell")),
          transport: Some(ToolTransport::Native),
          summary: tool_summary_for_io(Some("shell"), Some(&input), Some(&output)),
          phase: if message.time.completed.is_some() {
            Phase::Finished
          } else {
            Phase::Updated
          },
          input: Some(input),
          output: Some(output),
          images: Vec::new(),
          is_error: None,
          native: Some(native.clone()),
          timestamp: timestamp(time),
        }));
      }
      MessageItem::Compaction(message) => {
        let mut compact =
          CompactionEvent::new(self.provider, Some(self.session_id.clone()), CompactionState::Completed);
        compact.compaction_id = Some(id.clone());
        compact.timestamp = timestamp(time);
        compact.reason = Some(message.reason.clone());
        compact.summary = Some(message.summary.clone());
        compact.source_refs = vec![id];
        events.push(AgentEvent::Compaction(compact));
      }
      MessageItem::Unknown(_) => events.push(self.v2_unknown("message", native, time)),
    }
    events
  }

  fn v2_model(
    &mut self,
    model: &ModelRef,
    origin: ModelOrigin,
    id: Option<String>,
    time: Option<i64>,
  ) -> Vec<AgentEvent> {
    if !matches!(origin, ModelOrigin::Selection)
      && self.current_provider.as_deref() == Some(&model.provider_id)
      && self.current_model.as_deref() == Some(&model.id)
      && !self.current_model_is_catalog
    {
      return Vec::new();
    }
    self.current_provider = Some(model.provider_id.clone());
    self.current_model = Some(model.id.clone());
    self.current_model_is_catalog = false;
    vec![AgentEvent::ProviderChanged(ProviderChanged {
      model_origin: Some(origin),
      provider: self.provider,
      session_id: Some(self.session_id.clone()),
      native_id: id,
      native_parent_id: None,
      model_provider: Some(model.provider_id.clone()),
      model_id: Some(model.id.clone()),
      thinking_level: model.variant.clone(),
      timestamp: timestamp(time),
    })]
  }
  fn v2_text(
    &self,
    id: &str,
    role: Role,
    text: String,
    images: Vec<MessageImage>,
    phase: Phase,
    time: Option<i64>,
  ) -> AgentEvent {
    AgentEvent::Message(MessageEvent {
      provenance: None,
      provider: self.provider,
      session_id: Some(self.session_id.clone()),
      message_id: Some(id.into()),
      parent_id: None,
      role,
      delivery: if matches!(role, Role::Assistant) {
        MessageDelivery::Final
      } else {
        MessageDelivery::Unspecified
      },
      phase,
      text,
      images,
      timestamp: timestamp(time),
    })
  }
  fn v2_unknown(&self, kind: &str, native: &Value, time: Option<i64>) -> AgentEvent {
    unknown_event(
      self.provider,
      Some(self.session_id.clone()),
      Some(format!("v2.{kind}")),
      Some(native.clone()),
      timestamp(time),
    )
  }
}

fn v2_images(files: &[FileAttachment], owner: &str) -> Vec<MessageImage> {
  files
    .iter()
    .enumerate()
    .filter(|(_, file)| file.mime.starts_with("image/") || file.uri.starts_with("data:image/"))
    .map(|(index, file)| {
      MessageImage::from_url(
        format!("{owner}:file:{index}"),
        file.uri.clone(),
        Some(file.mime.clone()),
        file.name.clone(),
      )
    })
    .collect()
}
