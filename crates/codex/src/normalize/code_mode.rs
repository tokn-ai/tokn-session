//! Conservative decoding for Codex Code Mode's generated tool wrappers.
//!
//! `custom_tool_call` records named `exec` are JavaScript programs, not
//! necessarily shell commands.  We only project the small, generated shape
//! below.  Everything else deliberately remains an opaque Code Mode call:
//!
//! ```text
//! const r = await tools.write_stdin({ ... });
//! text(JSON.stringify(r));
//! ```
//!
//! The parser never evaluates JavaScript.  In particular, calls with dynamic
//! arguments, extra statements, or multiple tool calls cannot match.

use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CodeModeTool {
  WriteStdin,
  ExecCommand,
}

impl CodeModeTool {
  pub(crate) fn name(self) -> &'static str {
    match self {
      Self::WriteStdin => "write_stdin",
      Self::ExecCommand => "exec_command",
    }
  }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DecodedCodeModeCall {
  pub(crate) tool: CodeModeTool,
  pub(crate) input: Value,
}

/// Decode one generated Code Mode invocation without interpreting arbitrary
/// JavaScript.  The only accepted argument is a literal JSON object.
pub(crate) fn decode_call(input: &Value) -> Option<DecodedCodeModeCall> {
  let source = input.as_str()?;
  let prefix = "const r = await tools.";
  let remainder = source.strip_prefix(prefix)?;

  let (tool, remainder) = if let Some(remainder) = remainder.strip_prefix("write_stdin(") {
    (CodeModeTool::WriteStdin, remainder)
  } else if let Some(remainder) = remainder.strip_prefix("exec_command(") {
    (CodeModeTool::ExecCommand, remainder)
  } else {
    return None;
  };

  let (arguments, suffix) = split_literal_json_object(remainder)?;
  if !matches!(suffix, ");\ntext(JSON.stringify(r));\n" | ");\ntext(r);\n") {
    return None;
  }

  let input = parse_literal_object(arguments)?;
  input.is_object().then_some(DecodedCodeModeCall { tool, input })
}

/// A goal status can only come from the nested tool itself. Accept the literal
/// one-call wrapper and reject any script that could print its own response.
pub(super) fn completes_goal(input: &Value) -> bool {
  let Some(source) = input.as_str().map(str::trim) else {
    return false;
  };
  let source = source.strip_suffix(';').unwrap_or(source).trim_end();
  let Some(arguments) = source
    .strip_prefix("text(await tools.update_goal(")
    .and_then(|s| s.strip_suffix("))"))
  else {
    return false;
  };
  parse_literal_object(arguments.trim()).is_some_and(|value| value == serde_json::json!({"status":"complete"}))
}

/// Decode the executor's structured result for an already-recognized Code
/// Mode call.  This is intentionally coupled to `decode_call`: a random
/// custom-tool result containing JSON must not be reinterpreted this way.
///
/// The executor writes diagnostics such as `Script completed` separately from
/// the nested tool result.  The semantic result keeps stable response metadata
/// but exposes its useful payload as `text`, while the caller retains the full
/// provider envelope in `native`.
pub(crate) fn decode_output(tool: CodeModeTool, output: &Value) -> Option<Value> {
  let parts = output.as_array()?;
  if !parts.iter().any(is_code_mode_executor_preamble) {
    return None;
  }

  let response = parts
    .iter()
    .rev()
    .filter_map(content_text)
    .find_map(json_object_at_end)?;
  let text = response.get("output")?.as_str()?;

  if matches!(tool, CodeModeTool::WriteStdin) && !response.contains_key("session_id") {
    return None;
  }

  let mut semantic = Map::new();
  for key in [
    "session_id",
    "chunk_id",
    "exit_code",
    "wall_time_seconds",
    "original_token_count",
  ] {
    if let Some(value) = response.get(key) {
      semantic.insert(key.to_string(), value.clone());
    }
  }
  semantic.insert("text".to_string(), Value::String(text.to_string()));
  Some(Value::Object(semantic))
}

fn split_literal_json_object(source: &str) -> Option<(&str, &str)> {
  let bytes = source.as_bytes();
  if bytes.first().copied() != Some(b'{') {
    return None;
  }

  let mut depth = 0_u32;
  let mut in_string = false;
  let mut escaped = false;

  for (index, byte) in bytes.iter().copied().enumerate() {
    if in_string {
      if escaped {
        escaped = false;
      } else if byte == b'\\' {
        escaped = true;
      } else if byte == b'"' {
        in_string = false;
      }
      continue;
    }

    match byte {
      b'"' => in_string = true,
      b'{' => depth += 1,
      b'}' => {
        depth = depth.checked_sub(1)?;
        if depth == 0 {
          return Some((&source[..=index], &source[index + 1..]));
        }
      }
      _ => {}
    }
  }

  None
}

/// Code Mode currently emits both JSON and JavaScript object-literal argument
/// styles.  A literal with bare property names is still safe to decode after
/// quoting those names; values remain strict JSON and must pass serde's JSON
/// parser.  This is deliberately not a JavaScript parser or evaluator.
fn parse_literal_object(arguments: &str) -> Option<Value> {
  serde_json::from_str(arguments)
    .ok()
    .or_else(|| serde_json::from_str(&quote_bare_object_keys(arguments)).ok())
    .filter(Value::is_object)
}

fn quote_bare_object_keys(source: &str) -> String {
  let bytes = source.as_bytes();
  let mut output = Vec::with_capacity(source.len());
  let mut index = 0;
  let mut in_string = false;
  let mut escaped = false;

  while index < bytes.len() {
    let byte = bytes[index];
    if in_string {
      output.push(byte);
      if escaped {
        escaped = false;
      } else if byte == b'\\' {
        escaped = true;
      } else if byte == b'"' {
        in_string = false;
      }
      index += 1;
      continue;
    }

    if byte == b'"' {
      in_string = true;
      output.push(b'"');
      index += 1;
      continue;
    }

    if matches!(byte, b'{' | b',') {
      output.push(byte);
      index += 1;

      let whitespace_start = index;
      while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
      }
      output.extend_from_slice(&bytes[whitespace_start..index]);

      if index < bytes.len() && is_identifier_start(bytes[index]) {
        let key_start = index;
        index += 1;
        while index < bytes.len() && is_identifier_continue(bytes[index]) {
          index += 1;
        }
        let key_end = index;

        let mut colon = index;
        while colon < bytes.len() && bytes[colon].is_ascii_whitespace() {
          colon += 1;
        }
        if bytes.get(colon) == Some(&b':') {
          output.push(b'"');
          output.extend_from_slice(&bytes[key_start..key_end]);
          output.push(b'"');
          output.extend_from_slice(&bytes[key_end..colon]);
          index = colon;
          continue;
        }

        output.extend_from_slice(&bytes[key_start..key_end]);
        continue;
      }

      continue;
    }

    output.push(byte);
    index += 1;
  }

  String::from_utf8(output).expect("source was valid UTF-8 and transformations are ASCII-only")
}

fn is_identifier_start(byte: u8) -> bool {
  byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$')
}

fn is_identifier_continue(byte: u8) -> bool {
  is_identifier_start(byte) || byte.is_ascii_digit()
}

fn is_code_mode_executor_preamble(part: &Value) -> bool {
  content_text(part).is_some_and(|text| {
    (text.starts_with("Script completed\n") || text.starts_with("Script failed\n")) && text.contains("\nOutput:\n")
  })
}

fn content_text(part: &Value) -> Option<&str> {
  part
    .get("type")
    .and_then(Value::as_str)
    .filter(|kind| *kind == "input_text")?;
  part.get("text").and_then(Value::as_str)
}

/// Find a complete JSON object at the end of a text part.  The executor may
/// prefix it with truncation diagnostics, so parsing the complete text is too
/// strict; parsing anything other than the final object would be too loose.
fn json_object_at_end(text: &str) -> Option<Map<String, Value>> {
  let trimmed = text.trim_end();
  for (offset, _) in trimmed.match_indices('{').rev() {
    let Ok(value) = serde_json::from_str::<Value>(&trimmed[offset..]) else {
      continue;
    };
    if let Value::Object(object) = value {
      return Some(object);
    }
  }
  None
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::{CodeModeTool, decode_call, decode_output};

  #[test]
  fn decodes_strict_write_stdin_wrapper() {
    let call = decode_call(&json!(
      "const r = await tools.write_stdin({session_id: 90855, chars: \"\", yield_time_ms: 30000, max_output_tokens: 4000});\ntext(JSON.stringify(r));\n"
    ))
    .expect("generated wrapper should decode");

    assert_eq!(call.tool, CodeModeTool::WriteStdin);
    assert_eq!(
      call.input,
      json!({
        "session_id": 90855,
        "chars": "",
        "yield_time_ms": 30000,
        "max_output_tokens": 4000,
      })
    );
  }

  #[test]
  fn decodes_known_text_r_wrapper_for_backward_compatibility() {
    let call = decode_call(&json!(
      "const r = await tools.exec_command({\"cmd\":\"cargo test\",\"yield_time_ms\":30000});\ntext(r);\n"
    ))
    .expect("generated wrapper should decode");

    assert_eq!(call.tool, CodeModeTool::ExecCommand);
    assert_eq!(call.input, json!({"cmd": "cargo test", "yield_time_ms": 30000}));
  }

  #[test]
  fn preserves_dynamic_and_multi_call_programs_as_opaque() {
    for source in [
      "const r = await tools.write_stdin({session_id: process.pid, chars: \"x\"});\ntext(JSON.stringify(r));\n",
      "const r = await tools.write_stdin({\"session_id\":1,\"chars\":\"x\"});\nawait tools.exec_command({\"cmd\":\"pwd\"});\ntext(JSON.stringify(r));\n",
      "const r = await tools.write_stdin({\"session_id\":1,\"chars\":\"x\"});\ntext(JSON.stringify({ r }));\n",
      "await tools.write_stdin({\"session_id\":1,\"chars\":\"x\"});\n",
    ] {
      assert!(decode_call(&json!(source)).is_none(), "unexpected decode: {source}");
    }
  }

  #[test]
  fn decodes_nested_executor_result_and_drops_executor_noise() {
    let output = json!([
      {"type": "input_text", "text": "Script completed\nWall time 30.0 seconds\nOutput:\n"},
      {"type": "input_text", "text": "Warning: truncated output\n\n{\"chunk_id\":\"842651\",\"wall_time_seconds\":30.001430708,\"session_id\":90855,\"original_token_count\":179,\"output\":\"Refreshing checks status\"}"},
    ]);

    assert_eq!(
      decode_output(CodeModeTool::WriteStdin, &output),
      Some(json!({
        "session_id": 90855,
        "chunk_id": "842651",
        "wall_time_seconds": 30.001430708,
        "original_token_count": 179,
        "text": "Refreshing checks status",
      }))
    );
  }

  #[test]
  fn refuses_unrelated_or_ambiguous_result_payloads() {
    let no_preamble = json!([
      {"type": "input_text", "text": "{\"session_id\":1,\"output\":\"not executor output\"}"}
    ]);
    assert!(decode_output(CodeModeTool::WriteStdin, &no_preamble).is_none());

    let no_session_id = json!([
      {"type": "input_text", "text": "Script completed\nWall time 0.0 seconds\nOutput:\n"},
      {"type": "input_text", "text": "{\"output\":\"missing identity\"}"},
    ]);
    assert!(decode_output(CodeModeTool::WriteStdin, &no_session_id).is_none());

    let terminal_json = json!([
      {"type": "input_text", "text": "Script completed\nWall time 0.0 seconds\nOutput:\n"},
      {"type": "input_text", "text": "a command printed {\"session_id\":1,\"output\":\"fake\"} before its actual text"},
    ]);
    assert!(decode_output(CodeModeTool::WriteStdin, &terminal_json).is_none());
  }
}
