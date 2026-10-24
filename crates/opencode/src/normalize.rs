use crate::row::{OpenCodeMessageRow, OpenCodePartRow, OpenCodeSessionEntryRow, OpenCodeSessionRow};
use serde_json::{Value, json};
use tokn_opencode_protocol::v1::{MessageItem, PartItem, TokenUsage, ToolState, ToolStateItem};
use tokn_session_core::{
  AgentEvent, MessageDelivery, MessageEvent, MessageImage, MessageImageSource, MessageProvenance, MetadataEvent,
  MetadataKind, Phase, Provider, ProviderChanged, ReasoningEvent, Role, SessionStarted, ToolCallEvent, ToolRecordKind,
  ToolTransport, UnknownEvent, UsageEvent, UsageKind, tool_kind_for_optional_name, tool_summary_for_io,
};

mod v2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenCodeNormalizer {
  compaction_requests: std::collections::BTreeSet<String>,
  provider: Provider,
  session_id: String,
  current_provider: Option<String>,
  current_model: Option<String>,
  current_model_is_catalog: bool,
}

impl OpenCodeNormalizer {
  pub fn new(session_id: String) -> Self {
    Self::with_provider(session_id, Provider::OpenCode)
  }

  pub(crate) fn with_provider(session_id: String, provider: Provider) -> Self {
    Self {
      compaction_requests: Default::default(),
      provider,
      session_id,
      current_provider: None,
      current_model: None,
      current_model_is_catalog: false,
    }
  }

  pub fn normalize_session(&mut self, row: &OpenCodeSessionRow) -> Vec<AgentEvent> {
    let mut events = vec![AgentEvent::SessionStarted(SessionStarted {
      provider: self.provider,
      session_id: row.id.clone(),
      cwd: row.directory.clone(),
      timestamp: timestamp(row.time_created),
    })];

    if let Some(model) = &row.model
      && (model.provider_id.is_some() || model.id.is_some())
    {
      self.current_provider = model.provider_id.clone();
      self.current_model = model.id.clone();
      self.current_model_is_catalog = true;
      events.push(AgentEvent::ProviderChanged(ProviderChanged {
        model_origin: Some(tokn_session_core::ModelOrigin::Catalog),
        provider: self.provider,
        session_id: Some(row.id.clone()),
        native_id: None,
        native_parent_id: None,
        model_provider: model.provider_id.clone(),
        model_id: model.id.clone(),
        thinking_level: None,
        timestamp: timestamp(row.time_created),
      }));
    }

    events
  }

  pub fn normalize_message(&mut self, row: OpenCodeMessageRow) -> Vec<AgentEvent> {
    if let Some(mut events) =
      crate::compaction::normalize(self.provider, &self.session_id, &row, &mut self.compaction_requests)
    {
      // Reuse accounting validation, but never expose the internal summary as
      // a final assistant reply or change the conversation's active model.
      if let MessageItem::Assistant(message) = row.data.item() {
        let accounting = self.normalize_assistant_turn(
          &row.id,
          &message.parent_id,
          row.time_created,
          message.tokens.as_ref(),
          row.data.native(),
          None,
          true,
          row
            .parts
            .iter()
            .filter(|part| {
              let native = part.data.native();
              native["type"] != "compaction"
                && !(self.provider == Provider::ZCode
                  && native["type"] == "timeline"
                  && native["timelineType"] == "context_compaction")
            })
            .cloned()
            .collect(),
        );
        events.extend(
          accounting
            .into_iter()
            .filter(|event| matches!(event, AgentEvent::Usage(_) | AgentEvent::Unknown(_))),
        );
      }
      return events;
    }
    let OpenCodeMessageRow {
      id,
      time_created,
      data,
      parts,
    } = row;
    let (message, native) = data.into_parts();
    let parent_id = string_field(&native, "parentID");
    let provenance = message_provenance(self.provider, &native);
    let mut events = if provenance.as_ref().and_then(|source| source.display) == Some(false) {
      Vec::new()
    } else {
      self.normalize_model(&message, &native, time_created)
    };

    match message {
      MessageItem::User(_) => {
        events.extend(self.text_message(id, parent_id, Role::User, provenance, time_created, parts));
      }
      MessageItem::Assistant(message) => {
        events.extend(self.normalize_assistant_turn(
          &id,
          &message.parent_id,
          time_created,
          message.tokens.as_ref(),
          &native,
          provenance,
          true,
          parts,
        ));
        // The terminal response outcome must follow its own partial content
        // and accounting, or reasoning/tool parts can revive active state.
        if let Some(error) = message.error {
          events.extend(self.normalize_error(&id, &native, &error, time_created));
        }
      }
      MessageItem::Unknown(item) if item.native_type.as_deref() == Some("system") => {
        events.extend(self.text_message(id, parent_id, Role::System, provenance, time_created, parts));
      }
      MessageItem::Unknown(item) if item.native_type.as_deref() == Some("user") => {
        events.extend(self.text_message(id, parent_id, Role::User, provenance, time_created, parts));
        events.push(self.unknown_message(item, time_created));
      }
      MessageItem::Unknown(item) if item.native_type.as_deref() == Some("assistant") => {
        let error = item.native.get("error").filter(|error| !error.is_null()).cloned();
        let recovered_tokens = recover_token_usage(&native);
        events.extend(self.normalize_assistant_turn(
          &id,
          &parent_id,
          time_created,
          recovered_tokens.as_ref(),
          &native,
          provenance,
          false,
          parts,
        ));
        events.push(self.unknown_message(item, time_created));
        if let Some(error) = error {
          events.extend(self.normalize_error(&id, &native, &error, time_created));
        }
      }
      MessageItem::Unknown(item) => {
        events.push(self.unknown_message(item, time_created));
      }
    }

    events
  }

  fn normalize_error(
    &self,
    message_id: &str,
    data: &Value,
    error: &Value,
    time_created: Option<i64>,
  ) -> Vec<AgentEvent> {
    // SQLite keeps message identity outside data. Preserve both the original
    // envelope and relational ID when projecting an ErrorEvent without ID fields.
    let native = json!({"id":message_id, "data":data});
    crate::errors::normalize(
      self.provider,
      Some(self.session_id.clone()),
      error,
      "message.error",
      native.clone(),
      timestamp(time_created),
    )
    .unwrap_or_else(|| {
      vec![unknown_event(
        self.provider,
        Some(self.session_id.clone()),
        Some("message.error".into()),
        Some(native),
        timestamp(time_created),
      )]
    })
  }

  pub(crate) fn normalize_session_entry(&mut self, row: OpenCodeSessionEntryRow) -> AgentEvent {
    let OpenCodeSessionEntryRow {
      id,
      native_type,
      time_created,
      data,
    } = row;
    let native = json!({
      "id": id,
      "type": native_type,
      "data": data,
    });

    match native_type.as_str() {
      "runtime/model_selection" => {
        let provider_id = string_field(&data, "providerId");
        let model_id = string_field(&data, "modelId");
        let thinking_level = string_field(&data, "thoughtLevel");
        if provider_id.is_none() && model_id.is_none() {
          return unknown_event(
            self.provider,
            Some(self.session_id.clone()),
            Some(native_type),
            Some(native),
            timestamp(time_created),
          );
        }
        self.current_provider.clone_from(&provider_id);
        self.current_model.clone_from(&model_id);
        self.current_model_is_catalog = false;
        AgentEvent::ProviderChanged(ProviderChanged {
          model_origin: Some(tokn_session_core::ModelOrigin::Selection),
          provider: self.provider,
          session_id: Some(self.session_id.clone()),
          native_id: Some(id),
          native_parent_id: None,
          model_provider: provider_id,
          model_id,
          thinking_level,
          timestamp: timestamp(time_created),
        })
      }
      "runtime/bash_shell_selection" => AgentEvent::Metadata(MetadataEvent {
        provider: self.provider,
        session_id: Some(self.session_id.clone()),
        kind: MetadataKind::Configuration,
        native_type,
        summary: "bash shell selection".to_string(),
        native,
        timestamp: timestamp(time_created),
      }),
      "runtime/workspace_checkpoint" => AgentEvent::Metadata(MetadataEvent {
        provider: self.provider,
        session_id: Some(self.session_id.clone()),
        kind: MetadataKind::Context,
        native_type,
        summary: "workspace checkpoint".to_string(),
        native,
        timestamp: timestamp(time_created),
      }),
      "runtime/user_input_auto_resolution" => AgentEvent::Metadata(MetadataEvent {
        provider: self.provider,
        session_id: Some(self.session_id.clone()),
        kind: MetadataKind::Diagnostic,
        native_type,
        summary: "user input auto-resolution".to_string(),
        native,
        timestamp: timestamp(time_created),
      }),
      _ => unknown_event(
        self.provider,
        Some(self.session_id.clone()),
        Some(native_type),
        Some(native),
        timestamp(time_created),
      ),
    }
  }

  fn unknown_message(&self, item: tokn_opencode_protocol::UnknownItem, time_created: Option<i64>) -> AgentEvent {
    unknown_event(
      self.provider,
      Some(self.session_id.clone()),
      item
        .native_type
        .as_deref()
        .map(|role| format!("message.role.{role}"))
        .or_else(|| Some("message.role.unknown".to_string())),
      Some(item.native),
      timestamp(time_created),
    )
  }

  fn text_message(
    &self,
    message_id: String,
    parent_id: Option<String>,
    role: Role,
    provenance: Option<MessageProvenance>,
    time_created: Option<i64>,
    parts: Vec<OpenCodePartRow>,
  ) -> Vec<AgentEvent> {
    let mut texts = Vec::new();
    let mut images = Vec::new();
    let mut unknowns = Vec::new();
    for part in parts {
      let (item, native) = part.data.into_parts();
      match item {
        PartItem::Text(part) => texts.push(part.text),
        PartItem::File(file) if is_image_file(&file) => images.push(recorded_file_image(file, part.id)),
        item => unknowns.push(unknown_event(
          self.provider,
          Some(self.session_id.clone()),
          Some(format!("part.{}", item.native_type().unwrap_or("unknown"))),
          Some(native),
          timestamp(part.time_created),
        )),
      }
    }

    let text = texts.join("\n");
    let mut events = Vec::with_capacity(unknowns.len() + 1);
    if !text.is_empty() || !images.is_empty() {
      events.push(AgentEvent::Message(MessageEvent {
        images,
        provenance,
        provider: self.provider,
        session_id: Some(self.session_id.clone()),
        message_id: Some(message_id),
        parent_id,
        role,
        delivery: MessageDelivery::Unspecified,
        phase: Phase::Finished,
        text,
        timestamp: timestamp(time_created),
      }));
    }
    events.extend(unknowns);
    events
  }

  fn normalize_model(&mut self, message: &MessageItem, native: &Value, time_created: Option<i64>) -> Vec<AgentEvent> {
    let (provider_id, model_id) = match message {
      MessageItem::User(message) => (
        message.model.as_ref().and_then(|model| model.provider_id.clone()),
        message.model.as_ref().and_then(|model| model.model_id.clone()),
      ),
      MessageItem::Assistant(message) => (message.provider_id.clone(), message.model_id.clone()),
      MessageItem::Unknown(_) => model_from_native(native),
    };

    if provider_id == self.current_provider && model_id == self.current_model && !self.current_model_is_catalog {
      return Vec::new();
    }
    if provider_id.is_none() && model_id.is_none() {
      return Vec::new();
    }

    self.current_provider = provider_id.clone();
    self.current_model = model_id.clone();
    self.current_model_is_catalog = false;
    vec![AgentEvent::ProviderChanged(ProviderChanged {
      model_origin: Some(match message {
        MessageItem::Assistant(_) => tokn_session_core::ModelOrigin::Response,
        _ => tokn_session_core::ModelOrigin::Request,
      }),
      provider: self.provider,
      session_id: Some(self.session_id.clone()),
      native_id: None,
      native_parent_id: None,
      model_provider: provider_id,
      model_id,
      thinking_level: None,
      timestamp: timestamp(time_created),
    })]
  }

  fn normalize_assistant_part(
    &self,
    message_id: &str,
    parent_id: &Option<String>,
    provenance: &Option<MessageProvenance>,
    message_phase: Phase,
    part: OpenCodePartRow,
  ) -> Vec<AgentEvent> {
    let OpenCodePartRow {
      id: part_id,
      time_created,
      data,
    } = part;
    let (item, native) = data.into_parts();
    let native_type = item.native_type().map(str::to_string);

    match item {
      PartItem::Text(part) => vec![AgentEvent::Message(MessageEvent {
        images: Vec::new(),
        provenance: provenance.clone(),
        provider: self.provider,
        session_id: Some(self.session_id.clone()),
        message_id: Some(message_id.to_string()),
        parent_id: parent_id.clone(),
        role: Role::Assistant,
        delivery: MessageDelivery::Final,
        phase: message_phase,
        text: part.text,
        timestamp: timestamp(time_created),
      })],
      PartItem::File(file) if is_image_file(&file) => vec![AgentEvent::Message(MessageEvent {
        images: vec![recorded_file_image(file, part_id)],
        provenance: provenance.clone(),
        provider: self.provider,
        session_id: Some(self.session_id.clone()),
        message_id: Some(message_id.to_string()),
        parent_id: parent_id.clone(),
        role: Role::Assistant,
        delivery: MessageDelivery::Final,
        phase: message_phase,
        text: String::new(),
        timestamp: timestamp(time_created),
      })],
      PartItem::Reasoning(part) => {
        let signature = part
          .metadata
          .as_ref()
          .and_then(|metadata| metadata.pointer("/anthropic/signature"))
          .and_then(Value::as_str)
          .map(str::to_string);
        vec![AgentEvent::Reasoning(ReasoningEvent {
          provenance: provenance.clone(),
          provider: self.provider,
          session_id: Some(self.session_id.clone()),
          message_id: Some(message_id.to_string()),
          parent_id: parent_id.clone(),
          phase: Phase::Finished,
          text: Some(part.text),
          summary: None,
          redacted: None,
          encrypted_content: None,
          signature,
          timestamp: timestamp(time_created),
        })]
      }
      PartItem::Tool(part) => vec![tool_event(
        self.provider,
        self.session_id.clone(),
        message_id.to_string(),
        parent_id.clone(),
        part.call_id.or(part.identity.id).or(Some(part_id)),
        part.tool,
        part.state,
        time_created,
      )],
      PartItem::StepStart(_) | PartItem::StepFinish(_) => Vec::new(),
      _ => vec![unknown_event(
        self.provider,
        Some(self.session_id.clone()),
        Some(format!("part.{}", native_type.as_deref().unwrap_or("unknown"))),
        Some(native),
        timestamp(time_created),
      )],
    }
  }

  /// OpenCode writes token snapshots both on the assistant message and on its
  /// `step-finish` parts. A turn may contain several steps, so the final valid
  /// step snapshot is authoritative. The assistant-message snapshot remains a
  /// compatibility fallback for incomplete or older rows.
  fn normalize_assistant_turn(
    &self,
    message_id: &str,
    parent_id: &Option<String>,
    message_time_created: Option<i64>,
    message_tokens: Option<&TokenUsage>,
    message_native: &Value,
    provenance: Option<MessageProvenance>,
    report_invalid_message_usage: bool,
    parts: Vec<OpenCodePartRow>,
  ) -> Vec<AgentEvent> {
    let mut malformed_usage = Vec::new();
    let fallback_usage = message_tokens.and_then(|tokens| {
      let native = usage_native(message_native);
      match usage_event_for_provider(
        self.provider,
        Some(self.session_id.clone()),
        Some(message_id.to_string()),
        None,
        Some(message_id.to_string()),
        tokens,
        native.clone(),
        timestamp(message_time_created),
      ) {
        Some(event) => Some(event),
        None => {
          if report_invalid_message_usage {
            malformed_usage.push(unknown_event(
              self.provider,
              Some(self.session_id.clone()),
              Some("usage".to_string()),
              Some(native),
              timestamp(message_time_created),
            ));
          }
          None
        }
      }
    });

    let mut latest_step_usage = None;
    for part in &parts {
      let (tokens, report_invalid_step_usage) = match part.data.item() {
        PartItem::StepFinish(step) => (step.tokens.clone(), true),
        PartItem::Unknown(item) if item.native_type.as_deref() == Some("step-finish") => {
          (recover_token_usage(part.data.native()), false)
        }
        _ => continue,
      };
      let Some(tokens) = tokens.as_ref() else {
        continue;
      };

      let native = usage_native(part.data.native());
      match usage_event_for_provider(
        self.provider,
        Some(self.session_id.clone()),
        Some(message_id.to_string()),
        Some(part.id.clone()),
        Some(part.id.clone()),
        tokens,
        native.clone(),
        timestamp(part.time_created),
      ) {
        Some(event) => latest_step_usage = Some(event),
        None if report_invalid_step_usage => malformed_usage.push(unknown_event(
          self.provider,
          Some(self.session_id.clone()),
          Some("usage".to_string()),
          Some(native),
          timestamp(part.time_created),
        )),
        None => {}
      }
    }

    // Live V1 rows are mutable: text can exist before the assistant message
    // finishes. Preserve its phase so it cannot count as an unread final yet.
    // Older/exported rows without timing retain their historical behavior.
    let message_phase = if message_native
      .pointer("/time/created")
      .and_then(Value::as_u64)
      .is_some()
      && message_native
        .pointer("/time/completed")
        .and_then(Value::as_u64)
        .is_none()
      && message_native.get("finish").and_then(Value::as_str).is_none()
      && message_native.get("error").is_none_or(Value::is_null)
    {
      Phase::Delta
    } else {
      Phase::Finished
    };
    let mut events = Vec::new();
    for part in parts {
      events.extend(self.normalize_assistant_part(message_id, parent_id, &provenance, message_phase, part));
    }
    events.extend(malformed_usage);
    if let Some(usage) = latest_step_usage.or(fallback_usage) {
      events.push(usage);
    }
    events
  }
}

fn is_image_file(file: &tokn_opencode_protocol::v1::FilePart) -> bool {
  file.mime.as_deref().is_some_and(|mime| mime.starts_with("image/"))
    || file.url.as_deref().is_some_and(|url| url.starts_with("data:image/"))
}

fn recorded_file_image(file: tokn_opencode_protocol::v1::FilePart, part_id: String) -> MessageImage {
  let image_id = file.identity.id.unwrap_or(part_id);
  if let Some(url) = file.url {
    return MessageImage::from_url(image_id, url, file.mime, file.filename);
  }
  let path = file
    .source
    .as_ref()
    .filter(|source| source["type"] == "file")
    .and_then(|source| source.get("path"))
    .and_then(Value::as_str);
  MessageImage {
    image_id,
    mime_type: file.mime,
    name: file.filename,
    source: path
      .map(|path| MessageImageSource::LocalFile { path: path.into() })
      .unwrap_or(MessageImageSource::Unavailable { reference: None }),
  }
}

/// Normalize OpenCode's per-model-call token counters. OpenCode separates
/// cache reads/writes from the base input count, while the shared IR requires
/// `input_tokens` to include those cache tokens exactly once.
pub(crate) fn usage_event(
  session_id: Option<String>,
  message_id: Option<String>,
  step_id: Option<String>,
  record_id: Option<String>,
  tokens: &TokenUsage,
  native: Value,
  timestamp: Option<String>,
) -> Option<AgentEvent> {
  usage_event_for_provider(
    Provider::OpenCode,
    session_id,
    message_id,
    step_id,
    record_id,
    tokens,
    native,
    timestamp,
  )
}

fn usage_event_for_provider(
  provider: Provider,
  session_id: Option<String>,
  message_id: Option<String>,
  step_id: Option<String>,
  record_id: Option<String>,
  tokens: &TokenUsage,
  native: Value,
  timestamp: Option<String>,
) -> Option<AgentEvent> {
  let input = token_counter(tokens.input)?;
  let output = token_counter(tokens.output)?;
  let cache_read = optional_token_counter(tokens.cache.as_ref().and_then(|cache| cache.read))?;
  let cache_write = optional_token_counter(tokens.cache.as_ref().and_then(|cache| cache.write))?;
  let total = optional_token_counter(tokens.total)?;
  let reasoning = optional_token_counter(tokens.reasoning)?;
  let input_tokens = input
    .checked_add(cache_read.unwrap_or(0))?
    .checked_add(cache_write.unwrap_or(0))?;

  Some(AgentEvent::Usage(UsageEvent {
    kind: UsageKind::ModelCall,
    provider,
    session_id,
    turn_id: None,
    step_id,
    message_id,
    record_id,
    input_tokens,
    output_tokens: output,
    // Do not synthesize a total. OpenCode's reported total can include a
    // distinct reasoning count, and is authoritative when it is present.
    total_tokens: total,
    cache_read_tokens: cache_read,
    cache_write_tokens: cache_write,
    reasoning_tokens: reasoning,
    native,
    timestamp,
  }))
}

fn usage_native(native: &Value) -> Value {
  native.get("tokens").cloned().unwrap_or(Value::Null)
}

/// A malformed non-accounting field turns the tolerant wire item into an
/// `Unknown`, but valid token objects can still be normalized alongside the
/// retained unknown record.
fn recover_token_usage(native: &Value) -> Option<TokenUsage> {
  native
    .get("tokens")
    .filter(|tokens| !tokens.is_null())
    .and_then(|tokens| serde_json::from_value(tokens.clone()).ok())
}

fn token_counter(value: Option<f64>) -> Option<u64> {
  let value = value?;
  (value.is_finite() && value >= 0.0 && value.fract() == 0.0 && value < u64::MAX as f64).then_some(value as u64)
}

fn optional_token_counter(value: Option<f64>) -> Option<Option<u64>> {
  match value {
    Some(value) => token_counter(Some(value)).map(Some),
    None => Some(None),
  }
}

pub(crate) fn tool_event(
  provider: Provider,
  session_id: String,
  message_id: String,
  parent_id: Option<String>,
  call_id: Option<String>,
  tool: Option<String>,
  state: ToolState,
  time_created: Option<i64>,
) -> AgentEvent {
  let (state, native) = state.into_parts();
  let images = match &state {
    ToolStateItem::Completed(state) => attachment_images(state.attachments.as_deref()),
    _ => Vec::new(),
  };
  let (record_kind, phase, input, output, is_error) = match state {
    ToolStateItem::Pending(state) => {
      let input = match (state.input, state.raw) {
        (Some(input), Some(raw)) => Some(json!({ "input": input, "raw": raw })),
        (Some(input), None) => Some(input),
        (None, Some(raw)) => Some(json!({ "raw": raw })),
        (None, None) => None,
      };
      (ToolRecordKind::Invocation, Phase::Started, input, None, None)
    }
    ToolStateItem::Running(state) => (
      ToolRecordKind::Progress,
      Phase::Updated,
      state.input,
      present_output(state.title, None, state.metadata, None),
      Some(false),
    ),
    ToolStateItem::Completed(state) => {
      let is_error = metadata_exit_is_error(state.metadata.as_ref());
      (
        ToolRecordKind::Snapshot,
        Phase::Finished,
        state.input,
        present_output(state.title, state.output, state.metadata, None),
        Some(is_error),
      )
    }
    ToolStateItem::Error(state) => (
      ToolRecordKind::Snapshot,
      Phase::Finished,
      state.input,
      present_output(None, state.error.map(Value::String), state.metadata, state.raw),
      Some(true),
    ),
    ToolStateItem::Unknown(_) if native["status"] == "error" => (
      ToolRecordKind::Snapshot,
      Phase::Finished,
      native.get("input").cloned(),
      Some(native.clone()),
      Some(true),
    ),
    ToolStateItem::Unknown(_) => (
      ToolRecordKind::Snapshot,
      Phase::Updated,
      None,
      Some(native.clone()),
      None,
    ),
  };

  AgentEvent::ToolCall(ToolCallEvent {
    images,
    provider,
    session_id: Some(session_id),
    turn_id: None,
    message_id: Some(message_id),
    parent_id,
    record_kind,
    tool_call_id: call_id,
    provider_tool_name: tool.clone(),
    tool_kind: tool_kind_for_optional_name(tool.as_deref()),
    summary: tool_summary_for_io(tool.as_deref(), input.as_ref(), output.as_ref()),
    tool_name: tool,
    transport: Some(ToolTransport::Native),
    phase,
    input,
    output,
    is_error,
    native: Some(native),
    timestamp: timestamp(time_created),
  })
}

fn attachment_images(attachments: Option<&[Value]>) -> Vec<tokn_session_core::RecordedImage> {
  attachments
    .into_iter()
    .flatten()
    .enumerate()
    .filter_map(|(index, value)| {
      // Completed-tool attachments use the same file-part wire shape as
      // message files. Keep unsupported attachments in ToolCallEvent.native.
      if value.get("type").and_then(Value::as_str) != Some("file") {
        return None;
      }
      let image_id = format!("image.v1.{index}");
      match serde_json::from_value::<tokn_opencode_protocol::v1::FilePart>(value.clone()) {
        Ok(file) if is_image_file(&file) => Some(recorded_file_image(file, image_id)),
        Err(_)
          if value
            .get("mime")
            .and_then(Value::as_str)
            .is_some_and(|mime| mime.starts_with("image/")) =>
        {
          Some(tokn_session_core::RecordedImage {
            image_id: value.get("id").and_then(Value::as_str).unwrap_or(&image_id).to_owned(),
            mime_type: value.get("mime").and_then(Value::as_str).map(str::to_owned),
            name: value.get("filename").and_then(Value::as_str).map(str::to_owned),
            source: tokn_session_core::RecordedImageSource::Unavailable { reference: None },
          })
        }
        _ => None,
      }
    })
    .collect()
}

fn present_output(
  title: Option<String>,
  output: Option<Value>,
  metadata: Option<Value>,
  raw: Option<String>,
) -> Option<Value> {
  if title.is_none() && output.is_none() && metadata.is_none() && raw.is_none() {
    return None;
  }

  let mut value = json!({
    "title": title,
    "output": output,
    "metadata": metadata,
  });
  if let (Some(object), Some(raw)) = (value.as_object_mut(), raw) {
    object.insert("raw".to_string(), Value::String(raw));
  }
  Some(value)
}

fn metadata_exit_is_error(metadata: Option<&Value>) -> bool {
  metadata
    .and_then(|metadata| metadata.get("exit"))
    .and_then(Value::as_i64)
    .is_some_and(|exit| exit != 0)
}

pub(crate) fn timestamp(value: Option<i64>) -> Option<String> {
  value.map(|value| value.to_string())
}

fn model_from_native(value: &Value) -> (Option<String>, Option<String>) {
  let direct_provider = string_field(value, "providerID");
  let direct_model = string_field(value, "modelID");
  let model = value.get("model");
  let nested_provider = model.and_then(|model| string_field(model, "providerID"));
  let nested_model = model.and_then(|model| string_field(model, "modelID").or_else(|| string_field(model, "id")));
  (direct_provider.or(nested_provider), direct_model.or(nested_model))
}

fn message_provenance(provider: Provider, native: &Value) -> Option<MessageProvenance> {
  if !matches!(provider, Provider::ZCode) {
    return None;
  }

  let semantics = native.get("semantics");
  let transcript_visibility = semantics
    .and_then(|value| value.get("transcriptVisibility"))
    .and_then(Value::as_str);
  let ui_visibility = semantics
    .and_then(|value| value.get("uiVisibility"))
    .and_then(Value::as_str);
  let display = (transcript_visibility == Some("hidden") || ui_visibility == Some("hidden")).then_some(false);
  let source = semantics
    .and_then(|value| value.get("source"))
    .or_else(|| native.get("source"))
    .cloned()
    .unwrap_or_else(|| Value::String("zcode".to_string()));

  Some(MessageProvenance {
    source,
    display,
    native: semantics.cloned(),
    surface_op: None,
    source_event_seqs: None,
  })
}

fn string_field(value: &Value, field: &str) -> Option<String> {
  value.get(field).and_then(Value::as_str).map(str::to_string)
}

fn unknown_event(
  provider: Provider,
  session_id: Option<String>,
  native_type: Option<String>,
  native: Option<Value>,
  timestamp: Option<String>,
) -> AgentEvent {
  AgentEvent::Unknown(UnknownEvent {
    provider,
    session_id,
    native_type,
    native,
    timestamp,
  })
}

#[cfg(test)]
mod tests {
  use serde_json::{Value, json};
  use tokn_opencode_protocol::v1::{MessageData, PartData, TokenUsage};
  use tokn_session_core::{AgentEvent, MessageDelivery, Phase, Role, UsageKind};

  use super::OpenCodeNormalizer;
  use crate::row::{OpenCodeMessageRow, OpenCodePartRow};

  #[test]
  fn hidden_zcode_models_do_not_change_the_visible_session_model() {
    let mut normalizer = OpenCodeNormalizer::with_provider("session".into(), tokn_session_core::Provider::ZCode);
    let visible = normalizer.normalize_message(message_row(
      "visible",
      json!({
        "role":"user", "model":{"providerID":"first","modelID":"same"}
      }),
      Vec::new(),
    ));
    assert!(matches!(&visible[0], AgentEvent::ProviderChanged(event)
      if event.model_origin == Some(tokn_session_core::ModelOrigin::Request)));
    let hidden = normalizer.normalize_message(message_row(
      "hidden",
      json!({
        "role":"assistant", "providerID":"internal", "modelID":"auxiliary",
        "semantics":{"uiVisibility":"hidden"}
      }),
      vec![part_row("hidden-text", json!({"type":"text","text":"internal"}))],
    ));
    assert!(
      !hidden
        .iter()
        .any(|event| matches!(event, AgentEvent::ProviderChanged(_)))
    );
    assert!(hidden.iter().all(AgentEvent::is_hidden));
    let unchanged = normalizer.normalize_message(message_row(
      "unchanged",
      json!({
        "role":"assistant", "providerID":"first", "modelID":"same"
      }),
      Vec::new(),
    ));
    assert!(
      !unchanged
        .iter()
        .any(|event| matches!(event, AgentEvent::ProviderChanged(_)))
    );
    let changed = normalizer.normalize_message(message_row(
      "changed",
      json!({
        "role":"assistant", "providerID":"second", "modelID":"same"
      }),
      Vec::new(),
    ));
    assert!(matches!(&changed[0], AgentEvent::ProviderChanged(event)
      if event.model_provider.as_deref() == Some("second")
        && event.model_origin == Some(tokn_session_core::ModelOrigin::Response)));
  }

  #[test]
  fn opencode_and_zcode_completed_tools_retain_typed_image_attachments_and_native_unknowns() {
    for provider in [
      tokn_session_core::Provider::OpenCode,
      tokn_session_core::Provider::ZCode,
    ] {
      let attachments = json!([
        {"type":"file", "id":"inline", "mime":"image/png", "filename":"render.png", "url":"data:image/png;base64,YQ=="},
        {"type":"file", "mime":"application/pdf", "url":"file:///tmp/document.pdf"},
        {"type":"file", "mime":"image/png", "url":"file:///tmp/local%20image.png"},
        {"type":"file", "mime":"image/png", "url":"https://example.com/remote.png"},
        {"type":"file", "mime":"image/png", "url":123},
        {"type":"future_attachment", "opaque":true}
      ]);
      let mut normalizer = OpenCodeNormalizer::with_provider("session".into(), provider);
      let events = normalizer.normalize_message(message_row(
        "assistant",
        json!({"role":"assistant"}),
        vec![part_row(
          "tool",
          json!({
            "type":"tool", "callID":"call", "tool":"render", "state":{
              "status":"completed", "input":{}, "output":"rendered", "metadata":{"exit":1}, "attachments":attachments
            }
          }),
        )],
      ));
      let [AgentEvent::ToolCall(tool)] = &events[..] else {
        panic!("expected a tool result, not assistant messages")
      };
      assert_eq!(tool.provider, provider);
      assert_eq!(tool.tool_call_id.as_deref(), Some("call"));
      assert_eq!(
        tool
          .images
          .iter()
          .map(|image| image.image_id.as_str())
          .collect::<Vec<_>>(),
        ["inline", "image.v1.2", "image.v1.3", "image.v1.4"]
      );
      assert_eq!(tool.images[0].name.as_deref(), Some("render.png"));
      assert!(
        matches!(&tool.images[1].source, tokn_session_core::RecordedImageSource::LocalFile { path } if path.to_str() == Some("/tmp/local image.png"))
      );
      assert!(
        tool.images[2..]
          .iter()
          .all(|image| matches!(image.source, tokn_session_core::RecordedImageSource::Unavailable { .. }))
      );
      assert_eq!(tool.is_error, Some(true));
      assert_eq!(tool.output.as_ref().unwrap()["output"], "rendered");
      assert_eq!(tool.native.as_ref().unwrap()["attachments"], attachments);
    }
  }

  #[test]
  fn opencode_and_zcode_image_files_are_messages_and_opaque_references_stay_unavailable() {
    for provider in [
      tokn_session_core::Provider::OpenCode,
      tokn_session_core::Provider::ZCode,
    ] {
      let mut normalizer = OpenCodeNormalizer::with_provider("session".into(), provider);
      let events = normalizer.normalize_message(message_row(
        "user",
        json!({"role":"user"}),
        vec![
          part_row(
            "inline",
            json!({"type":"file", "mime":"image/png", "url":"data:image/png;base64,YQ==", "filename":"picture.png"}),
          ),
          part_row(
            "local",
            json!({"type":"file", "mime":"image/png", "url":"file:///tmp/a%20b.png"}),
          ),
          part_row(
            "remote",
            json!({"type":"file", "mime":"image/png", "url":"https://example.com/image.png"}),
          ),
          part_row(
            "document",
            json!({"type":"file", "mime":"application/pdf", "url":"file:///tmp/file.pdf"}),
          ),
        ],
      ));
      let [AgentEvent::Message(message), AgentEvent::Unknown(document)] = &events[..] else {
        panic!("unexpected file projection")
      };
      assert_eq!(message.provider, provider);
      assert!(message.text.is_empty());
      assert_eq!(
        message
          .images
          .iter()
          .map(|image| image.image_id.as_str())
          .collect::<Vec<_>>(),
        ["inline", "local", "remote"]
      );
      assert_eq!(message.images[0].name.as_deref(), Some("picture.png"));
      assert!(
        matches!(&message.images[1].source, tokn_session_core::MessageImageSource::LocalFile { path } if path.to_str() == Some("/tmp/a b.png"))
      );
      assert!(matches!(
        &message.images[2].source,
        tokn_session_core::MessageImageSource::Unavailable { .. }
      ));
      assert_eq!(document.native_type.as_deref(), Some("part.file"));
      let events = normalizer.normalize_message(message_row(
        "assistant",
        json!({"role":"assistant"}),
        vec![part_row(
          "image",
          json!({"type":"file", "mime":"image/png", "source":{"type":"file", "path":"relative.png"}}),
        )],
      ));
      assert!(
        matches!(&events[..], [AgentEvent::Message(message)] if message.role == Role::Assistant && matches!(&message.images[0].source, tokn_session_core::MessageImageSource::LocalFile { path } if path.to_str() == Some("relative.png")))
      );
    }
  }

  #[test]
  fn assistant_text_is_not_finalized_until_the_mutable_message_completes() {
    for provider in [
      tokn_session_core::Provider::OpenCode,
      tokn_session_core::Provider::ZCode,
    ] {
      let mut normalizer = OpenCodeNormalizer::with_provider("session".into(), provider);
      for (time, expected) in [
        (json!({"created": 100}), Phase::Delta),
        (json!({"created": 100, "completed": 200}), Phase::Finished),
      ] {
        let events = normalizer.normalize_message(message_row(
          "reply",
          json!({"role":"assistant","time":time}),
          vec![part_row("text", json!({"type":"text","text":"answer"}))],
        ));
        assert!(
          events
            .iter()
            .any(|event| matches!(event, AgentEvent::Message(message) if message.phase == expected))
        );
      }
    }
  }

  #[test]
  fn normalizes_known_persisted_messages_and_parts() {
    let mut normalizer = OpenCodeNormalizer::new("ses_1".to_string());
    let user = message_row(
      "msg_user",
      json!({
        "role": "user",
        "model": {
          "providerID": "openai",
          "modelID": "gpt-5"
        }
      }),
      vec![part_row("prt_user", json!({"type": "text", "text": "hello"}))],
    );
    let user_events = normalizer.normalize_message(user);

    assert!(matches!(&user_events[0], AgentEvent::ProviderChanged(event)
      if event.model_provider.as_deref() == Some("openai")
        && event.model_id.as_deref() == Some("gpt-5")));
    assert!(matches!(&user_events[1], AgentEvent::Message(event)
      if matches!(event.role, Role::User) && event.text == "hello"));

    let assistant = message_row(
      "msg_assistant",
      json!({
        "role": "assistant",
        "parentID": "msg_user",
        "providerID": "openai",
        "modelID": "gpt-5"
      }),
      vec![
        part_row("prt_reasoning", json!({"type": "reasoning", "text": "checking"})),
        part_row("prt_text", json!({"type": "text", "text": "done"})),
        part_row(
          "prt_tool",
          json!({
            "type": "tool",
            "callID": "call_1",
            "tool": "bash",
            "state": {
              "status": "completed",
              "input": {"command": "false"},
              "output": "",
              "metadata": {"exit": 1}
            }
          }),
        ),
      ],
    );
    let assistant_events = normalizer.normalize_message(assistant);

    assert!(matches!(&assistant_events[0], AgentEvent::Reasoning(event)
      if event.text.as_deref() == Some("checking")));
    assert!(matches!(&assistant_events[1], AgentEvent::Message(event)
      if matches!(event.role, Role::Assistant)
        && matches!(event.delivery, MessageDelivery::Final)
        && event.text == "done"));
    assert!(matches!(&assistant_events[2], AgentEvent::ToolCall(event)
      if event.tool_call_id.as_deref() == Some("call_1")
        && matches!(event.phase, Phase::Finished)
        && event.is_error == Some(true)));
  }

  #[test]
  fn preserves_unknown_and_malformed_native_payloads() {
    let mut normalizer = OpenCodeNormalizer::new("ses_1".to_string());
    let unknown = message_row(
      "msg_unknown",
      json!({
        "role": "future-role",
        "payload": {"answer": 42}
      }),
      Vec::new(),
    );
    let events = normalizer.normalize_message(unknown);
    assert!(matches!(&events[0], AgentEvent::Unknown(event)
      if event.native_type.as_deref() == Some("message.role.future-role")
        && event.native.as_ref().and_then(|native| native.pointer("/payload/answer")) == Some(&json!(42))));

    let malformed_part = message_row(
      "msg_assistant",
      json!({"role": "assistant"}),
      vec![part_row("prt_bad", json!({"type": "text", "text": 42}))],
    );
    let events = normalizer.normalize_message(malformed_part);
    assert!(matches!(&events[0], AgentEvent::Unknown(event)
      if event.native_type.as_deref() == Some("part.text")
        && event.native.as_ref().and_then(|native| native.get("text")) == Some(&json!(42))));

    let user_with_future_part = message_row(
      "msg_user_future",
      json!({"role": "user"}),
      vec![
        part_row("prt_text", json!({"type": "text", "text": "visible"})),
        part_row("prt_future", json!({"type": "future-part", "answer": 42})),
      ],
    );
    let events = normalizer.normalize_message(user_with_future_part);
    assert!(matches!(&events[0], AgentEvent::Message(event)
      if matches!(event.role, Role::User) && event.text == "visible"));
    assert!(matches!(&events[1], AgentEvent::Unknown(event)
      if event.native_type.as_deref() == Some("part.future-part")
        && event.native.as_ref().and_then(|native| native.get("answer")) == Some(&json!(42))));

    let malformed_assistant = message_row(
      "msg_malformed_assistant",
      json!({
        "role": "assistant",
        "providerID": 42,
        "error": null
      }),
      vec![part_row(
        "prt_recoverable",
        json!({"type": "text", "text": "still visible"}),
      )],
    );
    let events = normalizer.normalize_message(malformed_assistant);
    assert!(matches!(&events[0], AgentEvent::Message(event)
      if matches!(event.role, Role::Assistant) && event.text == "still visible"));
    assert!(matches!(&events[1], AgentEvent::Unknown(event)
      if event.native_type.as_deref() == Some("message.role.assistant")
        && event.native.as_ref().and_then(|native| native.get("providerID")) == Some(&json!(42))));
    assert_eq!(events.len(), 2);
  }

  #[test]
  fn keeps_historical_system_messages_and_error_state_detail() {
    let mut normalizer = OpenCodeNormalizer::new("ses_1".to_string());
    let system = message_row(
      "msg_system",
      json!({"role": "system"}),
      vec![part_row("prt_system", json!({"type": "text", "text": "context"}))],
    );
    let events = normalizer.normalize_message(system);
    assert!(matches!(&events[0], AgentEvent::Message(event)
      if matches!(event.role, Role::System) && event.text == "context"));

    let tool = message_row(
      "msg_assistant",
      json!({"role": "assistant"}),
      vec![part_row(
        "prt_tool_fallback",
        json!({
          "type": "tool",
          "tool": "bash",
          "state": {
            "status": "error",
            "input": {"command": "cargo test"},
            "raw": "{\"command\":\"cargo test\"}",
            "error": "failed"
          }
        }),
      )],
    );
    let events = normalizer.normalize_message(tool);
    let AgentEvent::ToolCall(event) = &events[0] else {
      panic!("expected tool event");
    };
    assert_eq!(event.tool_call_id.as_deref(), Some("prt_tool_fallback"));
    assert_eq!(
      event.output.as_ref().and_then(|output| output.get("raw")),
      Some(&Value::String("{\"command\":\"cargo test\"}".to_string()))
    );
  }

  #[test]
  fn usage_prefers_the_last_valid_step_finish_over_assistant_fallback() {
    let mut normalizer = OpenCodeNormalizer::new("ses_1".to_string());
    let fallback = json!({
      "input": 1,
      "output": 2,
      "reasoning": 3,
      "cache": {"read": 4, "write": 5},
      "total": 15
    });
    let final_step = json!({
      "input": 10,
      "output": 11,
      "reasoning": 12,
      "cache": {"read": 13, "write": 14},
      // Keep a provider-reported total even when it differs from the derived
      // counters: it can include provider-specific accounting.
      "total": 999
    });
    let events = normalizer.normalize_message(message_row(
      "msg_assistant",
      json!({
        "role": "assistant",
        "tokens": fallback.clone(),
      }),
      vec![
        part_row(
          "prt_first",
          json!({
            "type": "step-finish",
            "tokens": {
              "input": 6,
              "output": 7,
              "reasoning": 8,
              "cache": {"read": 9, "write": 10},
              "total": 40
            }
          }),
        ),
        part_row("prt_text", json!({"type": "text", "text": "still visible"})),
        part_row(
          "prt_final",
          json!({
            "type": "step-finish",
            "tokens": final_step.clone(),
          }),
        ),
      ],
    ));

    let usage: Vec<_> = events
      .iter()
      .filter_map(|event| match event {
        AgentEvent::Usage(usage) => Some(usage),
        _ => None,
      })
      .collect();
    assert_eq!(usage.len(), 1);
    let usage = usage[0];
    assert_eq!(usage.kind, UsageKind::ModelCall);
    assert_eq!(usage.message_id.as_deref(), Some("msg_assistant"));
    assert_eq!(usage.step_id.as_deref(), Some("prt_final"));
    assert_eq!(usage.record_id.as_deref(), Some("prt_final"));
    assert_eq!(usage.input_tokens, 37);
    assert_eq!(usage.output_tokens, 11);
    assert_eq!(usage.cache_read_tokens, Some(13));
    assert_eq!(usage.cache_write_tokens, Some(14));
    assert_eq!(usage.reasoning_tokens, Some(12));
    assert_eq!(usage.total_tokens, Some(999));
    assert_eq!(usage.native, final_step);
    assert!(
      events
        .iter()
        .any(|event| { matches!(event, AgentEvent::Message(message) if message.text == "still visible") })
    );
  }

  #[test]
  fn usage_falls_back_to_assistant_message_tokens() {
    let mut normalizer = OpenCodeNormalizer::new("ses_1".to_string());
    let fallback = json!({
      "input": 21,
      "output": 22,
      "reasoning": 23,
      "cache": {"read": 24, "write": 25},
      "total": 26
    });
    let events = normalizer.normalize_message(message_row(
      "msg_assistant",
      json!({"role": "assistant", "tokens": fallback.clone()}),
      vec![part_row("prt_step", json!({"type": "step-finish"}))],
    ));

    let usage: Vec<_> = events
      .iter()
      .filter_map(|event| match event {
        AgentEvent::Usage(usage) => Some(usage),
        _ => None,
      })
      .collect();
    assert_eq!(usage.len(), 1);
    let usage = usage[0];
    assert_eq!(usage.message_id.as_deref(), Some("msg_assistant"));
    assert_eq!(usage.step_id, None);
    assert_eq!(usage.record_id.as_deref(), Some("msg_assistant"));
    assert_eq!(usage.input_tokens, 70);
    assert_eq!(usage.output_tokens, 22);
    assert_eq!(usage.total_tokens, Some(26));
    assert_eq!(usage.native, fallback);
  }

  #[test]
  fn usage_recovers_from_an_unknown_assistant_with_valid_tokens() {
    let mut normalizer = OpenCodeNormalizer::new("ses_1".to_string());
    let events = normalizer.normalize_message(message_row(
      "msg_assistant",
      json!({
        "role": "assistant",
        // This unrelated malformed field makes the wire message unknown.
        "providerID": 42,
        "tokens": {
          "input": 2,
          "output": 3,
          "reasoning": 4,
          "cache": {"read": 5, "write": 6}
        }
      }),
      Vec::new(),
    ));

    let usage: Vec<_> = events
      .iter()
      .filter_map(|event| match event {
        AgentEvent::Usage(usage) => Some(usage),
        _ => None,
      })
      .collect();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].input_tokens, 13);
    assert_eq!(usage[0].output_tokens, 3);
    assert_eq!(usage[0].reasoning_tokens, Some(4));
    assert!(events.iter().any(|event| {
      matches!(event, AgentEvent::Unknown(event) if event.native_type.as_deref() == Some("message.role.assistant"))
    }));
  }

  #[test]
  fn malformed_usage_stays_visible_without_hiding_assistant_content() {
    let mut normalizer = OpenCodeNormalizer::new("ses_1".to_string());
    let malformed = json!({
      "input": 1.5,
      "output": 2,
      "reasoning": 0,
      "cache": {"read": 0, "write": 0}
    });
    let events = normalizer.normalize_message(message_row(
      "msg_assistant",
      json!({"role": "assistant"}),
      vec![
        part_row("prt_text", json!({"type": "text", "text": "answer remains visible"})),
        part_row("prt_step", json!({"type": "step-finish", "tokens": malformed.clone()})),
      ],
    ));

    assert!(
      events
        .iter()
        .any(|event| { matches!(event, AgentEvent::Message(message) if message.text == "answer remains visible") })
    );
    assert!(!events.iter().any(|event| matches!(event, AgentEvent::Usage(_))));
    assert!(events.iter().any(|event| {
      matches!(event, AgentEvent::Unknown(event)
        if event.native_type.as_deref() == Some("usage")
          && event.native.as_ref() == Some(&malformed))
    }));
  }

  #[test]
  fn usage_keeps_the_last_valid_step_when_a_later_step_is_malformed() {
    let mut normalizer = OpenCodeNormalizer::new("ses_1".to_string());
    let events = normalizer.normalize_message(message_row(
      "msg_assistant",
      json!({
        "role": "assistant",
        "tokens": {
          "input": 100,
          "output": 100,
          "reasoning": 0,
          "cache": {"read": 0, "write": 0}
        }
      }),
      vec![
        part_row(
          "prt_valid",
          json!({
            "type": "step-finish",
            "tokens": {
              "input": 2,
              "output": 3,
              "reasoning": 0,
              "cache": {"read": 4, "write": 5}
            }
          }),
        ),
        part_row(
          "prt_invalid",
          json!({
            "type": "step-finish",
            "tokens": {
              "input": -1,
              "output": 3,
              "reasoning": 0,
              "cache": {"read": 0, "write": 0}
            }
          }),
        ),
      ],
    ));

    let usage: Vec<_> = events
      .iter()
      .filter_map(|event| match event {
        AgentEvent::Usage(usage) => Some(usage),
        _ => None,
      })
      .collect();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].step_id.as_deref(), Some("prt_valid"));
    assert_eq!(usage[0].input_tokens, 11);
    assert!(
      events
        .iter()
        .any(|event| { matches!(event, AgentEvent::Unknown(event) if event.native_type.as_deref() == Some("usage")) })
    );
  }

  #[test]
  fn usage_rejects_non_finite_negative_fractional_and_out_of_range_counters() {
    for input in [f64::NAN, f64::INFINITY, -1.0, 1.5, u64::MAX as f64] {
      let tokens = TokenUsage {
        input: Some(input),
        output: Some(1.0),
        ..Default::default()
      };
      assert!(
        super::usage_event(
          Some("ses_1".to_string()),
          Some("msg_1".to_string()),
          None,
          Some("msg_1".to_string()),
          &tokens,
          Value::Null,
          Some("1".to_string()),
        )
        .is_none()
      );
    }
  }

  fn message_row(id: &str, data: Value, parts: Vec<OpenCodePartRow>) -> OpenCodeMessageRow {
    OpenCodeMessageRow {
      id: id.to_string(),
      time_created: Some(1),
      data: serde_json::from_value::<MessageData>(data).expect("message should decode"),
      parts,
    }
  }

  fn part_row(id: &str, data: Value) -> OpenCodePartRow {
    OpenCodePartRow {
      id: id.to_string(),
      time_created: Some(2),
      data: serde_json::from_value::<PartData>(data).expect("part should decode"),
    }
  }
}
