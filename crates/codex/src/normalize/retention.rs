//! Correlation caches have a byte budget independent of source length.

use serde_json::Value;

pub(super) const MAX_CORRELATION_BYTES: usize = 8 * 1024 * 1024;

/// Conservative retained allocation estimate, including collection elements.
/// This is a cache admission budget, not a measurement of process RSS.
pub(super) fn value_bytes(value: &Value) -> usize {
  let heap = match value {
    Value::String(text) => text.capacity(),
    Value::Array(items) => items.iter().fold(
      (items.capacity() - items.len()).saturating_mul(size_of::<Value>()),
      |bytes, item| bytes.saturating_add(value_bytes(item)),
    ),
    Value::Object(items) => items.iter().fold(0usize, |bytes, (key, value)| {
      bytes
        .saturating_add(64 + size_of::<String>())
        .saturating_add(key.capacity())
        .saturating_add(value_bytes(value))
    }),
    _ => 0,
  };
  size_of::<Value>().saturating_add(heap)
}
