//! Provider-neutral projections of append-only tool records.
//!
//! Provider histories do not agree on whether a tool invocation, progress
//! update, and result are one record or several. `ToolOperationAssembler`
//! keeps the normalized stream append-only while giving historical and live
//! consumers one safe, incrementally updated logical operation.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;

use crate::{
  AgentEvent, Phase, Provider, RecordedImage, ToolCallEvent, ToolKind, ToolRecordKind, ToolSummary, ToolTransport,
  tool_kind_for_optional_name, tool_summary_for_kind_io,
};

/// Stable identity for a logical tool operation inside one assembled stream.
///
/// A provider tool-call ID is only safe for correlation when it is scoped by a
/// known session. Reused IDs receive an occurrence number. Records with a
/// missing identity deliberately stay uncorrelated rather than being joined to
/// a plausible but unrelated operation.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolOperationId {
  Correlated {
    provider: Provider,
    session_id: String,
    turn_id: Option<String>,
    tool_call_id: String,
    occurrence: u64,
  },
  Uncorrelated {
    provider: Provider,
    session_id: Option<String>,
    source_event_index: usize,
  },
}

/// A source-record key retained by an assembled operation.
///
/// `source_event_index` is intentionally part of the key: provider message
/// IDs and timestamps are often absent, and neither is guaranteed to be a
/// unique record identity on its own.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct ToolSourceEventKey {
  pub provider: Provider,
  pub session_id: Option<String>,
  pub turn_id: Option<String>,
  pub message_id: Option<String>,
  pub tool_call_id: Option<String>,
  pub record_kind: ToolRecordKind,
  pub source_event_index: usize,
}

/// The derived state of a logical tool operation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOperationStatus {
  Pending,
  Running,
  Completed,
  Failed,
}

impl ToolOperationStatus {
  pub fn is_finished(self) -> bool {
    matches!(self, Self::Completed | Self::Failed)
  }
}

/// One image owned by the latest output record of an assembled operation.
/// Its source position is required to resolve the image against the exact record.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ToolOperationImage {
  pub source_event_index: usize,
  #[serde(flatten)]
  pub image: RecordedImage,
}

/// One logical tool operation assembled from one or more source records.
///
/// The semantic fields are intentionally value-level, rather than provider
/// wrappers. `native` retains the source payloads for inspection when an
/// adapter has decoded a cleaner semantic input or output.
#[derive(Clone, Debug, Serialize)]
pub struct ToolOperation {
  pub id: ToolOperationId,
  pub provider: Provider,
  pub session_id: Option<String>,
  pub turn_id: Option<String>,
  pub tool_call_id: Option<String>,
  pub provider_tool_name: Option<String>,
  pub tool_name: Option<String>,
  pub tool_kind: ToolKind,
  pub transport: Option<ToolTransport>,
  pub summary: Option<ToolSummary>,
  pub input: Option<Value>,
  pub output: Option<Value>,
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub images: Vec<ToolOperationImage>,
  pub is_error: Option<bool>,
  pub status: ToolOperationStatus,
  pub started_at: Option<String>,
  pub updated_at: Option<String>,
  /// Positions in the source event stream, in observation order.
  pub source_event_indices: Vec<usize>,
  /// Provider record keys aligned with `source_event_indices`.
  pub source_event_keys: Vec<ToolSourceEventKey>,
  /// Provider-native payloads aligned with the source records that supplied
  /// them. Empty when a normalizer has no native record to retain.
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub native: Vec<Value>,
}

impl ToolOperation {
  pub fn is_finished(&self) -> bool {
    self.status.is_finished()
  }

  /// Source position where this operation belongs in a historical timeline.
  ///
  /// An unfinished operation is introduced at its first record so a live
  /// view can update it in place. Once it is terminal, placing the final card
  /// at its last contributing record preserves the chronology of messages and
  /// reasoning that occurred while the tool was running.
  pub fn timeline_source_event_index(&self) -> Option<usize> {
    if self.is_finished() {
      self.source_event_indices.last().copied()
    } else {
      self.source_event_indices.first().copied()
    }
  }
}

/// Describes the mutation made by one streamed source event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolOperationUpdate {
  pub operation_index: usize,
  pub operation_id: ToolOperationId,
  pub created: bool,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ToolCorrelationKey {
  provider: Provider,
  session_id: String,
  turn_id: Option<String>,
  tool_call_id: String,
}

impl ToolCorrelationKey {
  fn from_event(event: &ToolCallEvent) -> Option<Self> {
    let session_id = event.session_id.as_deref()?.trim();
    let tool_call_id = event.tool_call_id.as_deref()?.trim();
    (!session_id.is_empty() && !tool_call_id.is_empty()).then(|| Self {
      provider: event.provider,
      session_id: session_id.to_string(),
      turn_id: event.turn_id.clone(),
      tool_call_id: tool_call_id.to_string(),
    })
  }
}

enum ActiveMatch {
  None,
  One(usize),
  Ambiguous,
}

/// Incrementally assembles source `AgentEvent`s into logical tool operations.
///
/// Feed records in source order. The assembler never joins missing IDs, and it
/// refuses to choose between overlapping invocations that reused the same
/// scoped call ID. This favors an extra standalone operation over incorrectly
/// attaching a result to the wrong invocation.
#[derive(Debug, Default)]
pub struct ToolOperationAssembler {
  operations: Vec<ToolOperation>,
  active: HashMap<ToolCorrelationKey, Vec<usize>>,
  next_occurrence: HashMap<ToolCorrelationKey, u64>,
}

impl ToolOperationAssembler {
  pub fn new() -> Self {
    Self::default()
  }

  /// Incorporate one event at its position in the source stream.
  ///
  /// Non-tool events are ignored. `source_event_index` should be monotonically
  /// increasing for a given assembler; it is retained for consumers that need
  /// to map an operation back to its original events.
  pub fn ingest(&mut self, source_event_index: usize, event: &AgentEvent) -> Option<ToolOperationUpdate> {
    let AgentEvent::ToolCall(tool) = event else {
      return None;
    };

    Some(self.ingest_tool_call(source_event_index, tool))
  }

  /// Incorporate one already-selected tool record.
  pub fn ingest_tool_call(&mut self, source_event_index: usize, event: &ToolCallEvent) -> ToolOperationUpdate {
    let correlation_key = ToolCorrelationKey::from_event(event);
    let active_match = correlation_key
      .as_ref()
      .map(|key| self.active_match(key))
      .unwrap_or(ActiveMatch::None);

    let target = match event.record_kind {
      ToolRecordKind::Invocation => None,
      ToolRecordKind::Progress | ToolRecordKind::Snapshot | ToolRecordKind::Result => match active_match {
        ActiveMatch::One(index) => Some(index),
        ActiveMatch::None | ActiveMatch::Ambiguous => None,
      },
    };

    let (operation_index, created) = if let Some(operation_index) = target {
      self.update_operation(operation_index, source_event_index, event);
      (operation_index, false)
    } else {
      let operation_index = self.create_operation(source_event_index, event, correlation_key.as_ref());
      (operation_index, true)
    };

    if let Some(key) = correlation_key {
      self.update_active_set(key, operation_index, event, created, active_match);
    }

    ToolOperationUpdate {
      operation_index,
      operation_id: self.operations[operation_index].id.clone(),
      created,
    }
  }

  pub fn operations(&self) -> &[ToolOperation] {
    &self.operations
  }

  pub fn into_operations(self) -> Vec<ToolOperation> {
    self.operations
  }

  fn active_match(&self, key: &ToolCorrelationKey) -> ActiveMatch {
    match self.active.get(key).map(Vec::as_slice) {
      None | Some([]) => ActiveMatch::None,
      Some([index]) => ActiveMatch::One(*index),
      Some(_) => ActiveMatch::Ambiguous,
    }
  }

  fn create_operation(
    &mut self,
    source_event_index: usize,
    event: &ToolCallEvent,
    correlation_key: Option<&ToolCorrelationKey>,
  ) -> usize {
    let id = if let Some(key) = correlation_key {
      let occurrence = self.next_occurrence.entry(key.clone()).or_default();
      let id = ToolOperationId::Correlated {
        provider: key.provider,
        session_id: key.session_id.clone(),
        turn_id: key.turn_id.clone(),
        tool_call_id: key.tool_call_id.clone(),
        occurrence: *occurrence,
      };
      *occurrence += 1;
      id
    } else {
      ToolOperationId::Uncorrelated {
        provider: event.provider,
        session_id: event.session_id.clone(),
        source_event_index,
      }
    };

    let mut operation = ToolOperation {
      id,
      provider: event.provider,
      session_id: event.session_id.clone(),
      turn_id: event.turn_id.clone(),
      tool_call_id: event.tool_call_id.clone(),
      provider_tool_name: event.effective_provider_tool_name().map(str::to_string),
      tool_name: event.tool_name.clone(),
      tool_kind: event.tool_kind,
      transport: event.transport,
      summary: event.summary.clone(),
      input: event.input.clone(),
      output: event.output.clone(),
      images: source_images(source_event_index, event),
      is_error: event.is_error,
      status: status_for_first_record(event),
      started_at: event.timestamp.clone(),
      updated_at: event.timestamp.clone(),
      source_event_indices: vec![source_event_index],
      source_event_keys: vec![source_event_key(source_event_index, event)],
      native: event.native.clone().into_iter().collect(),
    };
    refresh_summary(&mut operation);

    let index = self.operations.len();
    self.operations.push(operation);
    index
  }

  fn update_operation(&mut self, operation_index: usize, source_event_index: usize, event: &ToolCallEvent) {
    let operation = &mut self.operations[operation_index];
    operation.source_event_indices.push(source_event_index);
    operation
      .source_event_keys
      .push(source_event_key(source_event_index, event));
    if let Some(native) = &event.native {
      operation.native.push(native.clone());
    }

    if operation.turn_id.is_none() {
      operation.turn_id = event.turn_id.clone();
    }
    if operation.provider_tool_name.is_none() {
      operation.provider_tool_name = event.effective_provider_tool_name().map(str::to_string);
    }
    if operation.tool_name.is_none() {
      operation.tool_name = event.tool_name.clone();
    }
    if matches!(operation.tool_kind, ToolKind::Unknown) && !matches!(event.tool_kind, ToolKind::Unknown) {
      operation.tool_kind = event.tool_kind;
    }
    if operation.transport.is_none() {
      operation.transport = event.transport;
    }
    if operation.input.is_none() {
      operation.input = event.input.clone();
    }
    if event.output.is_some() {
      operation.output = event.output.clone();
    }
    if event.output.is_some() || !event.images.is_empty() {
      // A new output owns its complete image set, including an explicitly
      // empty output. Status-only progress must not retire an earlier image.
      operation.images = source_images(source_event_index, event);
    }
    if event.is_error.is_some() {
      operation.is_error = event.is_error;
    }
    if let Some(summary) = event.summary.clone() {
      merge_summary(&mut operation.summary, summary);
    }
    if event.timestamp.is_some() {
      operation.updated_at = event.timestamp.clone();
    }

    update_status(operation, event);
    refresh_summary(operation);
  }

  fn update_active_set(
    &mut self,
    key: ToolCorrelationKey,
    operation_index: usize,
    event: &ToolCallEvent,
    created: bool,
    active_match: ActiveMatch,
  ) {
    let terminal = record_is_terminal(event);
    match event.record_kind {
      ToolRecordKind::Invocation => {
        // Once this key has multiple active candidates, no later record can
        // be joined safely. Keep the standalone invocation visible, but do
        // not grow the poisoned candidate set forever.
        if !terminal && !matches!(active_match, ActiveMatch::Ambiguous) {
          self.active.entry(key).or_default().push(operation_index);
        }
      }
      ToolRecordKind::Progress | ToolRecordKind::Snapshot => {
        if created {
          // When the key was ambiguous, retain the standalone record but do
          // not add a third candidate that would make later recovery less
          // likely. A missing active record, however, can become the anchor
          // for a following result.
          if !terminal && matches!(active_match, ActiveMatch::None) {
            self.active.entry(key.clone()).or_default().push(operation_index);
          }
        }
        if terminal {
          self.remove_active(&key, operation_index);
        }
      }
      ToolRecordKind::Result => {
        self.remove_active(&key, operation_index);
      }
    }
  }

  fn remove_active(&mut self, key: &ToolCorrelationKey, operation_index: usize) {
    let Some(active) = self.active.get_mut(key) else {
      return;
    };
    active.retain(|index| *index != operation_index);
    if active.is_empty() {
      self.active.remove(key);
    }
  }
}

/// Assemble all tool operations in a historical event slice.
pub fn assemble_tool_operations(events: &[AgentEvent]) -> Vec<ToolOperation> {
  let mut assembler = ToolOperationAssembler::new();
  for (source_event_index, event) in events.iter().enumerate() {
    assembler.ingest(source_event_index, event);
  }
  assembler.into_operations()
}

fn source_images(source_event_index: usize, event: &ToolCallEvent) -> Vec<ToolOperationImage> {
  event
    .images
    .iter()
    .cloned()
    .map(|image| ToolOperationImage {
      source_event_index,
      image,
    })
    .collect()
}

fn source_event_key(source_event_index: usize, event: &ToolCallEvent) -> ToolSourceEventKey {
  ToolSourceEventKey {
    provider: event.provider,
    session_id: event.session_id.clone(),
    turn_id: event.turn_id.clone(),
    message_id: event.message_id.clone(),
    tool_call_id: event.tool_call_id.clone(),
    record_kind: event.record_kind,
    source_event_index,
  }
}

fn status_for_first_record(event: &ToolCallEvent) -> ToolOperationStatus {
  if record_is_terminal(event) {
    return terminal_status(event.is_error);
  }

  match event.record_kind {
    ToolRecordKind::Invocation => ToolOperationStatus::Pending,
    ToolRecordKind::Progress | ToolRecordKind::Snapshot => ToolOperationStatus::Running,
    ToolRecordKind::Result => unreachable!("result records are terminal"),
  }
}

fn update_status(operation: &mut ToolOperation, event: &ToolCallEvent) {
  if record_is_terminal(event) {
    operation.status = terminal_status(event.is_error.or(operation.is_error));
    return;
  }

  if !operation.status.is_finished() && matches!(event.record_kind, ToolRecordKind::Progress | ToolRecordKind::Snapshot)
  {
    operation.status = ToolOperationStatus::Running;
  }
}

fn terminal_status(is_error: Option<bool>) -> ToolOperationStatus {
  if is_error == Some(true) {
    ToolOperationStatus::Failed
  } else {
    ToolOperationStatus::Completed
  }
}

fn record_is_terminal(event: &ToolCallEvent) -> bool {
  matches!(event.record_kind, ToolRecordKind::Result)
    || event.is_error == Some(true)
    || (matches!(event.record_kind, ToolRecordKind::Snapshot) && matches!(event.phase, Phase::Finished))
}

fn refresh_summary(operation: &mut ToolOperation) {
  let derived_kind = tool_kind_for_optional_name(operation.tool_name.as_deref());
  if matches!(operation.tool_kind, ToolKind::Unknown) && !matches!(derived_kind, ToolKind::Unknown) {
    operation.tool_kind = derived_kind;
  }

  if let Some(summary) =
    tool_summary_for_kind_io(operation.tool_kind, operation.input.as_ref(), operation.output.as_ref())
  {
    merge_summary(&mut operation.summary, summary);
  }
}

/// Merge facts from later source records without throwing away the useful
/// invocation context. A result often supplies only an exit status while the
/// invocation has the command, path, or terminal session identity.
fn merge_summary(target: &mut Option<ToolSummary>, source: ToolSummary) {
  let Some(target) = target.as_mut() else {
    *target = Some(source);
    return;
  };

  match (target, source) {
    (
      ToolSummary::CodeExecution { language },
      ToolSummary::CodeExecution {
        language: source_language,
      },
    ) => fill_missing(language, source_language),
    (
      ToolSummary::Shell {
        command,
        cwd,
        exit_code,
      },
      ToolSummary::Shell {
        command: source_command,
        cwd: source_cwd,
        exit_code: source_exit_code,
      },
    ) => {
      fill_missing(command, source_command);
      fill_missing(cwd, source_cwd);
      if source_exit_code.is_some() {
        *exit_code = source_exit_code;
      }
    }
    (
      ToolSummary::Terminal {
        session_id,
        action,
        chars_len,
        wait_ms,
      },
      ToolSummary::Terminal {
        session_id: source_session_id,
        action: source_action,
        chars_len: source_chars_len,
        wait_ms: source_wait_ms,
      },
    ) => {
      fill_missing(session_id, source_session_id);
      fill_missing(action, source_action);
      fill_missing(chars_len, source_chars_len);
      fill_missing(wait_ms, source_wait_ms);
    }
    (ToolSummary::FileRead { path }, ToolSummary::FileRead { path: source_path }) => {
      fill_missing(path, source_path);
    }
    (
      ToolSummary::FileWrite { path, bytes },
      ToolSummary::FileWrite {
        path: source_path,
        bytes: source_bytes,
      },
    ) => {
      fill_missing(path, source_path);
      fill_missing(bytes, source_bytes);
    }
    (
      ToolSummary::FileEdit { path, added, removed },
      ToolSummary::FileEdit {
        path: source_path,
        added: source_added,
        removed: source_removed,
      },
    ) => {
      fill_missing(path, source_path);
      fill_missing(added, source_added);
      fill_missing(removed, source_removed);
    }
    (ToolSummary::Search { query }, ToolSummary::Search { query: source_query }) => {
      fill_missing(query, source_query);
    }
    (ToolSummary::Web { url }, ToolSummary::Web { url: source_url }) => {
      fill_missing(url, source_url);
    }
    (ToolSummary::Task { title }, ToolSummary::Task { title: source_title }) => {
      fill_missing(title, source_title);
    }
    // A provider changed its claimed tool family mid-operation. Retain the
    // first coherent summary instead of fabricating a hybrid one.
    (_, _) => {}
  }
}

fn fill_missing<T>(target: &mut Option<T>, source: Option<T>) {
  if target.is_none() {
    *target = source;
  }
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;
  use crate::{AgentEvent, Phase, Provider, ToolCallEvent, ToolKind, ToolRecordKind};

  fn tool(
    record_kind: ToolRecordKind,
    call_id: Option<&str>,
    input: Option<Value>,
    output: Option<Value>,
  ) -> AgentEvent {
    AgentEvent::ToolCall(ToolCallEvent {
      images: Vec::new(),
      provider: Provider::Codex,
      session_id: Some("session-1".to_string()),
      turn_id: Some("turn-1".to_string()),
      message_id: None,
      parent_id: None,
      record_kind,
      tool_call_id: call_id.map(str::to_string),
      provider_tool_name: None,
      tool_name: Some("write_stdin".to_string()),
      tool_kind: ToolKind::Terminal,
      transport: None,
      summary: None,
      phase: match record_kind {
        ToolRecordKind::Invocation => Phase::Started,
        ToolRecordKind::Progress => Phase::Updated,
        ToolRecordKind::Result | ToolRecordKind::Snapshot => Phase::Finished,
      },
      input,
      output,
      is_error: None,
      native: None,
      timestamp: None,
    })
  }

  fn image_tool(record_kind: ToolRecordKind, call_id: Option<&str>, output: Option<Value>, ids: &[&str]) -> AgentEvent {
    let mut event = tool(record_kind, call_id, None, output);
    let AgentEvent::ToolCall(tool) = &mut event else {
      unreachable!()
    };
    tool.images = ids
      .iter()
      .map(|id| RecordedImage {
        image_id: (*id).into(),
        mime_type: Some("image/png".into()),
        name: None,
        source: crate::RecordedImageSource::Base64 {
          data: format!("payload-{id}"),
        },
      })
      .collect();
    event
  }

  #[test]
  fn latest_output_replaces_images_but_status_only_progress_preserves_them() {
    let mut assembler = ToolOperationAssembler::new();
    assembler.ingest(0, &tool(ToolRecordKind::Invocation, Some("call-1"), None, None));
    assembler.ingest(
      1,
      &image_tool(
        ToolRecordKind::Progress,
        Some("call-1"),
        Some(json!("first output")),
        &["first"],
      ),
    );
    assembler.ingest(2, &image_tool(ToolRecordKind::Progress, Some("call-1"), None, &[]));
    let operation = &assembler.operations()[0];
    assert_eq!(operation.images[0].image.image_id, "first");
    assert_eq!(operation.images[0].source_event_index, 1);

    assembler.ingest(
      3,
      &image_tool(
        ToolRecordKind::Progress,
        Some("call-1"),
        None,
        &["replacement", "second"],
      ),
    );
    let operation = &assembler.operations()[0];
    assert_eq!(
      operation
        .images
        .iter()
        .map(|image| image.image.image_id.as_str())
        .collect::<Vec<_>>(),
      ["replacement", "second"]
    );
    assert!(operation.images.iter().all(|image| image.source_event_index == 3));
    let serialized = serde_json::to_value(operation).unwrap();
    assert_eq!(serialized["images"][0]["source_event_index"], 3);
    assert_eq!(serialized["images"][0]["image_id"], "replacement");
    assert!(
      serialized["images"][0].get("image").is_none(),
      "recorded image fields stay flattened"
    );

    assembler.ingest(
      4,
      &image_tool(ToolRecordKind::Progress, Some("call-1"), Some(json!("")), &[]),
    );
    assert!(
      assembler.operations()[0].images.is_empty(),
      "an explicit output without images clears earlier attachments"
    );
    assert!(
      serde_json::to_value(&assembler.operations()[0])
        .unwrap()
        .get("images")
        .is_none()
    );
    assembler.ingest(5, &image_tool(ToolRecordKind::Result, Some("call-1"), None, &["final"]));
    let operation = &assembler.operations()[0];
    assert!(operation.is_finished());
    assert_eq!(operation.images[0].image.image_id, "final");
    assert_eq!(operation.images[0].source_event_index, 5);
  }

  #[test]
  fn image_only_tool_results_remain_completed_and_keep_their_source_record() {
    let events = [image_tool(
      ToolRecordKind::Result,
      Some("image-call"),
      None,
      &["image.v1.0"],
    )];
    let operations = assemble_tool_operations(&events);
    assert_eq!(operations.len(), 1);
    assert!(operations[0].is_finished());
    assert!(operations[0].output.is_none());
    assert_eq!(operations[0].images.len(), 1);
    assert_eq!(operations[0].images[0].source_event_index, 0);
  }

  #[test]
  fn image_ids_never_join_outputs_to_a_different_call_or_guess_an_orphan_owner() {
    let events = [
      tool(ToolRecordKind::Invocation, Some("first-call"), None, None),
      tool(ToolRecordKind::Invocation, Some("second-call"), None, None),
      image_tool(ToolRecordKind::Result, Some("second-call"), None, &["image.v1.0"]),
      image_tool(ToolRecordKind::Result, None, None, &["image.v1.0"]),
      image_tool(ToolRecordKind::Result, Some("first-call"), None, &["image.v1.0"]),
    ];
    let operations = assemble_tool_operations(&events);
    assert_eq!(operations.len(), 3);
    assert_eq!(operations[0].tool_call_id.as_deref(), Some("first-call"));
    assert_eq!(operations[0].images[0].source_event_index, 4);
    assert_eq!(operations[1].tool_call_id.as_deref(), Some("second-call"));
    assert_eq!(operations[1].images[0].source_event_index, 2);
    assert!(matches!(
      operations[2].id,
      ToolOperationId::Uncorrelated {
        source_event_index: 3,
        ..
      }
    ));
    assert_eq!(operations[2].images[0].source_event_index, 3);
  }

  #[test]
  fn combines_invocation_and_result_without_losing_temporal_sources() {
    let events = vec![
      tool(
        ToolRecordKind::Invocation,
        Some("call-1"),
        Some(json!({ "session_id": 42, "chars": "", "yield_time_ms": 30_000 })),
        None,
      ),
      tool(
        ToolRecordKind::Result,
        Some("call-1"),
        None,
        Some(json!({ "session_id": 42, "text": "still working" })),
      ),
    ];

    let operations = assemble_tool_operations(&events);
    assert_eq!(operations.len(), 1);
    let operation = &operations[0];
    assert_eq!(operation.source_event_indices, vec![0, 1]);
    assert!(matches!(operation.status, ToolOperationStatus::Completed));
    assert_eq!(operation.input.as_ref().unwrap()["session_id"], 42);
    assert_eq!(operation.output.as_ref().unwrap()["text"], "still working");
    assert!(matches!(
      operation.summary,
      Some(ToolSummary::Terminal {
        session_id: Some(ref session_id),
        action: Some(crate::TerminalAction::Wait),
        chars_len: Some(0),
        wait_ms: Some(30_000),
      }) if session_id == "42"
    ));
  }

  #[test]
  fn missing_call_ids_are_never_correlated() {
    let events = vec![
      tool(ToolRecordKind::Invocation, None, Some(json!({ "chars": "one" })), None),
      tool(ToolRecordKind::Result, None, None, Some(json!({ "text": "two" }))),
    ];

    let operations = assemble_tool_operations(&events);
    assert_eq!(operations.len(), 2);
    assert!(
      operations
        .iter()
        .all(|operation| matches!(operation.id, ToolOperationId::Uncorrelated { .. }))
    );
  }

  #[test]
  fn reused_ids_get_distinct_occurrences_after_completion() {
    let events = vec![
      tool(
        ToolRecordKind::Invocation,
        Some("call-1"),
        Some(json!({ "chars": "first" })),
        None,
      ),
      tool(
        ToolRecordKind::Result,
        Some("call-1"),
        None,
        Some(json!({ "text": "first result" })),
      ),
      tool(
        ToolRecordKind::Invocation,
        Some("call-1"),
        Some(json!({ "chars": "second" })),
        None,
      ),
      tool(
        ToolRecordKind::Result,
        Some("call-1"),
        None,
        Some(json!({ "text": "second result" })),
      ),
    ];

    let operations = assemble_tool_operations(&events);
    assert_eq!(operations.len(), 2);
    assert!(matches!(
      operations[0].id,
      ToolOperationId::Correlated { occurrence: 0, .. }
    ));
    assert!(matches!(
      operations[1].id,
      ToolOperationId::Correlated { occurrence: 1, .. }
    ));
    assert_eq!(operations[0].output.as_ref().unwrap()["text"], "first result");
    assert_eq!(operations[1].output.as_ref().unwrap()["text"], "second result");
  }

  #[test]
  fn overlapping_reused_ids_do_not_guess_a_result_target() {
    let events = vec![
      tool(
        ToolRecordKind::Invocation,
        Some("call-1"),
        Some(json!({ "chars": "first" })),
        None,
      ),
      tool(
        ToolRecordKind::Invocation,
        Some("call-1"),
        Some(json!({ "chars": "second" })),
        None,
      ),
      tool(
        ToolRecordKind::Result,
        Some("call-1"),
        None,
        Some(json!({ "text": "ambiguous" })),
      ),
    ];

    let operations = assemble_tool_operations(&events);
    assert_eq!(operations.len(), 3);
    assert!(
      operations[..2]
        .iter()
        .all(|operation| matches!(operation.status, ToolOperationStatus::Pending))
    );
    assert!(matches!(operations[2].status, ToolOperationStatus::Completed));
    assert_eq!(operations[2].source_event_indices, vec![2]);
  }

  #[test]
  fn ambiguous_reused_ids_do_not_grow_the_active_candidate_set() {
    let first = tool(
      ToolRecordKind::Invocation,
      Some("call-1"),
      Some(json!({ "chars": "first" })),
      None,
    );
    let second = tool(
      ToolRecordKind::Invocation,
      Some("call-1"),
      Some(json!({ "chars": "second" })),
      None,
    );
    let ambiguous_result = tool(
      ToolRecordKind::Result,
      Some("call-1"),
      None,
      Some(json!({ "text": "ambiguous" })),
    );
    let later_invocation = tool(
      ToolRecordKind::Invocation,
      Some("call-1"),
      Some(json!({ "chars": "later" })),
      None,
    );

    let mut assembler = ToolOperationAssembler::new();
    for (index, event) in [first, second, ambiguous_result, later_invocation].iter().enumerate() {
      assembler.ingest(index, event);
    }

    assert_eq!(assembler.operations().len(), 4);
    assert_eq!(assembler.active.values().map(Vec::len).sum::<usize>(), 2);
  }

  #[test]
  fn snapshots_update_one_live_operation() {
    let mut started_event = tool(
      ToolRecordKind::Snapshot,
      Some("call-1"),
      Some(json!({ "chars": "ping" })),
      None,
    );
    let AgentEvent::ToolCall(started_tool) = &mut started_event else {
      unreachable!();
    };
    started_tool.phase = Phase::Started;

    let mut updated_event = tool(
      ToolRecordKind::Snapshot,
      Some("call-1"),
      None,
      Some(json!({ "text": "partial" })),
    );
    let AgentEvent::ToolCall(updated_tool) = &mut updated_event else {
      unreachable!();
    };
    updated_tool.phase = Phase::Updated;

    let events = vec![
      started_event,
      updated_event,
      tool(
        ToolRecordKind::Snapshot,
        Some("call-1"),
        None,
        Some(json!({ "text": "done" })),
      ),
    ];

    let operations = assemble_tool_operations(&events);
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].source_event_indices, vec![0, 1, 2]);
    assert!(matches!(operations[0].status, ToolOperationStatus::Completed));
    assert_eq!(operations[0].output.as_ref().unwrap()["text"], "done");
  }

  #[test]
  fn live_ingest_reports_new_then_updated_operation() {
    let invocation = tool(
      ToolRecordKind::Invocation,
      Some("call-1"),
      Some(json!({ "chars": "x" })),
      None,
    );
    let result = tool(
      ToolRecordKind::Result,
      Some("call-1"),
      None,
      Some(json!({ "text": "ok" })),
    );
    let mut assembler = ToolOperationAssembler::new();

    let first = assembler.ingest(4, &invocation).expect("tool event");
    let second = assembler.ingest(5, &result).expect("tool event");
    assert!(first.created);
    assert!(!second.created);
    assert_eq!(first.operation_index, second.operation_index);
    assert_eq!(assembler.operations()[0].source_event_indices, vec![4, 5]);
  }

  #[test]
  fn source_keys_keep_stream_positions_when_provider_ids_are_missing() {
    let events = vec![tool(ToolRecordKind::Invocation, None, None, None)];
    let operations = assemble_tool_operations(&events);
    let key = &operations[0].source_event_keys[0];
    assert_eq!(key.source_event_index, 0);
    assert_eq!(key.tool_call_id, None);
  }

  #[test]
  fn result_summary_adds_exit_code_without_erasing_invocation_context() {
    let mut invocation = tool(
      ToolRecordKind::Invocation,
      Some("call-1"),
      Some(json!({ "cmd": "rg TODO", "cwd": "/repo" })),
      None,
    );
    let AgentEvent::ToolCall(invocation_tool) = &mut invocation else {
      unreachable!();
    };
    invocation_tool.tool_name = Some("exec_command".to_string());
    invocation_tool.tool_kind = ToolKind::Shell;
    invocation_tool.summary = Some(ToolSummary::Shell {
      command: Some("rg TODO".to_string()),
      cwd: Some("/repo".to_string()),
      exit_code: None,
    });

    let mut result = tool(
      ToolRecordKind::Result,
      Some("call-1"),
      None,
      Some(json!({ "exit_code": 17 })),
    );
    let AgentEvent::ToolCall(result_tool) = &mut result else {
      unreachable!();
    };
    result_tool.tool_name = Some("exec_command".to_string());
    result_tool.tool_kind = ToolKind::Shell;
    result_tool.summary = Some(ToolSummary::Shell {
      command: None,
      cwd: None,
      exit_code: Some(17),
    });

    let operations = assemble_tool_operations(&[invocation, result]);
    assert_eq!(operations.len(), 1);
    assert!(matches!(
      operations[0].summary,
      Some(ToolSummary::Shell {
        command: Some(ref command),
        cwd: Some(ref cwd),
        exit_code: Some(17),
      }) if command == "rg TODO" && cwd == "/repo"
    ));
  }

  #[test]
  fn terminal_snapshot_releases_a_reused_call_id() {
    let finished_snapshot = tool(
      ToolRecordKind::Snapshot,
      Some("call-1"),
      Some(json!({ "chars": "first" })),
      Some(json!({ "text": "first result" })),
    );
    let next_invocation = tool(
      ToolRecordKind::Invocation,
      Some("call-1"),
      Some(json!({ "chars": "second" })),
      None,
    );
    let next_result = tool(
      ToolRecordKind::Result,
      Some("call-1"),
      None,
      Some(json!({ "text": "second result" })),
    );

    let operations = assemble_tool_operations(&[finished_snapshot, next_invocation, next_result]);
    assert_eq!(operations.len(), 2);
    assert_eq!(operations[0].source_event_indices, vec![0]);
    assert_eq!(operations[1].source_event_indices, vec![1, 2]);
    assert!(matches!(operations[0].status, ToolOperationStatus::Completed));
    assert!(matches!(operations[1].status, ToolOperationStatus::Completed));
    assert!(matches!(
      operations[0].id,
      ToolOperationId::Correlated { occurrence: 0, .. }
    ));
    assert!(matches!(
      operations[1].id,
      ToolOperationId::Correlated { occurrence: 1, .. }
    ));
  }
}
