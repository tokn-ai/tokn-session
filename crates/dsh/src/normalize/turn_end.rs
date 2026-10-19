//! Current turn endings and the precise pre-react-loop forms upgraded by
//! DSH's native session-persistence coordinator. Keep the original log intact.
use serde_json::Value;
use tokn_session_core::LifecycleOutcome;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

pub(super) struct Ending<'a> {
  pub outcome: LifecycleOutcome,
  pub error_message: Option<&'a str>,
}

pub(super) fn parse(data: &Value) -> Option<Ending<'_>> {
  if !safe_integer(data.get("turn")?, 1) {
    return None;
  }
  let reason = data.get("reason")?;
  let mut error_message = None;
  let outcome = match reason.get("kind")?.as_str()? {
    "completed" => LifecycleOutcome::Completed,
    "aborted" if valid_cancel_cause(&reason["reason"]) => LifecycleOutcome::Cancelled,
    "interrupted" => LifecycleOutcome::Interrupted,
    "blocked" => LifecycleOutcome::Blocked,
    "max-tokens" => LifecycleOutcome::TokenLimit,
    "error" if reason.get("error").is_some() => {
      let failure = &reason["error"];
      let message = failure.get("message")?.as_str()?;
      let code = failure.get("code")?.as_str()?;
      error_message = Some(diagnostic_message(message, Some(code)));
      LifecycleOutcome::Failed
    }
    "aborted" | "disposed" if legacy_data(data) && has_keys(reason, &["kind"], &[]) => LifecycleOutcome::Cancelled,
    "error" if legacy_data(data) && safe_integer(&reason["step"], 0) => {
      if has_keys(reason, &["kind", "step", "failure"], &[]) {
        let failure = &reason["failure"];
        if !has_keys(
          failure,
          &["message", "code"],
          &["status", "providerRetryAfterMs", "requestId"],
        ) || !optional_number(failure, "status")
          || !optional_number(failure, "providerRetryAfterMs")
          || failure.get("requestId").is_some_and(|value| !value.is_string())
        {
          return None;
        }
        let message = failure.get("message")?.as_str()?;
        let code = failure.get("code")?.as_str()?;
        error_message = Some(diagnostic_message(message, Some(code)));
      } else if has_keys(reason, &["kind", "step", "message"], &["code"]) {
        let message = reason.get("message")?.as_str()?;
        let code = match reason.get("code") {
          Some(code) => Some(code.as_str()?),
          None => None,
        };
        error_message = Some(diagnostic_message(message, code));
      } else {
        return None;
      }
      LifecycleOutcome::Failed
    }
    _ => return None,
  };
  Some(Ending { outcome, error_message })
}

fn diagnostic_message<'a>(message: &'a str, code: Option<&'a str>) -> &'a str {
  let message = message.trim();
  if !message.is_empty() {
    return message;
  }
  code
    .map(str::trim)
    .filter(|code| !code.is_empty())
    .unwrap_or("DSH turn failed")
}

fn legacy_data(data: &Value) -> bool {
  has_keys(data, &["turn", "reason"], &[]) && safe_integer(&data["turn"], 1)
}

fn has_keys(value: &Value, required: &[&str], optional: &[&str]) -> bool {
  value.as_object().is_some_and(|object| {
    required.iter().all(|key| object.contains_key(*key))
      && object
        .keys()
        .all(|key| required.contains(&key.as_str()) || optional.contains(&key.as_str()))
  })
}

fn safe_integer(value: &Value, minimum: u64) -> bool {
  value
    .as_u64()
    .is_some_and(|number| (minimum..=MAX_SAFE_INTEGER).contains(&number))
}

fn optional_number(value: &Value, key: &str) -> bool {
  value.get(key).is_none_or(Value::is_number)
}

fn valid_cancel_cause(cause: &Value) -> bool {
  match cause["kind"].as_str() {
    Some("user" | "parent" | "disposed" | "legacy") => true,
    Some("hook") => cause["reason"].is_string(),
    _ => false,
  }
}
