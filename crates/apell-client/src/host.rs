use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{cell::Cell, rc::Rc};
use tokn_hub_client_core::protocol;

pub fn string<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
  value
    .get(name)
    .and_then(Value::as_str)
    .filter(|value| value.len() <= 8192)
    .ok_or_else(|| format!("Missing or invalid {name}"))
}

pub async fn call<T: DeserializeOwned>(method: &str, params: Value) -> Result<T, String> {
  apell_guest::call(method, params).await.map_err(|error| error.message)
}

pub async fn entropy() -> Result<[u8; 32], String> {
  let value: Value = call("crypto.random_bytes", json!({"length":32})).await?;
  protocol::decode(string(&value, "data")?, 32)?
    .try_into()
    .map_err(|_| "Invalid host entropy response".into())
}

pub async fn delay(duration_ms: u64) -> Result<(), String> {
  call::<Value>("runtime.sleep", json!({"duration_ms":duration_ms}))
    .await
    .map(|_| ())
}

pub fn unix_time() -> Result<u64, String> {
  // Apell returns an RFC3339 clock string; the parser is bounded and keeps the
  // guest independent of a native clock or browser JS imports.
  let value = apell_guest::now();
  parse_time(&value)
}

fn parse_time(value: &str) -> Result<u64, String> {
  let value = value.as_bytes();
  if !(20..=30).contains(&value.len())
    || value[4] != b'-'
    || value[7] != b'-'
    || value[10] != b'T'
    || value[13] != b':'
    || value[16] != b':'
    || value.last() != Some(&b'Z')
  {
    return Err("Host clock must be UTC RFC3339".into());
  }
  if value.len() != 20
    && (value.len() < 22 || value[19] != b'.' || !value[20..value.len() - 1].iter().all(u8::is_ascii_digit))
  {
    return Err("Invalid host clock fraction".into());
  }
  let part = |a: usize, b: usize| -> Result<i64, String> {
    let raw = value.get(a..b).ok_or("Invalid host clock")?;
    if !raw.iter().all(u8::is_ascii_digit) {
      return Err("Invalid host clock".into());
    }
    std::str::from_utf8(raw)
      .unwrap()
      .parse()
      .map_err(|_| "Invalid host clock".into())
  };
  let (mut year, month, day) = (part(0, 4)?, part(5, 7)?, part(8, 10)?);
  let (hour, minute, second) = (part(11, 13)?, part(14, 16)?, part(17, 19)?);
  if !(1970..=9999).contains(&year)
    || !(1..=12).contains(&month)
    || !(1..=31).contains(&day)
    || hour > 23
    || minute > 59
    || second > 59
  {
    return Err("Invalid host clock".into());
  }
  let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
  let days_in_month = match month {
    2 => {
      if leap {
        29
      } else {
        28
      }
    }
    4 | 6 | 9 | 11 => 30,
    _ => 31,
  };
  if day > days_in_month {
    return Err("Invalid host clock date".into());
  }
  year -= i64::from(month <= 2);
  let era = year / 400;
  let yoe = year - era * 400;
  let adjusted_month = month + if month > 2 { -3 } else { 9 };
  let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + (153 * adjusted_month + 2) / 5 + day - 1 - 719468;
  u64::try_from(days * 86400 + hour * 3600 + minute * 60 + second).map_err(|_| "Invalid host clock".into())
}

pub struct Socket {
  pub handle: String,
  closed: Rc<Cell<bool>>,
}

impl Socket {
  pub async fn open(url: &str) -> Result<Self, String> {
    let response: Value = call("websocket.open", json!({"url":url})).await?;
    Ok(Self {
      handle: string(&response, "socket_handle")?.into(),
      closed: Rc::new(Cell::new(false)),
    })
  }
  pub async fn send(&self, data: Vec<u8>) -> Result<(), String> {
    if self.closed.get() || data.len() > 65535 {
      return Err("Encrypted socket closed or record too large".into());
    }
    call::<Value>(
      "websocket.send",
      json!({"socket_handle":self.handle,"data":protocol::encode(&data)}),
    )
    .await
    .map(|_| ())
  }
  pub async fn receive(&self) -> Result<Vec<u8>, String> {
    if self.closed.get() {
      return Err("Machine disconnected".into());
    }
    let value: Value = call(
      "websocket.receive",
      json!({"socket_handle":self.handle,"timeout_ms":90000}),
    )
    .await?;
    let data = value
      .get("data")
      .and_then(Value::as_str)
      .ok_or("Encrypted connection closed; requests are not retried")?;
    protocol::decode(data, 65535)
  }
  pub async fn close(&self) {
    if !self.closed.replace(true) {
      let _ = call::<Value>("websocket.close", json!({"socket_handle":self.handle})).await;
    }
  }
}

impl Drop for Socket {
  fn drop(&mut self) {
    if !self.closed.replace(true) {
      let handle = self.handle.clone();
      let _ = apell_guest::spawn(async move {
        let _ = call::<Value>("websocket.close", json!({"socket_handle":handle})).await;
      });
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn utc_clock_is_parsed_without_platform_imports() {
    assert_eq!(parse_time("1970-01-01T00:00:00Z").unwrap(), 0);
    assert_eq!(parse_time("2026-11-05T00:00:00.123Z").unwrap(), 1793836800);
    assert!(parse_time("2026-11-05T25:00:00Z").is_err());
    assert!(parse_time("2026-11-05T00:00:00+08:00").is_err());
    assert!(parse_time("2026-11-05T00:00:00junkZ").is_err());
    assert!(parse_time("2026-11-05T00:00:00.Z").is_err());
    assert!(parse_time("2026-02-29T00:00:00Z").is_err());
    assert!(parse_time("2100-02-29T00:00:00Z").is_err());
    assert!(parse_time("2000-02-29T00:00:00.123456789Z").is_ok());
  }
}
