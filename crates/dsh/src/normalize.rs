use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};
use tokn_dsh_protocol::{
  ContentBlock, DshSessionItem, DshSessionLine, SessionEvent, SessionHeader, StreamChunk, SurfaceOp, TokenUsage,
};
use tokn_session_core::{
  AgentEvent, ErrorEvent, LifecycleEvent, LifecycleOutcome, LifecycleScope, MessageDelivery, MessageEvent,
  MessageProvenance, MetadataEvent, MetadataKind, Phase, Provider, ProviderChanged, ReasoningEvent, Role,
  SessionStarted, ToolCallEvent, ToolRecordKind, ToolTransport, UnknownEvent, UsageEvent, UsageKind,
  tool_kind_for_optional_name, tool_summary_for_io,
};

mod turn_end;

/// Historical log view, not a reconstruction of the compacted model surface.
/// Assembled messages replace their redundant stream deltas for display, while
/// unfinished steps still expose their partial text and reasoning.
pub(crate) fn normalize(header: &SessionHeader, lines: Vec<DshSessionLine>) -> Vec<AgentEvent> {
  normalize_batches(header, lines).into_iter().flatten().collect()
}

pub(crate) fn normalize_batches(header: &SessionHeader, lines: Vec<DshSessionLine>) -> Vec<Vec<AgentEvent>> {
  let mut state = Normalizer {
    session_id: header.id.clone(),
    assembled: HashSet::new(),
    usage_records: HashMap::new(),
    calls: HashMap::new(),
    emitted_calls: HashSet::new(),
  };
  // Stream usage is a per-call snapshot, not an additive delta. Keep the last
  // snapshot unless an assembled message supplies the authoritative usage.
  for line in &lines {
    if !valid_surface(line.native()) {
      continue;
    }
    if let DshSessionItem::Event(SessionEvent::AssistantChunk(event)) = line.item()
      && matches!(event.data.chunk, StreamChunk::Usage(_))
    {
      state
        .usage_records
        .insert((event.data.turn, event.data.step), event.seq);
    }
  }
  for line in &lines {
    if !valid_surface(line.native()) {
      continue;
    }
    match line.item() {
      DshSessionItem::Event(SessionEvent::AssistantMessage(event)) => {
        state.assembled.insert((event.data.turn, event.data.step));
        if event.data.usage.is_some() {
          state
            .usage_records
            .insert((event.data.turn, event.data.step), event.seq);
        }
      }
      DshSessionItem::Event(SessionEvent::ToolCall(event)) => {
        state.calls.insert(
          (event.data.turn, event.data.step, event.data.call_id.clone()),
          (event.data.name.clone(), arguments(&event.data.arguments)),
        );
      }
      _ => {}
    }
  }
  let mut events = vec![vec![AgentEvent::SessionStarted(SessionStarted {
    provider: Provider::Dsh,
    session_id: header.id.clone(),
    cwd: header.cwd.clone(),
    timestamp: Some(header.created_at.to_string()),
  })]];
  for line in lines {
    events.push(state.line(line));
  }
  events
}

type CallKey = (u64, u64, String);

struct Normalizer {
  session_id: String,
  assembled: HashSet<(u64, u64)>,
  usage_records: HashMap<(u64, u64), u64>,
  calls: HashMap<CallKey, (String, Value)>,
  emitted_calls: HashSet<CallKey>,
}

impl Normalizer {
  fn line(&mut self, line: DshSessionLine) -> Vec<AgentEvent> {
    let time = line
      .native()
      .get("time")
      .and_then(Value::as_i64)
      .map(|time| time.to_string());
    let (item, native) = line.into_parts();
    if !valid_surface(&native) {
      return vec![self.unknown(native, time)];
    }
    if let Some(events) = super::compaction::normalize(&self.session_id, &native) {
      return events.unwrap_or_else(|| vec![self.unknown(native.clone(), time.clone())]);
    }
    let DshSessionItem::Event(event) = item else {
      if let Some((kind, summary)) = super::metadata::classify(&native) {
        return vec![self.metadata(kind, summary, native, time)];
      }
      return vec![self.unknown(native, time)];
    };
    match event {
      SessionEvent::RequestHeader(event) => vec![AgentEvent::ProviderChanged(ProviderChanged {
        provider: Provider::Dsh,
        session_id: Some(self.session_id.clone()),
        native_id: Some(event.seq.to_string()),
        native_parent_id: None,
        model_provider: Some(event.data.header.config.provider),
        model_id: Some(event.data.header.config.model),
        thinking_level: event.data.header.config.reasoning_effort,
        timestamp: time,
      })],
      SessionEvent::UserMessage(event) => {
        self.message(&event.data.id, &event.data.role, event.data.content, None, native, time)
      }
      SessionEvent::AssistantMessage(event) => {
        let step = (event.data.turn, event.data.step);
        let usage = event
          .data
          .usage
          .filter(|_| self.usage_records.get(&step) == Some(&event.seq))
          .map(|usage| {
            self.usage(
              step,
              Some(event.data.message.id.clone()),
              usage,
              native.clone(),
              time.clone(),
            )
          });
        let mut events = self.message(
          &event.data.message.id,
          &event.data.message.role,
          event.data.message.content,
          Some((event.data.turn, event.data.step)),
          native,
          time.clone(),
        );
        if let Some(usage) = usage {
          events.push(usage);
        }
        events
      }
      SessionEvent::ToolCall(event) => {
        let key = (event.data.turn, event.data.step, event.data.call_id.clone());
        if !self.emitted_calls.insert(key) {
          return vec![];
        }
        vec![self.tool(
          Some(event.data.turn),
          Some(event.data.call_id),
          Some(event.data.name),
          None,
          Some(arguments(&event.data.arguments)),
          None,
          None,
          Phase::Started,
          time,
        )]
      }
      SessionEvent::ToolResult(event) => {
        let mut events = Vec::new();
        for block in event.data.message.content {
          let ContentBlock::ToolResult(block) = block else {
            events.push(self.unknown(native.clone(), time.clone()));
            continue;
          };
          let call = self
            .calls
            .get(&(event.data.turn, event.data.step, block.tool_call_id.clone()));
          let output = json!({
            "content": native["data"]["message"]["content"],
            "meta": native["data"]["meta"],
            "error": native["data"]["error"]
          });
          events.push(self.tool(
            Some(event.data.turn),
            Some(block.tool_call_id),
            call.map(|(name, _)| name.clone()),
            Some(event.data.message.id.clone()),
            call.map(|(_, input)| input.clone()),
            Some(output),
            Some(block.is_error.unwrap_or(false) || event.data.error.is_some()),
            Phase::Finished,
            time.clone(),
          ));
        }
        if events.is_empty() {
          events.push(self.unknown(native, time));
        }
        events
      }
      SessionEvent::AssistantChunk(event) => {
        let step = (event.data.turn, event.data.step);
        match event.data.chunk {
          StreamChunk::Unknown(_) => vec![self.unknown(native, time)],
          StreamChunk::BlockEnd(ref chunk)
            if matches!(chunk.block, ContentBlock::Unknown(_) | ContentBlock::Image(_)) =>
          {
            vec![self.unknown(native, time)]
          }
          StreamChunk::Usage(usage) => {
            if self.usage_records.get(&step) == Some(&event.seq) {
              vec![self.usage(step, None, usage.usage, native, time)]
            } else {
              vec![]
            }
          }
          StreamChunk::BlockStart(ref chunk)
            if !matches!(chunk.block_type.as_str(), "text" | "reasoning" | "tool-call") =>
          {
            vec![self.unknown(native, time)]
          }
          StreamChunk::Finish(chunk) if !matches!(chunk.reason.kind.as_str(), "stop" | "tool-calls" | "max-tokens") => {
            vec![self.unknown(native, time)]
          }
          _ if self.assembled.contains(&step) => vec![],
          StreamChunk::TextDelta(chunk) => vec![self.text(
            None,
            Role::Assistant,
            MessageDelivery::Unspecified,
            Phase::Delta,
            chunk.text,
            time,
          )],
          StreamChunk::ReasoningDelta(chunk) => vec![self.reasoning(None, Phase::Delta, chunk.text, time)],
          // Known incomplete stream structure is inspectable metadata, not a
          // fabricated completed message/tool call or an unknown wire shape.
          _ => vec![self.metadata(MetadataKind::Stream, "partial assistant stream".into(), native, time)],
        }
      }
      SessionEvent::TurnStart(event) => vec![self.lifecycle(event.data.turn, None, Phase::Started, None, native, time)],
      SessionEvent::StepStart(event) => vec![self.lifecycle(
        event.data.turn,
        Some(event.data.step),
        Phase::Started,
        None,
        native,
        time,
      )],
      SessionEvent::StepEnd(event) => vec![self.lifecycle(
        event.data.turn,
        Some(event.data.step),
        Phase::Finished,
        None,
        native,
        time,
      )],
      SessionEvent::TurnEnd(event) => {
        let Some(ending) = turn_end::parse(&native["data"]) else {
          return vec![self.unknown(native, time)];
        };
        let mut events = Vec::new();
        if let Some(message) = ending.error_message {
          events.push(AgentEvent::Error(ErrorEvent {
            provider: Provider::Dsh,
            session_id: Some(self.session_id.clone()),
            message: message.to_string(),
            timestamp: time.clone(),
          }));
        }
        events.push(self.lifecycle(
          event.data.turn,
          None,
          Phase::Finished,
          Some(ending.outcome),
          native,
          time,
        ));
        events
      }
      SessionEvent::RequestContext(event) => vec![self.metadata(
        MetadataKind::Context,
        format!("request context {}/{}", event.data.provider, event.data.model),
        native,
        time,
      )],
      SessionEvent::SessionEndSeed(_) => {
        vec![self.metadata(MetadataKind::Session, "session seed ended".into(), native, time)]
      }
      SessionEvent::TodoWrite(event) => vec![self.metadata(
        MetadataKind::Context,
        format!("todo snapshot ({} items)", event.data.todos.len()),
        native,
        time,
      )],
    }
  }

  fn message(
    &mut self,
    id: &str,
    role: &str,
    blocks: Vec<ContentBlock>,
    step: Option<(u64, u64)>,
    native: Value,
    time: Option<String>,
  ) -> Vec<AgentEvent> {
    let role = match role {
      "user" => Role::User,
      "assistant" => Role::Assistant,
      "system" => Role::System,
      _ => Role::Unknown,
    };
    let has_calls = blocks.iter().any(|block| matches!(block, ContentBlock::ToolCall(_)));
    let delivery = if matches!(role, Role::Assistant) {
      if has_calls {
        MessageDelivery::Commentary
      } else {
        MessageDelivery::Final
      }
    } else {
      MessageDelivery::Unspecified
    };
    let mut events = Vec::new();
    let mut preserve_native = blocks.is_empty();
    for block in blocks {
      match block {
        ContentBlock::Text(block) => events.push(self.text(
          Some(id.into()),
          role,
          delivery,
          Phase::Finished,
          block.text,
          time.clone(),
        )),
        ContentBlock::Reasoning(block) => {
          events.push(self.reasoning(Some(id.into()), Phase::Finished, block.text, time.clone()))
        }
        ContentBlock::ToolCall(block) if step.is_some() => {
          let (turn, step) = step.unwrap();
          let key = (turn, step, block.id.clone());
          // The explicit tool/call is the execution boundary; assistant blocks
          // are only a fallback for imported logs without that record.
          if !self.calls.contains_key(&key) && self.emitted_calls.insert(key.clone()) {
            let input = arguments(&block.arguments);
            self.calls.insert(key, (block.name.clone(), input.clone()));
            events.push(self.tool(
              Some(turn),
              Some(block.id),
              Some(block.name),
              Some(id.into()),
              Some(input),
              None,
              None,
              Phase::Started,
              time.clone(),
            ));
          }
        }
        _ => preserve_native = true,
      }
    }
    // Attach provenance once per normalized content block instead of emitting
    // a second unknown event containing the entire plugin-origin message.
    let message = if native["type"] == "user/message" {
      &native["data"]
    } else {
      &native["data"]["message"]
    };
    let provenance = MessageProvenance {
      source: message["source"].clone(),
      display: None,
      native: None,
      surface_op: native.get("surfaceOp").cloned(),
      source_event_seqs: native
        .get("sourceEventSeqs")
        .and_then(|value| serde_json::from_value(value.clone()).ok()),
    };
    for event in &mut events {
      match event {
        AgentEvent::Message(event) => event.provenance = Some(provenance.clone()),
        AgentEvent::Reasoning(event) => event.provenance = Some(provenance.clone()),
        _ => {}
      }
    }
    if !preserve_native
      && provenance.surface_op.as_ref().is_some_and(Value::is_object)
      && !events
        .iter()
        .any(|event| matches!(event, AgentEvent::Message(_) | AgentEvent::Reasoning(_)))
    {
      events.push(self.metadata(
        MetadataKind::Context,
        "message surface replaced".into(),
        native.clone(),
        time.clone(),
      ));
    }
    if preserve_native || matches!(role, Role::Unknown) {
      events.push(self.unknown(native, time));
    }
    events
  }

  fn text(
    &self,
    id: Option<String>,
    role: Role,
    delivery: MessageDelivery,
    phase: Phase,
    text: String,
    time: Option<String>,
  ) -> AgentEvent {
    AgentEvent::Message(MessageEvent {
      provenance: None,
      provider: Provider::Dsh,
      session_id: Some(self.session_id.clone()),
      message_id: id,
      parent_id: None,
      role,
      delivery,
      phase,
      text,
      timestamp: time,
    })
  }

  fn reasoning(&self, id: Option<String>, phase: Phase, text: String, time: Option<String>) -> AgentEvent {
    AgentEvent::Reasoning(ReasoningEvent {
      provenance: None,
      provider: Provider::Dsh,
      session_id: Some(self.session_id.clone()),
      message_id: id,
      parent_id: None,
      phase,
      text: Some(text),
      summary: None,
      redacted: None,
      encrypted_content: None,
      signature: None,
      timestamp: time,
    })
  }

  #[allow(clippy::too_many_arguments)]
  fn tool(
    &self,
    turn_id: Option<u64>,
    id: Option<String>,
    name: Option<String>,
    message_id: Option<String>,
    input: Option<Value>,
    output: Option<Value>,
    is_error: Option<bool>,
    phase: Phase,
    time: Option<String>,
  ) -> AgentEvent {
    let record_kind = match (
      input.is_some(),
      output.as_ref().is_some_and(|value| !value.is_null()),
      phase,
    ) {
      (true, false, _) | (_, false, Phase::Started) => ToolRecordKind::Invocation,
      (_, _, Phase::Delta | Phase::Updated) => ToolRecordKind::Progress,
      (false, true, _) => ToolRecordKind::Result,
      _ => ToolRecordKind::Snapshot,
    };
    AgentEvent::ToolCall(ToolCallEvent {
      provider: Provider::Dsh,
      session_id: Some(self.session_id.clone()),
      turn_id: turn_id.map(|value| value.to_string()),
      message_id,
      parent_id: None,
      record_kind,
      tool_call_id: id,
      provider_tool_name: name.clone(),
      tool_kind: tool_kind_for_optional_name(name.as_deref()),
      summary: tool_summary_for_io(name.as_deref(), input.as_ref(), output.as_ref()),
      tool_name: name,
      transport: Some(ToolTransport::Native),
      phase,
      input,
      output,
      is_error,
      native: None,
      timestamp: time,
    })
  }

  fn unknown(&self, native: Value, time: Option<String>) -> AgentEvent {
    AgentEvent::Unknown(UnknownEvent {
      provider: Provider::Dsh,
      session_id: Some(self.session_id.clone()),
      native_type: native.get("type").and_then(Value::as_str).map(str::to_string),
      native: Some(native),
      timestamp: time,
    })
  }

  fn lifecycle(
    &self,
    turn: u64,
    step: Option<u64>,
    phase: Phase,
    outcome: Option<LifecycleOutcome>,
    native: Value,
    time: Option<String>,
  ) -> AgentEvent {
    AgentEvent::Lifecycle(LifecycleEvent {
      provider: Provider::Dsh,
      session_id: Some(self.session_id.clone()),
      turn_id: turn.to_string(),
      step_id: step.map(|step| step.to_string()),
      scope: if step.is_some() {
        LifecycleScope::Step
      } else {
        LifecycleScope::Turn
      },
      phase,
      outcome,
      native,
      timestamp: time,
    })
  }

  fn usage(
    &self,
    step: (u64, u64),
    message_id: Option<String>,
    usage: TokenUsage,
    native: Value,
    time: Option<String>,
  ) -> AgentEvent {
    let Some(input_tokens) = usage
      .input_tokens
      .checked_add(usage.cache_read_tokens.unwrap_or(0))
      .and_then(|tokens| tokens.checked_add(usage.cache_write_tokens.unwrap_or(0)))
    else {
      return self.unknown(native, time);
    };
    let raw = if native["type"] == "assistant/message" {
      &native["data"]["usage"]
    } else {
      &native["data"]["chunk"]["usage"]
    };
    AgentEvent::Usage(UsageEvent {
      kind: UsageKind::ModelCall,
      provider: Provider::Dsh,
      session_id: Some(self.session_id.clone()),
      turn_id: Some(step.0.to_string()),
      step_id: Some(step.1.to_string()),
      message_id,
      record_id: native.get("seq").and_then(Value::as_u64).map(|seq| seq.to_string()),
      input_tokens,
      output_tokens: usage.output_tokens,
      total_tokens: input_tokens.checked_add(usage.output_tokens),
      cache_read_tokens: usage.cache_read_tokens,
      cache_write_tokens: usage.cache_write_tokens,
      reasoning_tokens: usage.reasoning_tokens,
      native: raw.clone(),
      timestamp: time,
    })
  }

  fn metadata(&self, kind: MetadataKind, summary: String, native: Value, time: Option<String>) -> AgentEvent {
    AgentEvent::Metadata(MetadataEvent {
      provider: Provider::Dsh,
      session_id: Some(self.session_id.clone()),
      kind,
      native_type: native["type"].as_str().unwrap_or("event").to_string(),
      summary,
      native,
      timestamp: time,
    })
  }
}

fn arguments(raw: &str) -> Value {
  serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

fn valid_surface(native: &Value) -> bool {
  native
    .get("surfaceOp")
    .filter(|value| !value.is_null())
    .is_none_or(|surface| {
      matches!(
        serde_json::from_value::<SurfaceOp>(surface.clone()),
        Ok(SurfaceOp::Append | SurfaceOp::Replace(_))
      )
    })
}
