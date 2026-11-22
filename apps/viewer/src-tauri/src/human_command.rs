//! Native-only Codex checkpoints. No webview command can approve a request.
use std::{future::Future, sync::Arc};
use tauri::{AppHandle, Manager, WebviewWindow};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
use tokn_viewer_core::{
  CodexHumanCommandApproval as Approval, CodexHumanCommandDecision as Decision, CodexHumanCommandReviewer as Reviewer,
};

#[cfg(target_os = "linux")]
mod linux;

const DECLINE: &str = "Decline";
const ALLOW: &str = "Allow once";
const CANCEL: &str = "Cancel";
const MAX_REVIEW_BYTES: usize = 4096;
const MAX_REVIEW_LINES: usize = 24;
const MAX_PENDING_REVIEWS: usize = 8;

#[derive(Clone)]
struct ReviewQueue {
  capacity: Arc<Semaphore>,
  dialogs: Arc<Semaphore>,
}

struct ReviewSlots {
  _capacity: OwnedSemaphorePermit,
  _dialog: OwnedSemaphorePermit,
}

impl ReviewQueue {
  fn new() -> Self {
    Self {
      capacity: Arc::new(Semaphore::new(MAX_PENDING_REVIEWS)),
      dialogs: Arc::new(Semaphore::new(1)),
    }
  }

  async fn acquire(&self, cancelled: impl Future<Output = ()>) -> Option<ReviewSlots> {
    let capacity = self.capacity.clone().try_acquire_owned().ok()?;
    let dialog = tokio::select! {
      biased;
      _ = cancelled => return None,
      result = self.dialogs.clone().acquire_owned() => result.ok()?,
    };
    Some(ReviewSlots {
      _capacity: capacity,
      _dialog: dialog,
    })
  }
}

pub(super) fn reviewer(app: &AppHandle) -> Reviewer {
  let app = app.clone();
  let queue = ReviewQueue::new();
  Arc::new(move |approval| {
    let app = app.clone();
    let queue = queue.clone();
    Box::pin(async move {
      if approval.cancellation.is_cancelled() {
        return Decision::Decline;
      }
      let Some(body) = review_text(&approval) else {
        return Decision::Decline;
      };
      if available_main(&app).is_none() {
        return Decision::Decline;
      }
      let mut cancellation = approval.cancellation.clone();
      let Some(slot) = queue.acquire(cancellation.cancelled()).await else {
        return Decision::Decline;
      };
      let Some(parent) = available_main(&app).filter(|window| window.is_focused().ok() == Some(true)) else {
        return Decision::Decline;
      };
      let mut cancellation = approval.cancellation.clone();
      let (reply, result) = oneshot::channel();
      show(&app, parent, body, approval, slot, reply);
      tokio::select! {
        biased;
        _ = cancellation.cancelled() => Decision::Decline,
        result = result => result.unwrap_or(Decision::Decline),
      }
    })
  })
}

fn available_main(app: &AppHandle) -> Option<WebviewWindow> {
  app
    .get_webview_window("main")
    .filter(|window| window.is_visible().ok() == Some(true) && window.is_minimized().ok() == Some(false))
}

fn still_available(parent: &WebviewWindow, approval: &Approval) -> bool {
  !approval.cancellation.is_cancelled()
    && parent.is_visible().ok() == Some(true)
    && parent.is_minimized().ok() == Some(false)
}

#[cfg(any(target_os = "macos", windows))]
fn show(
  app: &AppHandle,
  parent: WebviewWindow,
  body: String,
  approval: Approval,
  slot: ReviewSlots,
  reply: oneshot::Sender<Decision>,
) {
  use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
  let dialog_app = app.clone();
  let _ = app.run_on_main_thread(move || {
    if !still_available(&parent, &approval) || parent.is_focused().ok() != Some(true) {
      let _ = reply.send(Decision::Decline);
      return;
    }
    // rfd's first custom button is the native default on macOS/Windows.
    // Put Decline first; neither Enter nor an unexpected result can approve.
    dialog_app
      .dialog()
      .message(body)
      .title("Codex command approval")
      .kind(MessageDialogKind::Warning)
      .parent(&parent)
      .buttons(MessageDialogButtons::YesNoCancelCustom(
        DECLINE.into(),
        ALLOW.into(),
        CANCEL.into(),
      ))
      .show_with_result(move |result| {
        // Keep the slot until the OS dialog actually closes, even when native
        // cancellation already retired its receiver. Late clicks do nothing.
        let _slot = slot;
        let decision = if still_available(&parent, &approval) {
          dialog_result(result)
        } else {
          Decision::Decline
        };
        let _ = reply.send(decision);
      });
  });
}

#[cfg(any(target_os = "macos", windows))]
fn dialog_result(result: tauri_plugin_dialog::MessageDialogResult) -> Decision {
  use tauri_plugin_dialog::MessageDialogResult;
  match result {
    MessageDialogResult::Custom(label) if label == ALLOW => Decision::AllowOnce,
    MessageDialogResult::Custom(label) if label == CANCEL => Decision::Cancel,
    MessageDialogResult::Cancel => Decision::Cancel,
    _ => Decision::Decline,
  }
}

#[cfg(target_os = "linux")]
use linux::show;

fn quoted(value: &str) -> String {
  visible(&serde_json::to_string(value).expect("strings serialize"))
}

fn visible(value: &str) -> String {
  let mut output = String::new();
  for c in value.chars() {
    if c.is_control()
      || (c.is_whitespace() && c != ' ')
      || matches!(c,
      '\u{00ad}' | '\u{034f}' | '\u{061c}' | '\u{180e}' | '\u{200b}'..='\u{200f}'
      | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{fe00}'..='\u{fe0f}'
      | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{e0100}'..='\u{e01ef}')
    {
      use std::fmt::Write;
      let _ = write!(output, "\\u{{{:x}}}", c as u32);
    } else {
      output.push(c);
    }
  }
  output
}

fn review_text(approval: &Approval) -> Option<String> {
  format_review(ReviewDetails {
    command: &approval.command,
    cwd: &approval.cwd,
    reason: approval.reason.as_deref(),
    environment: approval.environment_id.as_deref(),
    permissions: approval.additional_permissions.as_ref(),
  })
}

struct ReviewDetails<'a> {
  command: &'a str,
  cwd: &'a str,
  reason: Option<&'a str>,
  environment: Option<&'a str>,
  permissions: Option<&'a serde_json::Value>,
}

fn format_review(details: ReviewDetails<'_>) -> Option<String> {
  let permissions = details
    .permissions
    .map(|value| visible(&serde_json::to_string(value).expect("native JSON serializes")))
    .unwrap_or_else(|| "None".into());
  let optional = |value: Option<&str>| value.map(quoted).unwrap_or_else(|| "None".into());
  let body = format!(
    "Allow once applies to this command. Cancel stops the requesting turn.\nEscape sequences show hidden characters.\n\nCommand: {}\nFolder: {}\nReason: {}\nEnvironment: {}\nPermissions: {}",
    quoted(details.command),
    quoted(details.cwd),
    optional(details.reason),
    optional(details.environment),
    permissions,
  );
  // Native message dialogs have no common scroll/resize contract. Refuse an
  // oversized review rather than showing a truncated operation with Allow.
  let lines: usize = body
    .lines()
    .map(|line| {
      let width: usize = line.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
      width.div_ceil(48).max(1)
    })
    .sum();
  (body.len() <= MAX_REVIEW_BYTES && lines <= MAX_REVIEW_LINES).then_some(body)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn details(command: &str) -> ReviewDetails<'_> {
    ReviewDetails {
      command,
      cwd: "/workspace/project",
      reason: None,
      environment: None,
      permissions: None,
    }
  }

  #[test]
  fn review_shows_the_complete_operation_and_optional_context_as_plain_text() {
    let permissions = serde_json::json!({"file_system":{"write":["/workspace/output"]}});
    let text = format_review(ReviewDetails {
      reason: Some("Build the requested preview"),
      environment: Some("local"),
      permissions: Some(&permissions),
      ..details("printf '%s' '<b>100%</b>'")
    })
    .unwrap();
    assert!(text.contains("Command: \"printf '%s' '<b>100%</b>'\""));
    assert!(text.contains("Folder: \"/workspace/project\""));
    assert!(text.contains("Reason: \"Build the requested preview\""));
    assert!(text.contains("Environment: \"local\""));
    assert!(text.contains("Permissions: {\"file_system\":{\"write\":[\"/workspace/output\"]}}"));
    assert!(!text.contains("Thread:"));
    assert!(!text.contains("Approval:"));
  }

  #[test]
  fn unrenderable_reviews_are_declined_instead_of_truncated() {
    assert!(format_review(details("echo hello")).is_some());
    assert!(format_review(details(&"x".repeat(MAX_REVIEW_BYTES + 1))).is_none());
    // Escapes and wide characters count toward the visible layout limit too.
    assert!(format_review(details(&"\u{202e}".repeat(200))).is_none());
    assert!(format_review(details(&"界".repeat(600))).is_none());
  }

  #[tokio::test]
  async fn native_review_queue_is_bounded_serial_and_cancellable() {
    let queue = ReviewQueue::new();
    let active = queue.acquire(std::future::pending()).await.unwrap();
    let (cancel, cancelled) = oneshot::channel::<()>();
    let waiting = queue.acquire(async {
      let _ = cancelled.await;
    });
    tokio::pin!(waiting);
    tokio::select! {
      biased;
      _ = &mut waiting => panic!("a second dialog must wait"),
      _ = tokio::task::yield_now() => {}
    }
    assert_eq!(queue.capacity.available_permits(), MAX_PENDING_REVIEWS - 2);
    cancel.send(()).unwrap();
    assert!(waiting.await.is_none());
    assert_eq!(queue.capacity.available_permits(), MAX_PENDING_REVIEWS - 1);
    drop(active);
    // Cancellation wins even when the dialog permit is immediately available.
    assert!(queue.acquire(std::future::ready(())).await.is_none());
    assert_eq!(queue.capacity.available_permits(), MAX_PENDING_REVIEWS);
    let _full = queue
      .capacity
      .clone()
      .acquire_many_owned(MAX_PENDING_REVIEWS as u32)
      .await
      .unwrap();
    assert!(queue.acquire(std::future::pending()).await.is_none());
  }

  #[test]
  fn controls_and_bidi_are_visible_without_confusing_literal_backslashes() {
    assert_eq!(
      quoted("printf 'a'\n\t\u{1b}[31m\u{202e}txt\0"),
      "\"printf 'a'\\n\\t\\u001b[31m\\u{202e}txt\\u0000\""
    );
    assert_eq!(quoted("\\u{202e}"), "\"\\\\u{202e}\"");
    assert_eq!(quoted("a\u{200b}b"), "\"a\\u{200b}b\"");
    assert_eq!(quoted("100% %s <b>plain</b>"), "\"100% %s <b>plain</b>\"");
  }

  #[cfg(any(target_os = "macos", windows))]
  #[test]
  fn only_the_explicit_allow_once_custom_result_can_approve() {
    use tauri_plugin_dialog::MessageDialogResult as Result;
    for result in [
      Result::Yes,
      Result::No,
      Result::Ok,
      Result::Custom(DECLINE.into()),
      Result::Custom("unexpected".into()),
    ] {
      assert!(matches!(dialog_result(result), Decision::Decline));
    }
    assert!(matches!(
      dialog_result(Result::Custom(ALLOW.into())),
      Decision::AllowOnce
    ));
    assert!(matches!(dialog_result(Result::Custom(CANCEL.into())), Decision::Cancel));
    assert!(matches!(dialog_result(Result::Cancel), Decision::Cancel));
  }
}
