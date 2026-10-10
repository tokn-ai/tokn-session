//! Persisted context, per-response accounting, and session snapshots.
use serde::Deserialize;
use serde_json::Value;
use tokn_codex_protocol::{RolloutItem, RolloutLine, TokenUsageRecordItem};
use tokn_session_core::{AgentEvent, MetadataEvent, MetadataKind, Provider, UnknownEvent, UsageEvent, UsageKind};

#[derive(Default)]
pub(crate) struct RecordsNormalizer {
  compactions: crate::compaction::Compactions,
  last_info: Option<Value>,
  last_limits: Option<Value>,
}

impl RecordsNormalizer {
  pub(crate) fn normalize(
    &mut self,
    line: &RolloutLine,
    session_id: Option<String>,
    canonical_items: bool,
  ) -> Option<Vec<AgentEvent>> {
    if let Some(events) = self.compactions.normalize(line, session_id.clone(), canonical_items) {
      self.last_info = None;
      return Some(events);
    }
    let context = RecordContext {
      session_id,
      timestamp: line.timestamp().map(str::to_owned),
      native: line.native(),
    };
    let payload = &line.native()["payload"];
    let classification = match line.item() {
      RolloutItem::TokenUsageRecord(item) => {
        return Some(vec![context.token_usage_record(item, line.ordinal())]);
      }
      RolloutItem::Unknown(item) if item.native_type.as_deref() == Some("token_usage_record") => {
        return Some(vec![context.unknown()]);
      }
      RolloutItem::TurnContext(item) => {
        let valid = item.turn_id.is_some() || item.model.is_some() || item.cwd.is_some();
        valid.then_some((MetadataKind::Configuration, "turn context"))
      }
      RolloutItem::WorldState(item) => {
        (item.full.is_some() && payload.get("state").is_some()).then_some((MetadataKind::Context, "world state"))
      }
      RolloutItem::InterAgentCommunicationMetadata(item) => item
        .trigger_turn
        .map(|_| (MetadataKind::Context, "agent communication context")),
      // Valid compaction observations were handled above. Keep malformed
      // checkpoints visible as unknowns, never downgrade them to metadata.
      RolloutItem::Compacted(_) => {
        self.last_info = None;
        None
      }
      RolloutItem::EventMessage(item) => match item.event_type.as_deref() {
        Some("token_count") => return Some(self.token_count(&context, payload, line.ordinal())),
        Some("thread_rolled_back") => {
          self.last_info = None;
          payload["num_turns"]
            .as_u64()
            .map(|_| (MetadataKind::Context, "thread rolled back"))
        }
        Some("item_completed") if canonical_items && payload["item"]["type"] == "ContextCompaction" => {
          self.last_info = None;
          None
        }
        _ => return None,
      },
      _ => return None,
    };
    Some(vec![match classification {
      Some((kind, summary)) => context.metadata(kind, summary),
      None => context.unknown(),
    }])
  }

  fn token_count(&mut self, context: &RecordContext<'_>, payload: &Value, ordinal: Option<u64>) -> Vec<AgentEvent> {
    let Ok(record) = serde_json::from_value::<TokenCount>(payload.clone()) else {
      self.last_info = None;
      self.last_limits = None;
      self.compactions.clear();
      return vec![context.unknown()];
    };
    if payload.get("info").is_none() && payload.get("rate_limits").is_none() {
      self.compactions.clear();
      return vec![context.unknown()];
    }
    let mut events = Vec::new();
    if let Some(info) = record.info {
      if self.last_info.as_ref() != Some(&payload["info"]) {
        let total = info.total_token_usage;
        events.push(AgentEvent::Usage(UsageEvent {
          kind: UsageKind::SessionSnapshot,
          provider: Provider::Codex,
          session_id: context.session_id.clone(),
          turn_id: None,
          step_id: None,
          message_id: None,
          record_id: ordinal.map(|ordinal| ordinal.to_string()),
          // Codex input already includes cached input: never add it twice.
          input_tokens: total.input_tokens,
          output_tokens: total.output_tokens,
          total_tokens: Some(total.total_tokens),
          cache_read_tokens: Some(total.cached_input_tokens),
          cache_write_tokens: total.cache_write_input_tokens,
          reasoning_tokens: Some(total.reasoning_output_tokens),
          // Keep last-call counters and context-window estimates for inspection;
          // they cannot safely be interpreted as a new model call.
          native: payload["info"].clone(),
          timestamp: context.timestamp.clone(),
        }));
        self.last_info = Some(payload["info"].clone());
      }
    } else {
      self.last_info = None;
      events.push(context.metadata(MetadataKind::Diagnostic, "usage unavailable"));
    }
    let limits = payload.get("rate_limits");
    if limits != self.last_limits.as_ref() {
      if limits.is_some_and(|limits| !limits.is_null())
        || self.last_limits.as_ref().is_some_and(|limits| !limits.is_null())
      {
        events.push(context.metadata(MetadataKind::Diagnostic, "rate limits updated"));
      }
      self.last_limits = limits.cloned();
    }
    events
  }
}

struct RecordContext<'a> {
  session_id: Option<String>,
  timestamp: Option<String>,
  native: &'a Value,
}

impl RecordContext<'_> {
  fn token_usage_record(&self, record: &TokenUsageRecordItem, ordinal: Option<u64>) -> AgentEvent {
    let Some(usage) = &record.usage else {
      return self.unknown();
    };
    AgentEvent::Usage(UsageEvent {
      kind: UsageKind::ModelCall,
      provider: Provider::Codex,
      session_id: self.session_id.clone(),
      turn_id: record.turn_id.clone().filter(|id| !id.is_empty()),
      step_id: None,
      // A response can contain several messages, reasoning, and tool items.
      // Its ID identifies the accounting record, not an assistant message.
      message_id: None,
      record_id: record
        .response_id
        .clone()
        .filter(|id| !id.is_empty())
        .or_else(|| ordinal.map(|ordinal| ordinal.to_string())),
      input_tokens: usage.input_tokens,
      output_tokens: usage.output_tokens,
      total_tokens: Some(usage.total_tokens),
      cache_read_tokens: Some(usage.cached_input_tokens),
      cache_write_tokens: usage.cache_write_input_tokens,
      reasoning_tokens: Some(usage.reasoning_output_tokens),
      // Turn/thread totals are inspection context only. Emitting them as
      // additional calls would count the same usage more than once.
      native: self.native["payload"].clone(),
      timestamp: self.timestamp.clone(),
    })
  }

  fn native_type(&self) -> String {
    let kind = self.native["type"].as_str().unwrap_or("event");
    if kind == "event_msg" {
      format!(
        "event_msg.{}",
        self.native["payload"]["type"].as_str().unwrap_or("event")
      )
    } else {
      kind.into()
    }
  }

  fn metadata(&self, kind: MetadataKind, summary: &str) -> AgentEvent {
    AgentEvent::Metadata(MetadataEvent {
      provider: Provider::Codex,
      session_id: self.session_id.clone(),
      kind,
      native_type: self.native_type(),
      summary: summary.into(),
      native: self.native.clone(),
      timestamp: self.timestamp.clone(),
    })
  }

  fn unknown(&self) -> AgentEvent {
    AgentEvent::Unknown(UnknownEvent {
      provider: Provider::Codex,
      session_id: self.session_id.clone(),
      native_type: Some(self.native_type()),
      native: Some(self.native.clone()),
      timestamp: self.timestamp.clone(),
    })
  }
}

#[derive(Deserialize)]
struct TokenCount {
  info: Option<UsageInfo>,
  #[serde(rename = "rate_limits")]
  _rate_limits: Option<RateLimits>,
}

#[derive(Deserialize)]
struct UsageInfo {
  total_token_usage: Counters,
  #[serde(rename = "last_token_usage")]
  _last_token_usage: Counters,
  #[serde(rename = "model_context_window")]
  _model_context_window: Option<u64>,
}

#[derive(Deserialize)]
struct Counters {
  input_tokens: u64,
  cached_input_tokens: u64,
  cache_write_input_tokens: Option<u64>,
  output_tokens: u64,
  reasoning_output_tokens: u64,
  total_tokens: u64,
}

// Validate known rate-limit fields without closing the evolving schema.
// The original payload, including future extensions, is retained in metadata.
#[allow(dead_code)]
#[derive(Deserialize)]
struct RateLimits {
  limit_id: Option<String>,
  limit_name: Option<String>,
  primary: Option<RateWindow>,
  secondary: Option<RateWindow>,
  credits: Option<Credits>,
  individual_limit: Option<SpendControlLimit>,
  spend_control_reached: Option<bool>,
  plan_type: Option<String>,
  rate_limit_reached_type: Option<String>,
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct RateWindow {
  used_percent: f64,
  window_minutes: Option<u64>,
  resets_at: Option<i64>,
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct Credits {
  has_credits: bool,
  unlimited: bool,
  balance: Option<String>,
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct SpendControlLimit {
  limit: String,
  used: String,
  remaining_percent: i32,
  resets_at: i64,
}
