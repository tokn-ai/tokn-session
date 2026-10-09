use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
  SessionStarted(SessionStarted),
  ProviderChanged(ProviderChanged),
  SessionSettingsApplied(SessionSettingsApplied),
  Message(MessageEvent),
  QuestionRequest(QuestionRequestEvent),
  Reasoning(ReasoningEvent),
  GoalUpdated(GoalUpdated),
  AgentActivity(AgentActivity),
  ToolCall(ToolCallEvent),
  Lifecycle(LifecycleEvent),
  Compaction(crate::CompactionEvent),
  Usage(UsageEvent),
  Metadata(MetadataEvent),
  Error(ErrorEvent),
  Unknown(UnknownEvent),
}

impl AgentEvent {
  /// Human-facing consumers honor explicit visibility even when a hidden Pi
  /// extension message has an unsupported shape. Machine export stays lossless.
  pub fn is_hidden(&self) -> bool {
    match self {
      Self::Message(event) => event.provenance.as_ref().and_then(|source| source.display) == Some(false),
      Self::Reasoning(event) => event.provenance.as_ref().and_then(|source| source.display) == Some(false),
      Self::Unknown(event) if matches!(event.provider, Provider::Pi) => event
        .native
        .as_ref()
        .is_some_and(|native| native["type"] == "custom_message" && native["display"] == false),
      _ => false,
    }
  }
}

#[derive(Debug, Serialize, Clone, Deserialize)]
pub struct SessionStarted {
  pub provider: Provider,
  pub session_id: String,
  pub cwd: Option<String>,
  pub timestamp: Option<String>,
}

#[derive(Debug, Serialize, Clone, Deserialize)]
pub struct ProviderChanged {
  pub provider: Provider,
  pub session_id: Option<String>,
  pub native_id: Option<String>,
  pub native_parent_id: Option<String>,
  pub model_provider: Option<String>,
  pub model_id: Option<String>,
  pub thinking_level: Option<String>,
  pub timestamp: Option<String>,
}

#[derive(Debug, Serialize, Clone, Deserialize)]
pub struct SessionSettingsApplied {
  pub provider: Provider,
  pub session_id: Option<String>,
  pub model_provider: Option<String>,
  pub model_id: Option<String>,
  pub service_tier: Option<String>,
  pub cwd: Option<String>,
  pub reasoning_effort: Option<String>,
  pub reasoning_summary: Option<String>,
  pub personality: Option<String>,
  pub collaboration_mode: Option<String>,
  pub approval_policy: Option<String>,
  pub approvals_reviewer: Option<String>,
  pub active_permission_profile_id: Option<String>,
  pub native: Option<Value>,
  pub timestamp: Option<String>,
}

#[derive(Debug, Serialize, Clone, Deserialize)]
pub struct MessageEvent {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub provenance: Option<MessageProvenance>,
  pub provider: Provider,
  pub session_id: Option<String>,
  pub message_id: Option<String>,
  pub parent_id: Option<String>,
  pub role: Role,
  pub delivery: MessageDelivery,
  pub phase: Phase,
  pub text: String,
  pub timestamp: Option<String>,
}

/// A recorded request for user input, not evidence that input is still pending.
#[derive(Debug, Serialize, Clone, Deserialize)]
pub struct QuestionRequestEvent {
  pub provider: Provider,
  pub session_id: Option<String>,
  pub request_id: Option<String>,
  pub turn_id: Option<String>,
  /// None when the historical call lacks the provider's effective mode.
  pub is_blocking: Option<bool>,
  pub phase: Phase,
  pub text: Option<String>,
  pub questions: Vec<UserQuestion>,
  pub native: Value,
  pub timestamp: Option<String>,
}

#[derive(Debug, Serialize, Clone, Deserialize)]
pub struct UserQuestion {
  pub id: Option<String>,
  pub header: Option<String>,
  pub question: String,
  pub options: Option<Vec<UserQuestionOption>>,
  pub allows_free_text: bool,
  pub is_secret: bool,
}

#[derive(Debug, Serialize, Clone, Deserialize)]
pub struct UserQuestionOption {
  pub label: String,
  pub description: Option<String>,
}

#[derive(Debug, Serialize, Clone, Deserialize)]
pub struct ReasoningEvent {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub provenance: Option<MessageProvenance>,
  pub provider: Provider,
  pub session_id: Option<String>,
  pub message_id: Option<String>,
  pub parent_id: Option<String>,
  pub phase: Phase,
  pub text: Option<String>,
  pub summary: Option<String>,
  /// The provider deliberately withheld the reasoning text. This is distinct
  /// from surface visibility: redacted reasoning remains part of the event
  /// stream and can be represented without exposing its content.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub redacted: Option<bool>,
  pub encrypted_content: Option<String>,
  pub signature: Option<String>,
  pub timestamp: Option<String>,
}

#[derive(Debug, Serialize, Clone, Deserialize)]
pub struct GoalUpdated {
  pub provider: Provider,
  pub session_id: Option<String>,
  pub turn_id: Option<String>,
  pub goal: Option<Value>,
  pub timestamp: Option<String>,
}

#[derive(Debug, Serialize, Clone, Deserialize)]
pub struct AgentActivity {
  pub provider: Provider,
  pub session_id: Option<String>,
  pub event_id: Option<String>,
  pub actor_session_id: Option<String>,
  pub actor_agent_path: Option<String>,
  pub target_session_id: Option<String>,
  pub target_agent_path: Option<String>,
  pub kind: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub communication: Option<AgentCommunication>,
  pub occurred_at_ms: Option<u64>,
  pub native: Option<Value>,
  pub timestamp: Option<String>,
}

/// Readable inter-agent delivery content, separate from ordinary assistant replies.
/// Opaque provider content remains available only in the activity's native payload.
#[derive(Debug, Serialize, Clone, Deserialize)]
pub struct AgentCommunication {
  pub text: Option<String>,
  pub has_encrypted_content: bool,
  pub trigger_turn: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCallEvent {
  pub provider: Provider,
  pub session_id: Option<String>,
  /// Provider turn identity when one is available. It scopes a tool call ID
  /// without requiring consumers to infer a turn from nearby messages.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub turn_id: Option<String>,
  pub message_id: Option<String>,
  pub parent_id: Option<String>,
  /// The provider's role for this append-only tool record. `phase` describes
  /// the record's delivery state; it must not be used to guess whether the
  /// logical tool operation has a result yet.
  #[serde(default)]
  pub record_kind: ToolRecordKind,
  pub tool_call_id: Option<String>,
  /// The provider-native tool name before an adapter projects a semantic tool
  /// name. For a direct provider tool this may match `tool_name`.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub provider_tool_name: Option<String>,
  pub tool_name: Option<String>,
  pub tool_kind: ToolKind,
  /// The transport that carried the provider call, when it is distinct from
  /// the semantic tool operation. For example, Codex Code Mode can transport
  /// a `write_stdin` call through an outer JavaScript `exec` call.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub transport: Option<ToolTransport>,
  pub summary: Option<ToolSummary>,
  pub phase: Phase,
  pub input: Option<Value>,
  pub output: Option<Value>,
  pub is_error: Option<bool>,
  /// Lossless provider payload for this record. Semantic input/output may be
  /// projected from it, but consumers can still inspect the original wrapper.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub native: Option<Value>,
  pub timestamp: Option<String>,
}

impl ToolCallEvent {
  /// The provider-facing name, falling back to the semantic name for direct
  /// tool calls that predate explicit provider attribution.
  pub fn effective_provider_tool_name(&self) -> Option<&str> {
    self.provider_tool_name.as_deref().or(self.tool_name.as_deref())
  }
}

#[derive(Debug, Serialize, Clone, Deserialize)]
pub struct ErrorEvent {
  pub provider: Provider,
  pub session_id: Option<String>,
  pub message: String,
  pub timestamp: Option<String>,
}

/// Provider-native attribution and surface edits, not extra conversation text.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageProvenance {
  pub source: Value,
  /// Explicit provider visibility; absent means visible. JSONL retains content.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub display: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub native: Option<Value>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub surface_op: Option<Value>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub source_event_seqs: Option<Vec<u64>>,
}

#[derive(Debug, Serialize, Clone, Deserialize)]
pub struct LifecycleEvent {
  pub provider: Provider,
  pub session_id: Option<String>,
  pub turn_id: String,
  pub step_id: Option<String>,
  pub scope: LifecycleScope,
  pub phase: Phase,
  /// Closing a step alone does not imply success; absent means unspecified.
  pub outcome: Option<LifecycleOutcome>,
  pub native: Value,
  pub timestamp: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleScope {
  Turn,
  Step,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleOutcome {
  Completed,
  Cancelled,
  Interrupted,
  Blocked,
  Failed,
  TokenLimit,
}

/// Accounting scope is explicit: session snapshots replace rather than add.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageKind {
  ModelCall,
  OperationTotal,
  SessionSnapshot,
}

#[derive(Debug, Serialize, Clone, Deserialize)]
pub struct UsageEvent {
  pub kind: UsageKind,
  pub provider: Provider,
  pub session_id: Option<String>,
  pub turn_id: Option<String>,
  pub step_id: Option<String>,
  pub message_id: Option<String>,
  /// Provider record identity when available, including non-message operations.
  pub record_id: Option<String>,
  /// Total input, including cache reads and writes. Cache fields are subsets.
  pub input_tokens: u64,
  pub output_tokens: u64,
  /// Total when known; native estimates need not equal the sum of the counters.
  pub total_tokens: Option<u64>,
  pub cache_read_tokens: Option<u64>,
  pub cache_write_tokens: Option<u64>,
  /// Provider-reported reasoning count; do not add it to output_tokens.
  pub reasoning_tokens: Option<u64>,
  /// Original usage object (not a duplicate of the entire assistant message).
  pub native: Value,
  pub timestamp: Option<String>,
}

/// Recognized non-conversation records. Unknown is reserved for unsupported or
/// malformed shapes; metadata must only be emitted after shape validation.
#[derive(Debug, Serialize, Clone, Deserialize)]
pub struct MetadataEvent {
  pub provider: Provider,
  pub session_id: Option<String>,
  pub kind: MetadataKind,
  pub native_type: String,
  pub summary: String,
  pub native: Value,
  pub timestamp: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataKind {
  Session,
  Configuration,
  Context,
  Queue,
  Diagnostic,
  Stream,
}

#[derive(Debug, Serialize, Clone, Deserialize)]
pub struct UnknownEvent {
  pub provider: Provider,
  pub session_id: Option<String>,
  pub native_type: Option<String>,
  pub native: Option<Value>,
  pub timestamp: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
  Dsh,
  Pi,
  Codex,
  #[serde(rename = "opencode")]
  OpenCode,
  #[serde(rename = "zcode")]
  ZCode,
  #[serde(rename = "workbuddy")]
  WorkBuddy,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum Role {
  User,
  Assistant,
  System,
  Tool,
  Unknown,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageDelivery {
  Commentary,
  Final,
  Unspecified,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum Phase {
  Started,
  Delta,
  Updated,
  Finished,
}

/// The role of one provider record within a logical tool operation.
///
/// Providers may persist an invocation and its result separately. Keeping the
/// role explicit lets live consumers update one operation without pretending
/// that an invocation record already contains its later result.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRecordKind {
  #[default]
  Invocation,
  Progress,
  Result,
  /// A provider emits the current state of a tool operation rather than a
  /// distinct invocation or result record.
  Snapshot,
}

/// How a provider transported a semantic tool operation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTransport {
  /// The provider recorded the semantic tool call directly.
  Native,
  /// The semantic call was encoded in a code-execution wrapper.
  CodeExecution,
  /// The semantic call crossed an adapter, bridge, or proxy boundary.
  Proxy,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
  /// A code cell, script, or similar execution wrapper.
  CodeExecution,
  Shell,
  /// Interactive terminal control such as writing to an existing process.
  Terminal,
  FileRead,
  FileWrite,
  FileEdit,
  Search,
  Web,
  Task,
  Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolSummary {
  CodeExecution {
    language: Option<String>,
  },
  Shell {
    command: Option<String>,
    cwd: Option<String>,
    exit_code: Option<i64>,
  },
  Terminal {
    session_id: Option<String>,
    action: Option<TerminalAction>,
    chars_len: Option<u64>,
    wait_ms: Option<u64>,
  },
  FileRead {
    path: Option<String>,
  },
  FileWrite {
    path: Option<String>,
    bytes: Option<u64>,
  },
  FileEdit {
    path: Option<String>,
    added: Option<u64>,
    removed: Option<u64>,
  },
  Search {
    query: Option<String>,
  },
  Web {
    url: Option<String>,
  },
  Task {
    title: Option<String>,
  },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalAction {
  Send,
  Wait,
}

pub fn tool_kind_for_name(name: &str) -> ToolKind {
  let normalized = name.rsplit('.').next().unwrap_or(name).to_ascii_lowercase();
  match normalized.as_str() {
    "code_execution" | "code_interpreter" | "script" => ToolKind::CodeExecution,
    "bash" | "exec" | "exec_command" | "local_shell" | "shell" | "terminal" => ToolKind::Shell,
    "write_stdin" | "stdin" => ToolKind::Terminal,
    "ls" | "list_dir" | "list_directory" | "read" | "read_file" | "view" => ToolKind::FileRead,
    "write" | "write_file" | "create_file" => ToolKind::FileWrite,
    "edit" | "apply_patch" | "patch" | "str_replace" => ToolKind::FileEdit,
    "code_search" | "file_search" | "find" | "glob" | "grep" | "rg" | "search" | "tool_search" | "web_search"
    | "websearch" => ToolKind::Search,
    "fetch" | "fetch_content" | "get_search_content" | "open" | "web_fetch" | "webfetch" => ToolKind::Web,
    "agent"
    | "askuserquestion"
    | "enterplanmode"
    | "exitplanmode"
    | "followup_task"
    | "respondtocoordinator"
    | "send_message"
    | "sendmessage"
    | "skill"
    | "spawn_agent"
    | "subagent"
    | "task"
    | "taskoutput"
    | "taskstop"
    | "todo"
    | "todoread"
    | "todowrite"
    | "update_plan"
    | "wait"
    | "wait_agent" => ToolKind::Task,
    _ => ToolKind::Unknown,
  }
}

pub fn tool_kind_for_optional_name(name: Option<&str>) -> ToolKind {
  name.map(tool_kind_for_name).unwrap_or(ToolKind::Unknown)
}

pub fn tool_summary_for_input(name: &str, input: &Value) -> Option<ToolSummary> {
  tool_summary_for_io(Some(name), Some(input), None)
}

pub fn tool_summary_for_io(name: Option<&str>, input: Option<&Value>, output: Option<&Value>) -> Option<ToolSummary> {
  tool_summary_for_kind_io(tool_kind_for_optional_name(name), input, output)
}

/// Build a summary from an explicit semantic tool kind.
///
/// Most providers use their tool name for classification, but adapters may
/// safely project a different semantic operation from an outer transport. For
/// example, a Code Mode `exec` wrapper is code execution, not a shell call.
pub fn tool_summary_for_kind_io(kind: ToolKind, input: Option<&Value>, output: Option<&Value>) -> Option<ToolSummary> {
  match kind {
    ToolKind::CodeExecution => Some(ToolSummary::CodeExecution {
      language: input.and_then(|input| string_field(input, "language")),
    }),
    ToolKind::Shell => Some(ToolSummary::Shell {
      command: input.and_then(shell_command_from_value),
      cwd: input.and_then(|input| string_field(input, "cwd").or_else(|| string_field(input, "workdir"))),
      exit_code: output.and_then(output_exit_code),
    }),
    ToolKind::Terminal => Some(ToolSummary::Terminal {
      session_id: input.and_then(terminal_session_id),
      action: input.and_then(terminal_action),
      chars_len: input.and_then(|input| {
        input
          .get("chars")
          .and_then(Value::as_str)
          .map(|chars| chars.chars().count() as u64)
      }),
      wait_ms: input.and_then(|input| {
        unsigned_integer_field(input, "yield_time_ms").or_else(|| unsigned_integer_field(input, "wait_ms"))
      }),
    }),
    ToolKind::FileRead => Some(ToolSummary::FileRead {
      path: input.and_then(path_field),
    }),
    ToolKind::FileWrite => Some(ToolSummary::FileWrite {
      path: input.and_then(path_field),
      bytes: input.and_then(|input| {
        input
          .get("content")
          .and_then(Value::as_str)
          .map(|content| content.len() as u64)
      }),
    }),
    ToolKind::FileEdit => Some(ToolSummary::FileEdit {
      path: input.and_then(patch_path),
      added: input.and_then(|input| patch_line_count(input, '+')),
      removed: input.and_then(|input| patch_line_count(input, '-')),
    }),
    ToolKind::Search => Some(ToolSummary::Search {
      query: input.and_then(|input| {
        string_field(input, "query")
          .or_else(|| string_field(input, "q"))
          .or_else(|| string_field(input, "pattern"))
          .or_else(|| joined_string_array_field(input, "queries"))
      }),
    }),
    ToolKind::Web => Some(ToolSummary::Web {
      url: input.and_then(|input| string_field(input, "url").or_else(|| string_field(input, "ref_id"))),
    }),
    ToolKind::Task => Some(ToolSummary::Task {
      title: input.and_then(|input| {
        string_field(input, "title")
          .or_else(|| string_field(input, "task_name"))
          .or_else(|| string_field(input, "prompt"))
          .or_else(|| string_field(input, "message"))
          .or_else(|| string_field(input, "step"))
      }),
    }),
    ToolKind::Unknown => None,
  }
}

pub fn patch_summary(value: &Value) -> ToolSummary {
  ToolSummary::FileEdit {
    path: patch_path(value),
    added: patch_line_count(value, '+'),
    removed: patch_line_count(value, '-'),
  }
}

pub fn shell_command_from_value(value: &Value) -> Option<String> {
  string_field(value, "cmd")
    .or_else(|| string_field(value, "command"))
    .or_else(|| string_array_field(value, "command").map(|parts| parts.join(" ")))
}

fn output_exit_code(value: &Value) -> Option<i64> {
  value
    .get("metadata")
    .and_then(|metadata| metadata.get("exit"))
    .and_then(Value::as_i64)
    .or_else(|| value.get("exit_code").and_then(Value::as_i64))
    .or_else(|| value.get("exitCode").and_then(Value::as_i64))
    .or_else(|| value.get("details").and_then(output_exit_code))
}

fn terminal_session_id(value: &Value) -> Option<String> {
  string_field(value, "session_id")
    .or_else(|| unsigned_integer_field(value, "session_id").map(|value| value.to_string()))
}

fn terminal_action(value: &Value) -> Option<TerminalAction> {
  value.get("chars").and_then(Value::as_str).map(|chars| {
    if chars.is_empty() {
      TerminalAction::Wait
    } else {
      TerminalAction::Send
    }
  })
}

fn patch_path(value: &Value) -> Option<String> {
  path_field(value)
    .or_else(|| first_change_map_path(value))
    .or_else(|| value.as_str().and_then(patch_text_path))
    .or_else(|| {
      value
        .as_array()
        .and_then(|changes| changes.first())
        .and_then(path_field)
    })
}

fn patch_line_count(value: &Value, marker: char) -> Option<u64> {
  value.as_str().or_else(|| first_unified_diff(value)).map(|patch| {
    patch
      .lines()
      .filter(|line| {
        line.starts_with(marker) && !line.starts_with("+++") && !line.starts_with("---") && !line.starts_with("***")
      })
      .count() as u64
  })
}

fn first_unified_diff(value: &Value) -> Option<&str> {
  value.as_object().and_then(|changes| {
    changes
      .values()
      .find_map(|change| change.get("unified_diff").and_then(Value::as_str))
  })
}

fn first_change_map_path(value: &Value) -> Option<String> {
  value.as_object().and_then(|changes| {
    changes.iter().find_map(|(path, change)| {
      let change = change.as_object()?;
      let has_unified_diff = change.get("unified_diff").is_some_and(Value::is_string);
      let has_known_type = matches!(
        change.get("type").and_then(Value::as_str),
        Some("add" | "delete" | "update")
      );
      (has_unified_diff || has_known_type).then(|| path.clone())
    })
  })
}

fn patch_text_path(patch: &str) -> Option<String> {
  patch.lines().find_map(|line| {
    line
      .strip_prefix("*** Update File: ")
      .or_else(|| line.strip_prefix("*** Add File: "))
      .or_else(|| line.strip_prefix("*** Delete File: "))
      .map(str::to_string)
  })
}

fn path_field(value: &Value) -> Option<String> {
  string_field(value, "path")
    .or_else(|| string_field(value, "file_path"))
    .or_else(|| string_field(value, "filepath"))
    .or_else(|| string_field(value, "file"))
}

fn string_field(value: &Value, field: &str) -> Option<String> {
  value.get(field).and_then(Value::as_str).map(str::to_string)
}

fn unsigned_integer_field(value: &Value, field: &str) -> Option<u64> {
  value.get(field).and_then(Value::as_u64)
}

fn string_array_field(value: &Value, field: &str) -> Option<Vec<String>> {
  value.get(field).and_then(Value::as_array).map(|items| {
    items
      .iter()
      .filter_map(Value::as_str)
      .map(str::to_string)
      .collect::<Vec<_>>()
  })
}

fn joined_string_array_field(value: &Value, field: &str) -> Option<String> {
  string_array_field(value, field).and_then(|items| (!items.is_empty()).then(|| items.join(", ")))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn redacted_reasoning_remains_visible() {
    let event = AgentEvent::Reasoning(ReasoningEvent {
      provenance: None,
      provider: Provider::Pi,
      session_id: Some("session-1".to_string()),
      message_id: Some("message-1".to_string()),
      parent_id: None,
      phase: Phase::Finished,
      text: None,
      summary: None,
      redacted: Some(true),
      encrypted_content: None,
      signature: None,
      timestamp: None,
    });

    assert!(!event.is_hidden());
    let serialized = serde_json::to_value(event).expect("reasoning event should serialize");
    assert_eq!(serialized["redacted"], true);
  }

  #[test]
  fn classifies_known_tool_families_without_treating_user_input_as_a_task() {
    for name in ["ls", "list_dir", "list_directory"] {
      assert!(matches!(tool_kind_for_name(name), ToolKind::FileRead));
    }
    for name in ["code_search", "file_search", "glob", "tool_search"] {
      assert!(matches!(tool_kind_for_name(name), ToolKind::Search));
    }
    for name in ["fetch_content", "get_search_content", "web_fetch"] {
      assert!(matches!(tool_kind_for_name(name), ToolKind::Web));
    }
    for name in [
      "subagent",
      "spawn_agent",
      "followup_task",
      "send_message",
      "wait",
      "wait_agent",
    ] {
      assert!(matches!(tool_kind_for_name(name), ToolKind::Task));
    }

    assert!(matches!(tool_kind_for_name("ask_user_question"), ToolKind::Unknown));
  }

  #[test]
  fn classifies_zcode_tool_spellings() {
    for name in ["WebSearch"] {
      assert!(matches!(tool_kind_for_name(name), ToolKind::Search));
    }
    for name in ["WebFetch"] {
      assert!(matches!(tool_kind_for_name(name), ToolKind::Web));
    }
    for name in ["Agent", "AskUserQuestion", "TodoRead", "TodoWrite"] {
      assert!(matches!(tool_kind_for_name(name), ToolKind::Task));
    }
  }

  #[test]
  fn summarizes_multi_query_searches_and_file_write_bytes() {
    let search = serde_json::json!({ "queries": ["alpha", "beta"] });
    assert!(matches!(
      tool_summary_for_input("code_search", &search),
      Some(ToolSummary::Search { query: Some(query) }) if query == "alpha, beta"
    ));

    let write = serde_json::json!({ "path": "notes.txt", "content": "hello 🦀" });
    assert!(matches!(
      tool_summary_for_input("write", &write),
      Some(ToolSummary::FileWrite {
        path: Some(path),
        bytes: Some(10),
      }) if path == "notes.txt"
    ));
  }

  #[test]
  fn summarizes_collaboration_tasks_from_provider_specific_fields() {
    for (name, input, expected) in [
      (
        "spawn_agent",
        serde_json::json!({ "task_name": "reviewer" }),
        "reviewer",
      ),
      (
        "subagent",
        serde_json::json!({ "prompt": "Review the diff" }),
        "Review the diff",
      ),
      (
        "send_message",
        serde_json::json!({ "message": "Please retry" }),
        "Please retry",
      ),
      ("update_plan", serde_json::json!({ "step": "Run tests" }), "Run tests"),
    ] {
      assert!(matches!(
        tool_summary_for_input(name, &input),
        Some(ToolSummary::Task { title: Some(title) }) if title == expected
      ));
    }
  }

  #[test]
  fn shell_result_summary_reads_exit_metadata_through_a_provider_wrapper() {
    let output = serde_json::json!({
      "content": [{ "type": "text", "text": "failed" }],
      "details": { "metadata": { "exit": 2 } },
    });

    assert!(matches!(
      tool_summary_for_io(Some("bash"), None, Some(&output)),
      Some(ToolSummary::Shell {
        command: None,
        cwd: None,
        exit_code: Some(2),
      })
    ));
  }

  #[test]
  fn explicit_code_execution_kind_does_not_inherit_exec_shell_classification() {
    let input = serde_json::json!({ "language": "javascript", "source": "await tools.write_stdin(...)" });

    assert!(matches!(
      tool_summary_for_kind_io(ToolKind::CodeExecution, Some(&input), None),
      Some(ToolSummary::CodeExecution {
        language: Some(language),
      }) if language == "javascript"
    ));
  }

  #[test]
  fn file_edit_summary_prefers_an_explicit_path_over_edit_fields() {
    let input = serde_json::json!({
      "oldText": "before",
      "newText": "after",
      "path": "src/lib.rs",
    });

    assert!(matches!(
      tool_summary_for_input("edit", &input),
      Some(ToolSummary::FileEdit {
        path: Some(path),
        added: None,
        removed: None,
      }) if path == "src/lib.rs"
    ));
  }

  #[test]
  fn file_edit_summary_keeps_change_map_path_and_counts() {
    let input = serde_json::json!({
      "src/main.rs": {
        "unified_diff": "@@ -1 +1 @@\n-before\n+after\n",
      },
    });

    assert!(matches!(
      tool_summary_for_input("apply_patch", &input),
      Some(ToolSummary::FileEdit {
        path: Some(path),
        added: Some(1),
        removed: Some(1),
      }) if path == "src/main.rs"
    ));

    let add = serde_json::json!({
      "src/new.rs": {
        "type": "add",
        "content": "fn main() {}\n",
      },
    });
    assert!(matches!(
      tool_summary_for_input("apply_patch", &add),
      Some(ToolSummary::FileEdit {
        path: Some(path),
        ..
      }) if path == "src/new.rs"
    ));
  }
}
