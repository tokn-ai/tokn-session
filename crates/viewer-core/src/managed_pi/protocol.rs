use std::path::PathBuf;

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, BufReader};

use crate::workspace::ManagedSessionDialog;

/// Maximum bytes before LF, including an optional CR. A 24 MiB image batch
/// becomes 32 MiB of base64; leave bounded room for its text and JSON envelope.
pub(super) const MAX_FRAME_BYTES: usize = 34 * 1024 * 1024;
const MAX_DIALOG_TEXT_BYTES: usize = 16 * 1024;
const MAX_DIALOG_OPTIONS: usize = 128;
const MAX_ID_BYTES: usize = 256;
const STDERR_TAIL_BYTES: usize = 16 * 1024;

/// Retain this reader for the process lifetime so bytes after an LF stay buffered.
pub(super) struct RpcReader<R> {
  reader: BufReader<R>,
}

impl<R: AsyncRead + Unpin> RpcReader<R> {
  pub(super) fn new(reader: R) -> Self {
    Self {
      reader: BufReader::new(reader),
    }
  }

  pub(super) async fn read_frame(&mut self) -> Result<Option<Value>, String> {
    read_frame(&mut self.reader).await
  }
}

/// Clean EOF is distinct from a truncated frame. Invalid frames are terminal:
/// callers must stop the process rather than try to recover delimiter alignment.
pub(super) async fn read_frame(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Option<Value>, String> {
  let mut bytes = Vec::new();
  loop {
    let (consumed, complete) = {
      let available = reader
        .fill_buf()
        .await
        .map_err(|error| format!("failed to read Pi RPC stdout: {error}"))?;
      if available.is_empty() {
        return if bytes.is_empty() {
          Ok(None)
        } else {
          Err("Pi RPC stdout ended before the LF frame delimiter".to_string())
        };
      }
      let newline = available.iter().position(|byte| *byte == b'\n');
      let payload_length = newline.unwrap_or(available.len());
      if payload_length > MAX_FRAME_BYTES - bytes.len() {
        return Err("Pi RPC frame exceeds the 34 MiB limit".to_string());
      }
      bytes.extend_from_slice(&available[..payload_length]);
      (payload_length + usize::from(newline.is_some()), newline.is_some())
    };
    reader.consume(consumed);
    if complete {
      if bytes.last() == Some(&b'\r') {
        bytes.pop();
      }
      return serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("invalid Pi RPC JSON frame: {error}"));
    }
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RpcState {
  pub(super) session_id: String,
  pub(super) session_file: Option<PathBuf>,
  pub(super) is_streaming: bool,
  pub(super) is_compacting: bool,
  pub(super) pending_message_count: u64,
  pub(super) images_available: bool,
}

/// Parse the `data` object of a successful native `get_state` response.
/// The managed owner validates the returned history path before trusting it.
pub(super) fn parse_state(data: &Value) -> Result<RpcState, String> {
  if !data.is_object() {
    return Err("Pi RPC state must be an object".to_string());
  }
  let session_id = identity(data, "sessionId")?.to_string();
  let session_file = match data.get("sessionFile") {
    None | Some(Value::Null) => None,
    Some(Value::String(path)) if !path.trim().is_empty() && !path.contains('\0') => Some(PathBuf::from(path)),
    _ => return Err("Pi RPC state has an invalid sessionFile".to_string()),
  };
  Ok(RpcState {
    session_id,
    session_file,
    is_streaming: boolean(data, "isStreaming")?,
    is_compacting: boolean(data, "isCompacting")?,
    images_available: data
      .get("model")
      .and_then(|model| model.get("input"))
      .and_then(Value::as_array)
      .is_some_and(|input| input.iter().any(|value| value.as_str() == Some("image"))),
    pending_message_count: data
      .get("pendingMessageCount")
      .and_then(Value::as_u64)
      .ok_or_else(|| "Pi RPC state has an invalid pendingMessageCount".to_string())?,
  })
}

fn identity<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
  let identity = value
    .get(field)
    .and_then(Value::as_str)
    .ok_or_else(|| format!("Pi RPC has an invalid {field}"))?;
  if identity.is_empty()
    || identity.len() > MAX_ID_BYTES
    || identity.trim() != identity
    || identity.chars().any(char::is_control)
  {
    return Err(format!("Pi RPC has an invalid {field}"));
  }
  Ok(identity)
}

fn boolean(value: &Value, field: &str) -> Result<bool, String> {
  value
    .get(field)
    .and_then(Value::as_bool)
    .ok_or_else(|| format!("Pi RPC state has an invalid {field}"))
}

pub(super) fn is_cosmetic_extension_method(method: &str) -> bool {
  matches!(
    method,
    "notify" | "setStatus" | "setWidget" | "setTitle" | "set_editor_text"
  )
}

/// Unknown/malformed interactive requests return a reason for explicit native
/// cancellation. The caller retains the raw request to recover its native ID.
pub(super) fn parse_extension_dialog(value: &Value) -> Result<Option<ManagedSessionDialog>, String> {
  if value.get("type").and_then(Value::as_str) != Some("extension_ui_request") {
    return Err("invalid Pi extension UI request type".to_string());
  }
  let method = value
    .get("method")
    .and_then(Value::as_str)
    .ok_or_else(|| "Pi extension UI request has no method".to_string())?;
  if is_cosmetic_extension_method(method) {
    return Ok(None);
  }
  if !matches!(method, "confirm" | "select" | "input" | "editor") {
    return Err("unsupported Pi extension dialog method".to_string());
  }
  let dialog_id = identity(value, "id")?.to_string();
  let title = dialog_text(value, "title", true)?.unwrap();
  let mut dialog = ManagedSessionDialog {
    dialog_id,
    kind: method.to_string(),
    title,
    message: None,
    options: Vec::new(),
    initial_value: None,
  };
  match method {
    "confirm" => dialog.message = dialog_text(value, "message", true)?,
    "select" => {
      let options = value
        .get("options")
        .and_then(Value::as_array)
        .ok_or_else(|| "Pi select dialog has invalid options".to_string())?;
      if options.len() > MAX_DIALOG_OPTIONS {
        return Err("Pi select dialog exceeds the 128 option limit".to_string());
      }
      dialog.options = options
        .iter()
        .map(|option| {
          option
            .as_str()
            .filter(|text| text.len() <= MAX_DIALOG_TEXT_BYTES)
            .map(str::to_string)
            .ok_or_else(|| "Pi select dialog has an invalid or oversized option".to_string())
        })
        .collect::<Result<_, _>>()?;
    }
    // A placeholder is a hint, never text to submit as the initial value.
    "input" => dialog.message = dialog_text(value, "placeholder", false)?,
    "editor" => dialog.initial_value = dialog_text(value, "prefill", false)?,
    _ => unreachable!(),
  }
  Ok(Some(dialog))
}

fn dialog_text(value: &Value, field: &str, required: bool) -> Result<Option<String>, String> {
  match value.get(field) {
    None if !required => Ok(None),
    Some(Value::String(text)) if text.len() <= MAX_DIALOG_TEXT_BYTES => Ok(Some(text.clone())),
    _ => Err(format!("Pi extension dialog has an invalid or oversized {field}")),
  }
}

/// Bounded diagnostic bytes, independent of stderr line lengths or UTF-8 splits.
#[derive(Default)]
pub(super) struct StderrTail {
  bytes: Vec<u8>,
}

impl StderrTail {
  pub(super) fn push(&mut self, bytes: &[u8]) {
    if bytes.len() >= STDERR_TAIL_BYTES {
      self.bytes.clear();
      self.bytes.extend_from_slice(&bytes[bytes.len() - STDERR_TAIL_BYTES..]);
    } else {
      let overflow = (self.bytes.len() + bytes.len()).saturating_sub(STDERR_TAIL_BYTES);
      self.bytes.drain(..overflow);
      self.bytes.extend_from_slice(bytes);
    }
  }

  pub(super) fn text(&self) -> String {
    String::from_utf8_lossy(&self.bytes).into_owned()
  }
}

#[cfg(test)]
mod tests {
  use serde_json::json;
  use tokio::io::AsyncWriteExt;

  use super::*;

  #[tokio::test]
  async fn reads_lf_crlf_and_multiple_buffered_frames_without_losing_bytes() {
    let bytes = b"{\"type\":\"event\"}\n[1,2]\r\nnull\n";
    let mut reader = RpcReader::new(bytes.as_slice());
    assert_eq!(reader.read_frame().await.unwrap(), Some(json!({ "type": "event" })));
    assert_eq!(reader.read_frame().await.unwrap(), Some(json!([1, 2])));
    assert_eq!(reader.read_frame().await.unwrap(), Some(Value::Null));
    assert_eq!(reader.read_frame().await.unwrap(), None);
  }

  #[tokio::test]
  async fn assembles_frames_split_across_async_reads() {
    let (mut sender, receiver) = tokio::io::duplex(16);
    let writer = tokio::spawn(async move {
      for chunk in [b"{\"text\":\"".as_slice(), b"hello\\nworld\"}", b"\r", b"\n"] {
        sender.write_all(chunk).await.unwrap();
        tokio::task::yield_now().await;
      }
    });
    let mut reader = RpcReader::new(receiver);
    assert_eq!(
      reader.read_frame().await.unwrap(),
      Some(json!({ "text": "hello\nworld" }))
    );
    assert_eq!(reader.read_frame().await.unwrap(), None);
    writer.await.unwrap();
  }

  #[tokio::test]
  async fn rejects_partial_eof_empty_invalid_json_and_invalid_utf8() {
    for bytes in [b"{}".as_slice(), b"\n", b"not json\n", b"{} trailing\n", b"\xff\n"] {
      assert!(RpcReader::new(bytes).read_frame().await.is_err());
    }
    assert_eq!(RpcReader::new(b"".as_slice()).read_frame().await.unwrap(), None);
  }

  #[tokio::test]
  async fn accepts_exact_frame_limit_and_rejects_larger_frames_without_lf() {
    let mut exact = vec![b' '; MAX_FRAME_BYTES];
    exact[0] = b'0';
    exact.push(b'\n');
    assert_eq!(
      RpcReader::new(exact.as_slice()).read_frame().await.unwrap(),
      Some(json!(0))
    );
    let oversized = vec![b' '; MAX_FRAME_BYTES + 1];
    assert!(
      RpcReader::new(oversized.as_slice())
        .read_frame()
        .await
        .unwrap_err()
        .contains("34 MiB")
    );
  }

  fn state() -> Value {
    json!({
      "sessionId": "synthetic-session",
      "sessionFile": "/tmp/synthetic-session.jsonl",
      "isStreaming": false,
      "isCompacting": true,
      "pendingMessageCount": 2,
      "futureField": true
    })
  }

  #[test]
  fn parses_native_state_with_optional_unpersisted_file() {
    let parsed = parse_state(&state()).unwrap();
    assert_eq!(parsed.session_id, "synthetic-session");
    assert_eq!(parsed.session_file, Some(PathBuf::from("/tmp/synthetic-session.jsonl")));
    assert!(!parsed.is_streaming);
    assert!(parsed.is_compacting);
    assert_eq!(parsed.pending_message_count, 2);
    let mut value = state();
    value.as_object_mut().unwrap().remove("sessionFile");
    assert_eq!(parse_state(&value).unwrap().session_file, None);
    value["sessionFile"] = Value::Null;
    assert_eq!(parse_state(&value).unwrap().session_file, None);
  }

  #[test]
  fn native_model_input_is_required_before_advertising_image_support() {
    let mut value = state();
    assert!(!parse_state(&value).unwrap().images_available);
    for input in [json!(["text"]), json!(null), json!("image")] {
      value["model"] = json!({"input":input});
      assert!(!parse_state(&value).unwrap().images_available);
    }
    value["model"] = json!({"input":["text","image"]});
    assert!(parse_state(&value).unwrap().images_available);
  }

  #[test]
  fn rejects_invalid_state_identity_types_and_counts() {
    for id in [
      json!(null),
      json!(""),
      json!(" "),
      json!(" leading"),
      json!("line\nbreak"),
      json!(4),
    ] {
      let mut value = state();
      value["sessionId"] = id;
      assert!(parse_state(&value).is_err());
    }
    for (field, invalid) in [
      ("isStreaming", json!("false")),
      ("isCompacting", Value::Null),
      ("pendingMessageCount", json!(-1)),
      ("pendingMessageCount", json!(1.5)),
      ("sessionFile", json!("")),
      ("sessionFile", json!(false)),
      ("sessionFile", json!("invalid\0path")),
    ] {
      let mut value = state();
      value[field] = invalid;
      assert!(parse_state(&value).is_err(), "accepted invalid {field}");
    }
    assert!(parse_state(&Value::Null).is_err());
  }

  fn request(method: &str) -> Value {
    json!({ "type": "extension_ui_request", "id": "dialog-1", "method": method, "title": "Continue?" })
  }

  #[test]
  fn converts_confirm_select_input_and_editor_without_turning_hints_into_input() {
    let mut confirm = request("confirm");
    confirm["message"] = json!("Allow this operation?");
    let dialog = parse_extension_dialog(&confirm).unwrap().unwrap();
    assert_eq!(dialog.dialog_id, "dialog-1");
    assert_eq!(dialog.kind, "confirm");
    assert_eq!(dialog.message.as_deref(), Some("Allow this operation?"));

    let mut select = request("select");
    select["options"] = json!(["Yes", "No"]);
    assert_eq!(parse_extension_dialog(&select).unwrap().unwrap().options, ["Yes", "No"]);

    let mut input = request("input");
    input["placeholder"] = json!("Enter a name");
    let dialog = parse_extension_dialog(&input).unwrap().unwrap();
    assert_eq!(dialog.message.as_deref(), Some("Enter a name"));
    assert_eq!(dialog.initial_value, None);

    let mut editor = request("editor");
    editor["prefill"] = json!("Existing text");
    assert_eq!(
      parse_extension_dialog(&editor)
        .unwrap()
        .unwrap()
        .initial_value
        .as_deref(),
      Some("Existing text")
    );
  }

  #[test]
  fn ignores_only_known_cosmetic_methods() {
    for method in ["notify", "setStatus", "setWidget", "setTitle", "set_editor_text"] {
      assert!(is_cosmetic_extension_method(method));
      assert!(parse_extension_dialog(&request(method)).unwrap().is_none());
    }
    assert!(!is_cosmetic_extension_method("custom"));
    assert!(parse_extension_dialog(&request("custom")).is_err());
  }

  #[test]
  fn rejects_malformed_or_excessive_dialogs_for_explicit_cancellation() {
    let mut value = request("confirm");
    assert!(parse_extension_dialog(&value).is_err());
    value["message"] = json!("x".repeat(MAX_DIALOG_TEXT_BYTES + 1));
    assert!(parse_extension_dialog(&value).is_err());
    value = request("input");
    value["id"] = json!("");
    assert!(parse_extension_dialog(&value).is_err());
    value = request("editor");
    value["prefill"] = json!(42);
    assert!(parse_extension_dialog(&value).is_err());
    value = request("select");
    for options in [json!(["Yes", false]), json!(vec!["choice"; MAX_DIALOG_OPTIONS + 1])] {
      value["options"] = options;
      assert!(parse_extension_dialog(&value).is_err());
    }
    value["options"] = json!(["x".repeat(MAX_DIALOG_TEXT_BYTES + 1)]);
    assert!(parse_extension_dialog(&value).is_err());
    value["options"] = json!(["x".repeat(MAX_DIALOG_TEXT_BYTES)]);
    assert!(parse_extension_dialog(&value).unwrap().is_some());
  }

  #[test]
  fn stderr_tail_is_bounded_and_handles_split_utf8() {
    let mut tail = StderrTail::default();
    tail.push(&vec![b'a'; STDERR_TAIL_BYTES + 200]);
    assert_eq!(tail.bytes.len(), STDERR_TAIL_BYTES);
    tail.push(b"last diagnostic");
    assert_eq!(tail.bytes.len(), STDERR_TAIL_BYTES);
    assert!(tail.text().ends_with("last diagnostic"));
    tail.push(&[0xf0, 0x9f]);
    tail.push(&[0x98, 0x80]);
    assert!(tail.text().ends_with('😀'));
    tail.push(&[0xff]);
    assert!(tail.text().ends_with('\u{fffd}'));
  }
}
