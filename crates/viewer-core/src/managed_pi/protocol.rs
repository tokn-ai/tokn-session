//! Viewer-specific display mapping over the reusable native Pi RPC layer.
use crate::workspace::ManagedSessionDialog;
use serde_json::Value;
use std::path::PathBuf;
#[cfg(test)]
use tokn_pi_adapter::framing::STDERR_TAIL_BYTES;
pub(super) use tokn_pi_adapter::framing::{MAX_FRAME_BYTES, RpcReader, StderrTail};
#[cfg(test)]
use tokn_pi_rpc_protocol::{MAX_DIALOG_OPTIONS, MAX_DIALOG_TEXT_BYTES, is_cosmetic_extension_method};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RpcState {
  pub(super) session_id: String,
  pub(super) session_file: Option<PathBuf>,
  pub(super) is_streaming: bool,
  pub(super) is_compacting: bool,
  pub(super) pending_message_count: u64,
  pub(super) images_available: bool,
  pub(super) model: Option<crate::models::ModelIdentity>,
}

pub(super) fn parse_state(data: &Value) -> Result<RpcState, String> {
  let state = tokn_pi_rpc_protocol::parse_state(data)?;
  Ok(RpcState {
    session_id: state.session_id,
    session_file: state.session_file,
    is_streaming: state.is_streaming,
    is_compacting: state.is_compacting,
    pending_message_count: state.pending_message_count,
    images_available: state.images_available,
    model: state.model.as_ref().and_then(crate::models::pi_model),
  })
}

pub(super) fn parse_extension_dialog(value: &Value) -> Result<Option<ManagedSessionDialog>, String> {
  Ok(
    tokn_pi_rpc_protocol::parse_extension_dialog(value)?.map(|dialog| ManagedSessionDialog {
      dialog_id: dialog.dialog_id,
      kind: dialog.kind,
      title: dialog.title,
      message: dialog.message,
      options: dialog.options,
      initial_value: dialog.initial_value,
    }),
  )
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
    assert_eq!(tail.text().len(), STDERR_TAIL_BYTES);
    tail.push(b"last diagnostic");
    assert_eq!(tail.text().len(), STDERR_TAIL_BYTES);
    assert!(tail.text().ends_with("last diagnostic"));
    tail.push(&[0xf0, 0x9f]);
    tail.push(&[0x98, 0x80]);
    assert!(tail.text().ends_with('😀'));
    tail.push(&[0xff]);
    assert!(tail.text().ends_with('\u{fffd}'));
  }
}
