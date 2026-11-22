use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use serde_json::{Value, json};
use tokn_dsh_protocol::{DshSessionItem, DshSessionLine};

pub(crate) struct Snapshot {
  pub header: tokn_dsh_protocol::SessionHeader,
  pub native_header: Value,
  pub rows: Vec<DshSessionLine>,
}

pub(crate) fn snapshot(path: &Path, expected_id: Option<&str>, max_bytes: usize) -> Result<Snapshot, String> {
  let (native_header, rows, stored_cut) = if crate::sqlite::is_database(path) || crate::sqlite::is_marker(path) {
    let snapshot = crate::sqlite::snapshot(path, expected_id, max_bytes)?;
    (snapshot.header, snapshot.rows, Some(snapshot.inherited_count))
  } else {
    let mut bytes = Vec::new();
    reader(path)?
      .take(max_bytes as u64 + 1)
      .read_to_end(&mut bytes)
      .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    if bytes.len() > max_bytes {
      return Err("DSH decoded source exceeds the snapshot size limit".into());
    }
    let complete = bytes
      .iter()
      .rposition(|byte| *byte == b'\n')
      .map_or(0, |index| index + 1);
    let mut lines = bytes[..complete].split(|byte| *byte == b'\n');
    let native_header: Value = serde_json::from_slice(lines.next().unwrap_or_default())
      .map_err(|error| format!("invalid dsh JSON header: {error}"))?;
    let mut rows = Vec::new();
    for (index, line) in lines.enumerate() {
      if line.iter().all(u8::is_ascii_whitespace) {
        continue;
      }
      if rows.len() >= 1_000_000 {
        return Err("DSH snapshot exceeds its event count limit".into());
      }
      rows.push(
        serde_json::from_slice(line)
          .map_err(|error| format!("invalid dsh JSON at {}:{}: {error}", path.display(), index + 2))?,
      );
    }
    (native_header, rows, None)
  };
  let mut header = crate::session_source::header(
    serde_json::from_value(native_header.clone()).map_err(|error| error.to_string())?,
    path,
  )?;
  if expected_id.is_some_and(|expected| expected != header.id) {
    return Err("DSH history has a different session identity".into());
  }
  let rows = rows
    .into_iter()
    .map(serde_json::from_value)
    .collect::<Result<Vec<DshSessionLine>, _>>()
    .map_err(|error| error.to_string())?;
  let mut event_count = 0u64;
  let mut inherited_cut = None;
  for row in &rows {
    for event in expand(row.clone())? {
      let native = event.native();
      if header.version >= 2 {
        if native["seq"].as_u64() != Some(event_count)
          || native["time"]
            .as_i64()
            .is_none_or(|time| !(-9_007_199_254_740_991..=9_007_199_254_740_991).contains(&time))
        {
          return Err("Invalid DSH event envelope or sequence gap".into());
        }
        if native["type"] == "session/end-seed" && native["data"]["inherited"] == true {
          inherited_cut = Some(event_count);
        }
      }
      event_count += 1;
      if event_count > 1_000_000 {
        return Err("DSH snapshot exceeds its event count limit".into());
      }
    }
  }
  if header.version >= 2 {
    header.seed_length = match (header.is_seeded, inherited_cut) {
      (Some(true), Some(cut)) => Some(cut),
      (Some(false), None) => None,
      _ => return Err("DSH seeded header and inherited end-seed marker disagree".into()),
    };
  }
  if header.seed_length.is_some_and(|cut| cut > event_count)
    || stored_cut.is_some_and(|cut| cut != header.seed_length.unwrap_or(0))
  {
    return Err("DSH inherited event count does not match its history".into());
  }
  Ok(Snapshot {
    header,
    native_header,
    rows,
  })
}

/// Snapshot the byte length, so a concurrently appended session cannot make a
/// historical read follow forever. Truncated/corrupt frames are reported, not
/// repaired; the harness owns its storage.
pub(crate) fn reader(path: &Path) -> Result<Box<dyn BufRead>, String> {
  let file = File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
  let len = file.metadata().map_err(|err| err.to_string())?.len();
  if len > 128 * 1024 * 1024 {
    return Err("DSH source exceeds the input size limit".into());
  }
  let input = file.take(len);
  if path
    .extension()
    .is_some_and(|extension| extension == "zstd" || extension == "zst")
  {
    let mut decoder =
      zstd::stream::read::Decoder::new(input).map_err(|err| format!("failed to decode {}: {err}", path.display()))?;
    decoder.window_log_max(23).map_err(|error| error.to_string())?;
    // Decoder reads all concatenated frames by default, as written by DSH.
    Ok(Box::new(BufReader::new(decoder)))
  } else {
    Ok(Box::new(BufReader::new(input)))
  }
}

pub(crate) fn parse(line: &str, path: &Path, index: usize) -> Result<DshSessionLine, String> {
  serde_json::from_str(line).map_err(|err| format!("invalid dsh JSON at {}:{index}: {err}", path.display()))
}

/// Expand the physical row representation before normalization. Unknown
/// ordinary events pass through, but a malformed packed run must never silently
/// lose its members. Validate before emitting any of them.
pub(crate) fn expand(line: DshSessionLine) -> Result<Vec<DshSessionLine>, String> {
  let mut line = line;
  if let Some(sources) = line.native().get("sourceEventSeqs")
    && sources
      .as_array()
      .is_some_and(|sources| sources.iter().any(Value::is_array))
  {
    let seq = line.native()["seq"].as_u64().ok_or("Invalid DSH source event owner")?;
    let mut expanded = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let sources = sources.as_array().ok_or("Invalid DSH source event ranges")?;
    let has_ranges = sources.iter().any(Value::is_array);
    for source in sources {
      let (start, end) = if let Some(range) = source.as_array() {
        if range.len() != 2 {
          return Err("Invalid DSH source event range".into());
        }
        (
          range[0].as_u64().ok_or("Invalid DSH source event range")?,
          range[1].as_u64().ok_or("Invalid DSH source event range")?,
        )
      } else {
        let source = source.as_u64().ok_or("Invalid DSH source event coordinate")?;
        (source, source)
      };
      if end < start
        || end >= seq
        || end - start >= 1_000_000
        || expanded.len().saturating_add((end - start + 1) as usize) > 1_000_000
      {
        return Err("DSH source event range exceeds its owner or size limit".into());
      }
      for source in start..=end {
        if !seen.insert(source) || has_ranges && expanded.last().is_some_and(|last| *last >= source) {
          return Err("Invalid duplicate or unordered DSH source event ranges".into());
        }
        expanded.push(source);
      }
    }
    let mut native = line.into_native();
    native["sourceEventSeqs"] = json!(expanded);
    line = serde_json::from_value(native).map_err(|error| error.to_string())?;
  }
  let native = line.native();
  let tag = native.get("type").and_then(Value::as_str).unwrap_or("");
  if !matches!(tag, "text-chunks" | "reasoning-chunks" | "tool-call-chunks") {
    return Ok(vec![line]);
  }
  let malformed = || format!("malformed {tag} storage row");
  let (seq, mut time, turn, step, index, dt, members, call) = match line.item() {
    DshSessionItem::TextChunks(row) | DshSessionItem::ReasoningChunks(row) => {
      if !row.extra.is_empty() || !row.data.extra.is_empty() {
        return Err(malformed());
      }
      (
        row.seq0,
        row.time0,
        row.data.turn,
        row.data.step,
        row.data.index,
        &row.data.dt,
        &row.data.texts,
        None,
      )
    }
    DshSessionItem::ToolCallChunks(row) => {
      if !row.extra.is_empty() || !row.data.extra.is_empty() || native["data"].get("name").is_some_and(Value::is_null) {
        return Err(malformed());
      }
      (
        row.seq0,
        row.time0,
        row.data.turn,
        row.data.step,
        row.data.index,
        &row.data.dt,
        &row.data.args,
        Some((&row.data.id, &row.data.name)),
      )
    }
    _ => return Err(malformed()),
  };
  const MAX_SAFE: i64 = 9_007_199_254_740_991;
  if members.is_empty()
    || dt.len() != members.len() - 1
    || seq
      .checked_add(members.len() as u64 - 1)
      .is_none_or(|end| end > MAX_SAFE as u64)
    || !(-MAX_SAFE..=MAX_SAFE).contains(&time)
  {
    return Err(malformed());
  }
  let mut events = Vec::with_capacity(members.len());
  for (offset, member) in members.iter().enumerate() {
    if offset > 0 {
      let gap = dt[offset - 1];
      if !(-MAX_SAFE..=MAX_SAFE).contains(&gap) {
        return Err(malformed());
      }
      time = time
        .checked_add(gap)
        .filter(|time| (-MAX_SAFE..=MAX_SAFE).contains(time))
        .ok_or_else(malformed)?;
    }
    let chunk = if let Some((id, name)) = call {
      let mut chunk = json!({"type": "tool-call-delta", "index": index, "id": id, "argumentsDelta": member});
      if let Some(name) = name {
        chunk["name"] = json!(name);
      }
      chunk
    } else {
      json!({"type": if tag == "text-chunks" { "text-delta" } else { "reasoning-delta" }, "index": index, "text": member})
    };
    events.push(
      serde_json::from_value(json!({
        "type": "assistant/chunk", "seq": seq + offset as u64, "time": time,
        "data": { "turn": turn, "step": step, "chunk": chunk }
      }))
      .map_err(|err| format!("invalid expanded chunk: {err}"))?,
    );
  }
  Ok(events)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn expands_each_packed_kind_with_exact_member_times_and_call_identity() {
    for (tag, delta_tag, payload_key) in [
      ("text-chunks", "text-delta", "texts"),
      ("reasoning-chunks", "reasoning-delta", "texts"),
      ("tool-call-chunks", "tool-call-delta", "args"),
    ] {
      for named in [false, true] {
        let mut data = json!({"turn": 2, "step": 3, "index": 4, "dt": [2, -3]});
        data[payload_key] = json!(["a", "b", "c"]);
        if tag == "tool-call-chunks" {
          data["id"] = json!("call");
          if named {
            data["name"] = json!("read");
          }
        }
        let row = serde_json::from_value(json!({"type": tag, "seq0": 7, "time0": 10, "data": data})).unwrap();
        let events = expand(row).unwrap();
        for (index, (text, time)) in [("a", 10), ("b", 12), ("c", 9)].iter().enumerate() {
          let mut chunk = json!({"type": delta_tag, "index": 4});
          if tag == "tool-call-chunks" {
            chunk["id"] = json!("call");
            chunk["argumentsDelta"] = json!(text);
            if named {
              chunk["name"] = json!("read");
            }
          } else {
            chunk["text"] = json!(text);
          }
          assert_eq!(
            events[index].native(),
            &json!({"type":"assistant/chunk","seq":7+index,"time":time,
            "data":{"turn":2,"step":3,"chunk":chunk}})
          );
        }
      }
    }
  }

  #[test]
  fn rejects_invalid_arity_overflow_and_unrecognized_packed_fields() {
    let valid = json!({"type":"text-chunks","seq0":0,"time0":0,
      "data":{"turn":1,"step":1,"index":0,"dt":[1],"texts":["a","b"]}});
    for (pointer, value) in [
      ("/data/dt", json!([])),
      ("/data/texts", json!([])),
      ("/seq0", json!(9_007_199_254_740_991_u64)),
      ("/time0", json!(9_007_199_254_740_991_i64)),
      ("/data/dt", json!([9_007_199_254_740_992_i64])),
      ("/data/texts", json!([1, 2])),
    ] {
      let mut native = valid.clone();
      *native.pointer_mut(pointer).unwrap() = value;
      assert!(expand(serde_json::from_value(native).unwrap()).is_err(), "{pointer}");
    }
    let mut native = valid;
    native["future"] = json!(true);
    assert!(expand(serde_json::from_value(native).unwrap()).is_err());
  }
}
