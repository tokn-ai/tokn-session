//! Fingerprints/checkpoints for disk-backed consumers. Only changed normalized
//! rows cross this API; unchanged rows reference the previous committed image.
use sha2::{Digest, Sha256};

use super::*;

pub enum CompactRecord {
  Reused(usize),
  Changed(Arc<NormalizedRecord>),
}

pub struct CompactSessionRecords {
  pub reference: SessionRef,
  pub header: SessionHeader,
  pub records: Vec<CompactRecord>,
}

struct Checkpoint {
  fingerprint: [u8; 32],
  before: Arc<OpenCodeNormalizer>,
  after: Arc<OpenCodeNormalizer>,
  has_preview: bool,
  position: usize,
}

/// Keeps no raw rows, decoded objects, or normalized message bodies. The one
/// catalog preview is retained, and equal normalization states share an Arc.
#[derive(Default)]
pub struct OpenCodeCompactCache {
  identity: Option<(PathBuf, Provider, String, bool)>,
  rows: HashMap<String, Checkpoint>,
  preview: Option<(String, String)>,
  max_source_bytes: Option<usize>,
  #[cfg(test)]
  decoded: usize,
  #[cfg(test)]
  normalized: usize,
}

impl OpenCodeCompactCache {
  pub fn with_max_source_bytes(max_source_bytes: usize) -> Self {
    Self {
      max_source_bytes: Some(max_source_bytes),
      ..Default::default()
    }
  }

  /// Call when the consumer failed to commit the returned image. Reused
  /// positions always refer to the last successfully consumed image.
  pub fn invalidate(&mut self) {
    self.identity = None;
    self.rows.clear();
    self.preview = None;
  }

  pub(crate) fn load(
    &mut self,
    source: &OpenCodeSessionSource,
    path: PathBuf,
    session_id: &str,
    native: bool,
  ) -> Result<CompactSessionRecords, String> {
    let (session, raw) = read_snapshot_rows(source, &path, session_id, self.max_source_bytes)?;
    let identity = (path.clone(), source.flavor.provider(), session_id.to_owned(), native);
    let compatible = self.identity.as_ref() == Some(&identity);
    let mut normalizer = Arc::new(OpenCodeNormalizer::with_provider(
      session_id.to_owned(),
      source.flavor.provider(),
    ));
    let mut rows = HashMap::new();
    let mut records = Vec::with_capacity(raw.len());
    let mut preview = None;
    let mut message_count = 0;
    #[cfg(test)]
    let (mut decoded_count, mut normalized_count) = (0, 0);
    for row in raw {
      let fingerprint = fingerprint(&row);
      let previous = compatible.then(|| self.rows.get(&row.id)).flatten();
      let same_input = previous.is_some_and(|old| old.fingerprint == fingerprint);
      let before = normalizer.clone();
      let (record, has_preview) = if same_input && previous.is_some_and(|old| old.before == before) {
        let old = previous.unwrap();
        normalizer = old.after.clone();
        if preview.is_none() && old.has_preview {
          let value = if let Some((_, value)) = self.preview.as_ref().filter(|(id, _)| id == &row.id) {
            Some(value.clone())
          } else {
            #[cfg(test)]
            {
              decoded_count += 1;
            }
            row.decode(&session)?.preview().map(bound_preview)
          };
          preview = value.map(|value| (row.id.clone(), value));
        }
        (CompactRecord::Reused(old.position), old.has_preview)
      } else {
        #[cfg(test)]
        {
          decoded_count += 1;
          normalized_count += 1;
        }
        let decoded = row.decode(&session)?;
        let mut next = (*normalizer).clone();
        let events = match &decoded {
          DecodedRecord::Session(row) => next.normalize_session(row),
          DecodedRecord::Message(row) => next.normalize_message(row.clone()),
          DecodedRecord::Entry(row) => vec![next.normalize_session_entry(row.clone())],
          DecodedRecord::V2Message(row) => next.normalize_v2_message(row),
        };
        if next != *normalizer {
          normalizer = Arc::new(next);
        }
        let native = native
          .then(|| match &decoded {
            DecodedRecord::Session(row) => serde_json::to_value(row),
            DecodedRecord::Message(row) => serde_json::to_value(row),
            DecodedRecord::Entry(row) => serde_json::to_value(row),
            DecodedRecord::V2Message(row) => serde_json::to_value(row),
          })
          .transpose()
          .map_err(|error| error.to_string())?;
        let row_preview = decoded.preview().map(bound_preview);
        let has_preview = row_preview.is_some();
        if preview.is_none() {
          preview = row_preview.map(|value| (row.id.clone(), value));
        }
        (
          CompactRecord::Changed(Arc::new(NormalizedRecord {
            record_id: row.id.clone(),
            events,
            native,
          })),
          has_preview,
        )
      };
      if matches!(row.kind, RawKind::Message | RawKind::V2Message(_)) {
        message_count += 1;
      }
      rows.insert(
        row.id,
        Checkpoint {
          fingerprint,
          before,
          after: normalizer.clone(),
          has_preview,
          position: records.len(),
        },
      );
      records.push(record);
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
      preview: if title.is_none() {
        preview.as_ref().map(|(_, value)| value.clone())
      } else {
        None
      },
      title,
      path,
      cwd: session.directory,
      timestamp: timestamp(updated_at_ms),
      message_count,
    };
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
    self.identity = Some(identity);
    self.rows = rows;
    self.preview = preview;
    #[cfg(test)]
    {
      self.decoded += decoded_count;
      self.normalized += normalized_count;
    }
    Ok(CompactSessionRecords {
      reference,
      header,
      records,
    })
  }
}

fn bound_preview(value: String) -> String {
  // Presentation metadata must not become a hidden copy of a giant prompt.
  value.chars().take(512).collect()
}

fn fingerprint(row: &RawRecord) -> [u8; 32] {
  fn text(hash: &mut Sha256, value: &str) {
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value.as_bytes());
  }
  fn time(hash: &mut Sha256, value: Option<i64>) {
    hash.update([u8::from(value.is_some())]);
    hash.update(value.unwrap_or_default().to_be_bytes());
  }
  let mut hash = Sha256::new();
  text(&mut hash, &row.id);
  time(&mut hash, row.time);
  match &row.kind {
    RawKind::Session => hash.update([0]),
    RawKind::Message => hash.update([1]),
    RawKind::Entry(kind) => {
      hash.update([2]);
      text(&mut hash, kind);
    }
    RawKind::V2Message(kind) => {
      hash.update([3]);
      text(&mut hash, kind);
    }
  }
  text(&mut hash, &row.data);
  hash.update((row.parts.len() as u64).to_be_bytes());
  for part in &row.parts {
    text(&mut hash, &part.id);
    time(&mut hash, part.time);
    text(&mut hash, &part.data);
  }
  hash.finalize().into()
}

#[cfg(test)]
mod tests;
