//! Incremental normalization over consistent SQLite row snapshots. Timestamps
//! are not change tokens: compare raw rows before decoding so timestamp-free
//! edits and concurrent changes to different sessions cannot be missed.
use std::{collections::HashMap, sync::Arc};

use super::*;

mod compact;
pub use compact::{CompactRecord, CompactSessionRecords, OpenCodeCompactCache};

#[cfg(test)]
mod tests;

/// One consistent session image. Unchanged records keep their Arc identity.
pub struct CachedSessionRecords {
  pub reference: SessionRef,
  pub header: SessionHeader,
  pub records: Vec<Arc<NormalizedRecord>>,
}

/// Per-follow cache, not a database change feed. Each load scans raw SQL rows;
/// unchanged message JSON and its normalization state are reused. Reusing this
/// cache for a different source/session/native mode discards its old contents.
#[derive(Default)]
pub struct OpenCodeSessionCache {
  identity: Option<(PathBuf, Provider, String, bool)>,
  rows: HashMap<String, Arc<CachedRow>>,
  max_source_bytes: Option<usize>,
  #[cfg(test)]
  decoded: usize,
  #[cfg(test)]
  normalized: usize,
}

impl OpenCodeSessionCache {
  /// Bound retained raw row payloads; decoded objects add memory overhead.
  pub fn with_max_source_bytes(max_source_bytes: usize) -> Self {
    Self {
      max_source_bytes: Some(max_source_bytes),
      ..Default::default()
    }
  }

  pub(super) fn load(
    &mut self,
    source: &OpenCodeSessionSource,
    path: PathBuf,
    session_id: &str,
    native: bool,
  ) -> Result<CachedSessionRecords, String> {
    let (session, raw) = read_snapshot_rows(source, &path, session_id, self.max_source_bytes)?;
    let identity = (path.clone(), source.flavor.provider(), session_id.to_owned(), native);
    let compatible = self.identity.as_ref() == Some(&identity);
    let mut normalizer = OpenCodeNormalizer::with_provider(session_id.to_owned(), source.flavor.provider());
    let mut rows = HashMap::new();
    let mut records = Vec::with_capacity(raw.len());
    let mut preview = None;
    let mut message_count = 0;
    #[cfg(test)]
    let (mut decoded_count, mut normalized_count) = (0, 0);
    for row in raw {
      let previous = compatible.then(|| self.rows.get(&row.id)).flatten();
      let same_input = previous.is_some_and(|old| old.raw == row);
      let cached = if same_input && previous.is_some_and(|old| old.before == normalizer) {
        previous.unwrap().clone()
      } else {
        let decoded = if same_input {
          previous.unwrap().decoded.clone()
        } else {
          #[cfg(test)]
          {
            decoded_count += 1;
          }
          Arc::new(row.decode(&session)?)
        };
        let before = normalizer.clone();
        let events = match decoded.as_ref() {
          DecodedRecord::Session(row) => normalizer.normalize_session(row),
          DecodedRecord::Message(row) => normalizer.normalize_message(row.clone()),
          DecodedRecord::Entry(row) => vec![normalizer.normalize_session_entry(row.clone())],
          DecodedRecord::V2Message(row) => normalizer.normalize_v2_message(row),
        };
        #[cfg(test)]
        {
          normalized_count += 1;
        }
        let native = native
          .then(|| match decoded.as_ref() {
            DecodedRecord::Session(row) => serde_json::to_value(row),
            DecodedRecord::Message(row) => serde_json::to_value(row),
            DecodedRecord::Entry(row) => serde_json::to_value(row),
            DecodedRecord::V2Message(row) => serde_json::to_value(row),
          })
          .transpose()
          .map_err(|e| e.to_string())?;
        let preview = decoded.preview();
        let record = Arc::new(NormalizedRecord {
          record_id: row.id.clone(),
          native,
          events,
        });
        Arc::new(CachedRow {
          raw: row,
          decoded,
          before,
          after: normalizer.clone(),
          record,
          preview,
        })
      };
      normalizer = cached.after.clone();
      if matches!(cached.raw.kind, RawKind::Message | RawKind::V2Message(_)) {
        message_count += 1;
      }
      if preview.is_none() {
        preview.clone_from(&cached.preview);
      }
      records.push(cached.record.clone());
      rows.insert(cached.raw.id.clone(), cached);
    }
    let created_at = timestamp(session.time_created);
    let updated_at_ms = session.time_updated.or(session.time_created);
    let title = native_title(session.title);
    let reference = SessionRef {
      model_baseline: None,
      id: session.id,
      parent_session_id: session.parent_id,
      agent_path: None,
      agent_nickname: None,
      agent_role: None,
      preview: if title.is_none() { preview } else { None },
      title,
      path,
      cwd: session.directory,
      timestamp: timestamp(session.time_updated.or(session.time_created)),
      message_count,
    };
    // Install only after the entire snapshot succeeds. Errors leave all old
    // checkpoints intact, and missing keys are removed atomically.
    self.identity = Some(identity);
    self.rows = rows;
    #[cfg(test)]
    {
      self.decoded += decoded_count;
      self.normalized += normalized_count;
    }
    let header = SessionHeader {
      id: reference.id.clone(),
      parent_session_id: reference.parent_session_id.clone(),
      agent_path: None,
      agent_nickname: None,
      agent_role: None,
      title: reference.title.clone(),
      preview: reference.preview.clone(),
      path: reference.path.clone(),
      cwd: reference.cwd.clone(),
      timestamp: created_at,
      updated_at: reference.timestamp.clone(),
      updated_at_ms,
    };
    Ok(CachedSessionRecords {
      reference,
      header,
      records,
    })
  }
}

struct SourceBudget {
  bytes: usize,
  limit: Option<usize>,
}
impl SourceBudget {
  fn add(&mut self, bytes: usize) -> Result<(), String> {
    self.bytes = self.bytes.saturating_add(bytes);
    if self.limit.is_some_and(|limit| self.bytes > limit) {
      return Err("OpenCode session exceeds the source cache size limit".into());
    }
    Ok(())
  }
}

#[derive(PartialEq, Eq)]
enum RawKind {
  Session,
  Message,
  V2Message(String),
  Entry(String),
}
#[derive(PartialEq, Eq)]
struct RawRecord {
  id: String,
  time: Option<i64>,
  data: String,
  kind: RawKind,
  parts: Vec<RawPart>,
}
#[derive(PartialEq, Eq)]
struct RawPart {
  id: String,
  time: Option<i64>,
  data: String,
}
enum DecodedRecord {
  Session(OpenCodeSessionRow),
  Message(OpenCodeMessageRow),
  Entry(OpenCodeSessionEntryRow),
  V2Message(tokn_opencode_protocol::v2::MessageData),
}
struct CachedRow {
  raw: RawRecord,
  decoded: Arc<DecodedRecord>,
  before: OpenCodeNormalizer,
  after: OpenCodeNormalizer,
  record: Arc<NormalizedRecord>,
  preview: Option<String>,
}

impl RawRecord {
  fn source_id(&self) -> &str {
    self.id.split_once(':').unwrap().1
  }
  fn decode(&self, session: &OpenCodeSessionRow) -> Result<DecodedRecord, String> {
    Ok(match &self.kind {
      RawKind::Session => DecodedRecord::Session(session.clone()),
      RawKind::Message => DecodedRecord::Message(OpenCodeMessageRow {
        id: self.source_id().into(),
        time_created: self.time,
        data: serde_json::from_str(&self.data).map_err(|e| format!("invalid message `{}`: {e}", self.source_id()))?,
        parts: self
          .parts
          .iter()
          .map(|part| {
            Ok(OpenCodePartRow {
              id: part.id.clone(),
              time_created: part.time,
              data: serde_json::from_str(&part.data).map_err(|e| format!("invalid part `{}`: {e}", part.id))?,
            })
          })
          .collect::<Result<_, String>>()?,
      }),
      RawKind::V2Message(kind) => DecodedRecord::V2Message(decode_v2_message(self.source_id(), kind, &self.data)?),
      RawKind::Entry(kind) => DecodedRecord::Entry(OpenCodeSessionEntryRow {
        id: self.source_id().into(),
        native_type: kind.clone(),
        time_created: self.time,
        data: serde_json::from_str(&self.data).unwrap_or_else(|_| Value::String(self.data.clone())),
      }),
    })
  }
}

impl DecodedRecord {
  fn preview(&self) -> Option<String> {
    if let Self::V2Message(message) = self {
      return match message.item() {
        tokn_opencode_protocol::v2::MessageItem::User(user) => non_blank(&user.text).map(str::to_owned),
        _ => None,
      };
    }
    let Self::Message(message) = self else {
      return None;
    };
    if !matches!(message.data.item(), MessageItem::User(_)) {
      return None;
    }
    message.parts.iter().find_map(|part| match part.data.item() {
      PartItem::Text(part) if part.synthetic != Some(true) && part.ignored != Some(true) => {
        non_blank(&part.text).map(str::to_owned)
      }
      PartItem::Subtask(part) => part.prompt.as_deref().and_then(non_blank).map(str::to_owned),
      _ => None,
    })
  }
}

fn read_message_rows(
  connection: &Connection,
  session_id: &str,
  budget: &mut SourceBudget,
) -> Result<Vec<RawRecord>, String> {
  let mut messages = Vec::new();
  let mut indices = HashMap::new();
  let mut statement = connection
    .prepare("select id, time_created, data from message where session_id = ?1 order by time_created, id")
    .map_err(|e| e.to_string())?;
  let rows = statement
    .query_map([session_id], |row| {
      Ok((
        row.get::<_, String>(0)?,
        row.get::<_, Option<i64>>(1)?,
        row.get::<_, String>(2)?,
      ))
    })
    .map_err(|e| e.to_string())?;
  for row in rows {
    let (id, time, data) = row.map_err(|e| e.to_string())?;
    budget.add(id.len().saturating_add(data.len()))?;
    indices.insert(id.clone(), messages.len());
    messages.push(RawRecord {
      id: format!("message:{id}"),
      time,
      data,
      parts: Vec::new(),
      kind: RawKind::Message,
    });
  }
  let mut statement = connection
    .prepare(
      "select part.id, part.message_id, part.time_created, part.data
       from part join message
         on message.id = part.message_id and message.session_id = part.session_id
       where part.session_id = ?1
       order by part.time_created, part.id",
    )
    .map_err(|e| e.to_string())?;
  let rows = statement
    .query_map([session_id], |row| {
      Ok((
        row.get::<_, String>(0)?,
        row.get::<_, String>(1)?,
        row.get::<_, Option<i64>>(2)?,
        row.get::<_, String>(3)?,
      ))
    })
    .map_err(|e| e.to_string())?;
  for row in rows {
    let (id, message_id, time, data) = row.map_err(|e| e.to_string())?;
    budget.add(id.len().saturating_add(message_id.len()).saturating_add(data.len()))?;
    if let Some(&index) = indices.get(&message_id) {
      messages[index].parts.push(RawPart { id, time, data });
    }
  }
  Ok(messages)
}

fn read_snapshot_rows(
  source: &OpenCodeSessionSource,
  path: &Path,
  session_id: &str,
  max_source_bytes: Option<usize>,
) -> Result<(OpenCodeSessionRow, Vec<RawRecord>), String> {
  let mut database = connect_database(&path)?;
  let transaction = database
    .transaction()
    .map_err(|e| format!("failed to start session snapshot: {e}"))?;
  let capabilities = OpenCodeCapabilities::detect(&transaction)?;
  let session = load_session_row(&transaction, capabilities, session_id, source.flavor.name())?
    .ok_or_else(|| format!("no {} session found for `{session_id}`", source.flavor.name()))?;
  let mut budget = SourceBudget {
    bytes: 0,
    limit: max_source_bytes,
  };
  let session_json = serde_json::to_string(&session).map_err(|e| e.to_string())?;
  budget.add(session_json.len())?;
  let mut raw = vec![RawRecord {
    id: format!("session:{session_id}"),
    time: session.time_created,
    data: session_json,
    kind: RawKind::Session,
    parts: Vec::new(),
  }];
  let v2 = source.v2_history(&transaction, capabilities, session_id)?;
  let mut timeline = if v2 {
    read_v2_message_rows(&transaction, session_id, &mut budget)?
  } else {
    read_message_rows(&transaction, session_id, &mut budget)?
  };
  if matches!(source.flavor, SessionDatabaseFlavor::ZCode) && capabilities.has_session_entry {
    let mut statement = transaction
      .prepare("select id, type, time_created, data from session_entry where session_id = ?1")
      .map_err(|e| e.to_string())?;
    let rows = statement
      .query_map([session_id], |row| {
        Ok((
          row.get::<_, String>(0)?,
          row.get::<_, String>(1)?,
          row.get::<_, Option<i64>>(2)?,
          row.get::<_, String>(3)?,
        ))
      })
      .map_err(|e| e.to_string())?;
    for row in rows {
      let (id, kind, time, data) = row.map_err(|e| e.to_string())?;
      budget.add(id.len().saturating_add(kind.len()).saturating_add(data.len()))?;
      timeline.push(RawRecord {
        id: format!("entry:{id}"),
        kind: RawKind::Entry(kind),
        time,
        data,
        parts: Vec::new(),
      });
    }
  }
  if !v2 {
    timeline.sort_by(|a, b| a.time.cmp(&b.time).then_with(|| a.source_id().cmp(b.source_id())));
  }
  raw.extend(timeline);
  transaction
    .commit()
    .map_err(|e| format!("failed to finish session snapshot: {e}"))?;
  Ok((session, raw))
}

fn read_v2_message_rows(
  connection: &Connection,
  session_id: &str,
  budget: &mut SourceBudget,
) -> Result<Vec<RawRecord>, String> {
  let mut statement = connection
    .prepare("select id, type, time_created, data from session_message where session_id = ?1 order by seq")
    .map_err(|error| error.to_string())?;
  let rows = statement
    .query_map([session_id], |row| {
      Ok((
        row.get::<_, String>(0)?,
        row.get::<_, String>(1)?,
        row.get::<_, Option<i64>>(2)?,
        row.get::<_, String>(3)?,
      ))
    })
    .map_err(|error| error.to_string())?;
  let mut result = Vec::new();
  for row in rows {
    let (id, kind, time, data) = row.map_err(|error| error.to_string())?;
    budget.add(id.len().saturating_add(kind.len()).saturating_add(data.len()))?;
    result.push(RawRecord {
      id: format!("v2:{id}"),
      time,
      data,
      kind: RawKind::V2Message(kind),
      parts: Vec::new(),
    });
  }
  Ok(result)
}
