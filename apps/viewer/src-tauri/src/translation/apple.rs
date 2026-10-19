use std::{
  collections::HashMap,
  ffi::{CStr, CString, c_char, c_void},
  sync::{
    Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
  },
  time::{Duration, Instant},
};

use serde::Deserialize;
use tokio::sync::oneshot;

use super::{TranslationFailure, TranslationRecovery, TranslationRequest, TranslationResponse};

const TIMEOUT: Duration = Duration::from_secs(300);
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const CANCELLATION_TTL: Duration = Duration::from_secs(60);
const MAX_CANCELLATIONS: usize = 512;
type TranslationResult = Result<TranslationResponse, TranslationFailure>;

struct Pending {
  request_id: String,
  window_label: String,
  text_count: usize,
  sender: oneshot::Sender<TranslationResult>,
}

#[derive(Default)]
struct Requests {
  pending: HashMap<u64, Pending>,
  cancelled: HashMap<(String, String), Instant>,
}

impl Requests {
  fn register(&mut self, token: u64, pending: Pending, now: Instant) -> Result<(), String> {
    self.expire_cancellations(now);
    if self
      .cancelled
      .contains_key(&(pending.window_label.clone(), pending.request_id.clone()))
    {
      return Err("Translation cancelled.".into());
    }
    // One session at a time avoids competing system permission sheets and
    // bounds native work. The viewer can retry after the active request ends.
    if !self.pending.is_empty() {
      return Err("Another response is being translated. Please wait or cancel it first.".into());
    }
    self.pending.insert(token, pending);
    Ok(())
  }

  fn record_cancellation(&mut self, window_label: &str, request_id: &str, now: Instant) {
    self.expire_cancellations(now);
    if self.cancelled.len() >= MAX_CANCELLATIONS {
      let oldest = self
        .cancelled
        .iter()
        .min_by_key(|(_, instant)| **instant)
        .map(|(key, _)| key.clone());
      if let Some(oldest) = oldest {
        self.cancelled.remove(&oldest);
      }
    }
    self.cancelled.insert((window_label.into(), request_id.into()), now);
  }

  fn expire_cancellations(&mut self, now: Instant) {
    self
      .cancelled
      .retain(|_, cancelled| now.duration_since(*cancelled) < CANCELLATION_TTL);
  }
}

static REQUESTS: OnceLock<Mutex<Requests>> = OnceLock::new();
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

fn requests() -> std::sync::MutexGuard<'static, Requests> {
  REQUESTS
    .get_or_init(Mutex::default)
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner)
}

unsafe extern "C" {
  fn tokn_translation_available() -> bool;
  fn tokn_translation_start(
    window: *mut c_void,
    token: u64,
    input: *const c_char,
    callback: extern "C" fn(u64, *const c_char),
  );
  fn tokn_translation_cancel(token: u64);
}

pub fn available() -> bool {
  // The Swift entry point only performs a runtime OS availability check.
  unsafe { tokn_translation_available() }
}

#[derive(Deserialize)]
struct BridgeResponse {
  texts: Option<Vec<String>>,
  error: Option<String>,
  recovery: Option<TranslationRecovery>,
}

extern "C" fn completed(token: u64, json: *const c_char) {
  let pending = requests().pending.remove(&token);
  let Some(pending) = pending else {
    // The request may have timed out/cancelled while Swift was suspended.
    return;
  };
  let result = if json.is_null() {
    Err("Apple Translation returned no result.".into())
  } else {
    // Swift guarantees the NUL-terminated JSON lives until this call returns.
    let bytes = unsafe { CStr::from_ptr(json) }.to_bytes();
    decode_response(bytes, pending.text_count)
  };
  let _ = pending.sender.send(result);
}

fn decode_response(bytes: &[u8], text_count: usize) -> TranslationResult {
  if bytes.len() > MAX_OUTPUT_BYTES {
    return Err("Apple Translation returned too much text.".into());
  }
  let response: BridgeResponse =
    serde_json::from_slice(bytes).map_err(|_| "Apple Translation returned an invalid result.".to_string())?;
  if let Some(error) = response.error {
    return Err(TranslationFailure {
      message: error,
      recovery: response.recovery,
    });
  }
  match response.texts {
    Some(texts) if texts.len() == text_count && texts.iter().all(|text| !text.trim().is_empty()) => {
      Ok(TranslationResponse { texts })
    }
    _ => Err("Apple Translation returned an incomplete result.".into()),
  }
}

fn finish(token: u64, reason: &str) {
  if let Some(pending) = requests().pending.remove(&token) {
    let _ = pending.sender.send(Err(reason.into()));
  }
}

struct RequestGuard {
  token: u64,
  window: tauri::WebviewWindow,
}

impl Drop for RequestGuard {
  fn drop(&mut self) {
    finish(self.token, "Translation cancelled.");
    let token = self.token;
    // Also runs if the command future is dropped. The native cancellation is
    // idempotent; completion callbacks hold only a token, never a Rust pointer.
    let _ = self.window.run_on_main_thread(move || unsafe {
      tokn_translation_cancel(token);
    });
  }
}

pub async fn translate(window: tauri::WebviewWindow, request: TranslationRequest) -> TranslationResult {
  let json = CString::new(serde_json::to_vec(&request).map_err(|error| error.to_string())?)
    .map_err(|error| error.to_string())?;
  let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
  let (sender, receiver) = oneshot::channel();
  {
    let mut requests = requests();
    requests.register(
      token,
      Pending {
        request_id: request.request_id,
        window_label: window.label().into(),
        text_count: request.texts.len(),
        sender,
      },
      Instant::now(),
    )?;
  }
  let _guard = RequestGuard {
    token,
    window: window.clone(),
  };
  let native_window = window.clone();
  window
    .run_on_main_thread(move || {
      // Cancellation may win before AppKit executes this queued closure.
      if !requests().pending.contains_key(&token) {
        return;
      }
      match native_window.ns_window() {
        Ok(pointer) => unsafe {
          tokn_translation_start(pointer, token, json.as_ptr(), completed);
        },
        Err(error) => finish(token, &format!("Could not access the viewer window: {error}")),
      }
    })
    .map_err(|error| format!("Could not start Apple Translation: {error}"))?;
  match tokio::time::timeout(TIMEOUT, receiver).await {
    Ok(Ok(result)) => result,
    Ok(Err(_)) => Err("Apple Translation stopped unexpectedly. Please retry.".into()),
    Err(_) => Err("Apple Translation timed out. Finish any language download, then retry.".into()),
  }
}

pub fn cancel(window: tauri::WebviewWindow, request_id: &str) {
  let token = {
    let mut requests = requests();
    // Tauri can poll async commands out of invocation order. Remember a
    // cancellation even when translate_text has not registered its token yet.
    requests.record_cancellation(window.label(), request_id, Instant::now());
    requests.pending.iter().find_map(|(&token, pending)| {
      (pending.request_id == request_id && pending.window_label == window.label()).then_some(token)
    })
  };
  if let Some(token) = token {
    finish(token, "Translation cancelled.");
    let _ = window.run_on_main_thread(move || unsafe {
      tokn_translation_cancel(token);
    });
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn pending(window_label: &str, request_id: &str) -> Pending {
    Pending {
      request_id: request_id.into(),
      window_label: window_label.into(),
      text_count: 1,
      sender: oneshot::channel().0,
    }
  }

  #[test]
  fn cancellation_before_registration_prevents_late_start_and_is_window_scoped() {
    let now = Instant::now();
    let mut requests = Requests::default();
    requests.record_cancellation("main", "early-cancel", now);
    assert!(requests.register(1, pending("main", "early-cancel"), now).is_err());
    assert!(requests.pending.is_empty());
    assert!(
      requests
        .register(2, pending("other-window", "early-cancel"), now)
        .is_ok()
    );
  }

  #[test]
  fn cancellation_memory_is_bounded_and_expires() {
    let now = Instant::now();
    let mut requests = Requests::default();
    for index in 0..MAX_CANCELLATIONS + 10 {
      requests.record_cancellation("main", &index.to_string(), now);
    }
    assert_eq!(requests.cancelled.len(), MAX_CANCELLATIONS);
    requests.expire_cancellations(now + CANCELLATION_TTL);
    assert!(requests.cancelled.is_empty());
  }

  #[test]
  fn validates_native_result_before_exposing_it() {
    assert_eq!(
      decode_response(br#"{"texts":["translated"],"error":null}"#, 1)
        .unwrap()
        .texts,
      ["translated"]
    );
    assert!(decode_response(br#"{"texts":[],"error":null}"#, 1).is_err());
    assert!(decode_response(br#"{"texts":[" "],"error":null}"#, 1).is_err());
    assert!(decode_response(b"not JSON", 1).is_err());
    assert!(decode_response(br#"{"error":"Language download cancelled."}"#, 1).is_err());
  }

  #[test]
  fn preserves_native_recovery_metadata_without_guessing_from_error_text() {
    let failure = decode_response(
      br#"{"error":"Apple Translation: Unable to Translate","recovery":"translation_settings"}"#,
      1,
    )
    .unwrap_err();
    assert_eq!(failure.recovery, Some(TranslationRecovery::TranslationSettings));
    assert_eq!(failure.message, "Apple Translation: Unable to Translate");

    let legacy = decode_response(br#"{"error":"Apple Translation: Unable to Translate"}"#, 1).unwrap_err();
    assert_eq!(legacy.recovery, None);
    assert!(
      decode_response(br#"{"error":"Failure","recovery":"arbitrary_url"}"#, 1)
        .unwrap_err()
        .recovery
        .is_none()
    );
  }

  #[test]
  fn cancellation_resolves_once_and_late_callbacks_do_not_affect_the_next_request() {
    let cancelled_token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    let (sender, mut receiver) = oneshot::channel();
    requests().pending.insert(
      cancelled_token,
      Pending {
        request_id: "cancelled".into(),
        window_label: "main".into(),
        text_count: 1,
        sender,
      },
    );
    finish(cancelled_token, "Translation cancelled.");
    assert!(receiver.try_recv().unwrap().is_err());

    let next_token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    let (sender, mut receiver) = oneshot::channel();
    requests().pending.insert(
      next_token,
      Pending {
        request_id: "next".into(),
        window_label: "main".into(),
        text_count: 1,
        sender,
      },
    );
    // A stale callback must not dereference a pointer or complete another job.
    completed(cancelled_token, std::ptr::null());
    assert!(matches!(receiver.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
    let json = CString::new(r#"{"texts":["translated"]}"#).unwrap();
    completed(next_token, json.as_ptr());
    assert_eq!(receiver.try_recv().unwrap().unwrap().texts, ["translated"]);
    assert!(requests().pending.is_empty());
  }
}
