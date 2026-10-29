//! Shared, bounded presentation backfill. Catalog requests never wait for a
//! transcript scan; cached names/previews arrive on subsequent catalog polls.
use std::{
  collections::{HashMap, HashSet, VecDeque},
  fs::File,
  io::{BufRead, BufReader, Read, Seek, SeekFrom},
  sync::Mutex,
};

use tokn_session_client::AgentClient;
use tokn_session_core::{Provider, SessionHeader};
use tokn_session_pi::session_source::PiSessionSummary;

use crate::{
  service_protocol::CatalogEntry,
  service_source::{FileVersion, versions},
};

#[cfg(test)]
mod tests;

const TEXT_LIMIT: usize = 512;
const BATCH_SIZE: usize = 8;
const SCAN_BUDGET: u64 = 1024 * 1024;
const MAX_LINE: u64 = 8 * 1024 * 1024;
type Revision = Vec<Option<FileVersion>>;

#[derive(Clone, Default)]
struct Presentation {
  title: Option<String>,
  preview: Option<String>,
}

#[derive(Default)]
struct PiCursor {
  offset: u64,
  version: Option<FileVersion>,
  summary: PiSessionSummary,
  #[cfg(test)]
  decoded: usize,
}

impl PiCursor {
  /// Return true once the snapshotted complete lines have been inspected.
  fn read(&mut self, header: &SessionHeader, revision: &Revision) -> Result<bool, String> {
    let current = revision
      .first()
      .and_then(Option::as_ref)
      .ok_or("Pi source is unavailable")?;
    let replaced = self.version.as_ref().is_some_and(|previous| {
      #[cfg(unix)]
      if previous.identity != current.identity {
        return true;
      }
      previous.length > current.length || (previous != current && previous.length == current.length)
    });
    if replaced || current.length < self.offset {
      *self = Self::default();
    }
    self.version = Some(current.clone());
    let mut file = File::open(&header.path).map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(self.offset)).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(file.take(current.length.saturating_sub(self.offset)));
    let start = self.offset;
    while self.offset < current.length && self.offset - start < SCAN_BUDGET {
      let mut line = Vec::new();
      (&mut reader)
        .take(MAX_LINE + 1)
        .read_until(b'\n', &mut line)
        .map_err(|e| e.to_string())?;
      if line.len() as u64 > MAX_LINE {
        return Err("Pi metadata record exceeds the size limit".into());
      }
      // Retain the byte offset, not a potentially huge partial record. A later
      // append retries the incomplete line without decoding it prematurely.
      if line.last() != Some(&b'\n') {
        return Ok(true);
      }
      let text = std::str::from_utf8(&line).map_err(|e| e.to_string())?;
      if !text.trim().is_empty() {
        self.summary.ingest_line(text)?;
        self.summary.title = bounded(self.summary.title.take());
        self.summary.preview = bounded(self.summary.preview.take());
        #[cfg(test)]
        {
          self.decoded += 1;
        }
      }
      self.offset += line.len() as u64;
    }
    Ok(self.offset == current.length)
  }
}

struct Entry {
  provider: Provider,
  header: SessionHeader,
  desired: Revision,
  attempted: Option<Revision>,
  value: Option<Presentation>,
  pi: PiCursor,
  queued: bool,
  running: bool,
}

#[derive(Default)]
struct State {
  entries: HashMap<String, Entry>,
  queue: VecDeque<String>,
}

#[derive(Default)]
pub(crate) struct PresentationCache {
  state: Mutex<State>,
}

fn bounded(value: Option<String>) -> Option<String> {
  value.map(|text| text.chars().take(TEXT_LIMIT).collect())
}

fn revision(header: &SessionHeader, provider: Provider) -> Revision {
  versions(&header.path, matches!(provider, Provider::OpenCode | Provider::ZCode))
}

fn needs_backfill(entry: &CatalogEntry) -> bool {
  matches!(entry.provider, Provider::Pi | Provider::Dsh | Provider::WorkBuddy)
    || (entry.header.title.is_none()
      && entry.header.preview.is_none()
      && (entry.provider != Provider::Codex || entry.header.parent_session_id.is_none()))
}

impl PresentationCache {
  /// Called in the blocking catalog-discovery job, never under the async
  /// socket writer. Metadata keeps provider+path+session identity from its key.
  pub fn reconcile(&self, catalog: &[CatalogEntry]) {
    let mut candidates: Vec<_> = catalog.iter().filter(|entry| needs_backfill(entry)).collect();
    candidates.sort_by(|a, b| {
      b.header
        .updated_at_ms
        .cmp(&a.header.updated_at_ms)
        .then_with(|| b.header.timestamp.cmp(&a.header.timestamp))
    });
    let revisions: Vec<_> = candidates
      .iter()
      .map(|entry| revision(&entry.header, entry.provider))
      .collect();
    let mut state = self.state.lock().unwrap();
    let keys: HashSet<_> = candidates.iter().map(|entry| &entry.key).collect();
    state.entries.retain(|key, _| keys.contains(key));
    state.queue.retain(|key| keys.contains(key));
    for (item, desired) in candidates.into_iter().zip(revisions) {
      let entry = state.entries.entry(item.key.clone()).or_insert_with(|| Entry {
        provider: item.provider,
        header: item.header.clone(),
        desired: desired.clone(),
        attempted: None,
        value: None,
        pi: PiCursor::default(),
        queued: false,
        running: false,
      });
      entry.header = item.header.clone();
      entry.desired = desired;
      if !entry.queued && !entry.running && entry.attempted.as_ref() != Some(&entry.desired) {
        entry.queued = true;
        state.queue.push_back(item.key.clone());
      }
    }
  }

  /// Follow readers also notice revisions when no client is polling catalogs.
  pub fn refresh_followed(&self, item: &CatalogEntry) {
    let desired = revision(&item.header, item.provider);
    let mut state = self.state.lock().unwrap();
    let Some(entry) = state.entries.get_mut(&item.key) else {
      return;
    };
    entry.desired = desired;
    if !entry.queued && !entry.running && entry.attempted.as_ref() != Some(&entry.desired) {
      entry.queued = true;
      state.queue.push_front(item.key.clone());
    }
  }

  pub fn apply(&self, key: &str, provider: Provider, header: &mut SessionHeader) {
    let state = self.state.lock().unwrap();
    let Some(value) = state.entries.get(key).and_then(|entry| entry.value.as_ref()) else {
      return;
    };
    if provider == Provider::Pi {
      // A blank latest session_info explicitly clears an older Pi name.
      header.title = value.title.clone();
      header.preview = value.preview.clone();
    } else if header.title.is_none() {
      // Native titles belong to the current lightweight header, never to an
      // older body backfill. Followed OpenCode headers already have a preview.
      if matches!(provider, Provider::Dsh | Provider::WorkBuddy) {
        header.title = value.title.clone();
      }
      header.preview = value.preview.clone();
    }
  }

  pub fn decorate(&self, catalog: &[CatalogEntry]) -> Vec<CatalogEntry> {
    catalog
      .iter()
      .cloned()
      .map(|mut entry| {
        self.apply(&entry.key, entry.provider, &mut entry.header);
        entry
      })
      .collect()
  }

  /// One worker owns the queue. A slow source never holds the mutex needed by
  /// catalog consumers, and each pass advances at most eight metadata jobs.
  pub fn step(&self) {
    for _ in 0..BATCH_SIZE {
      let job = {
        let mut state = self.state.lock().unwrap();
        let Some(key) = state.queue.pop_front() else {
          break;
        };
        let Some(entry) = state.entries.get_mut(&key) else {
          continue;
        };
        entry.queued = false;
        entry.running = true;
        (
          key,
          entry.provider,
          entry.header.clone(),
          entry.desired.clone(),
          std::mem::take(&mut entry.pi),
        )
      };
      let (key, provider, header, desired, mut pi) = job;
      let result: Result<_, String> = (|| {
        if provider == Provider::Pi {
          let complete = pi.read(&header, &desired)?;
          Ok((
            Presentation {
              title: pi.summary.title.clone(),
              preview: pi.summary.preview.clone(),
            },
            complete,
          ))
        } else {
          let source = tokn_session_relay::providers::source(provider);
          let hydrated = AgentClient::hydrate_session_header(source, header.clone())?;
          Ok((
            Presentation {
              title: bounded(hydrated.title),
              preview: bounded(hydrated.preview),
            },
            true,
          ))
        }
      })();
      let observed = revision(&header, provider);
      let mut state = self.state.lock().unwrap();
      let Some(entry) = state.entries.get_mut(&key) else {
        continue;
      };
      entry.running = false;
      entry.pi = pi;
      if entry.desired == desired {
        entry.desired = observed.clone();
      }
      let complete = match result {
        Ok((value, complete)) => {
          if complete && entry.desired == desired && observed == desired {
            entry.value = Some(value);
          }
          complete
        }
        // Retry after a new source revision; never replace last-good metadata
        // with an error or fail an otherwise valid provider catalog.
        Err(_) => true,
      };
      if complete {
        entry.attempted = Some(desired);
      }
      if !complete || entry.attempted.as_ref() != Some(&entry.desired) {
        entry.queued = true;
        state.queue.push_back(key);
      }
    }
  }
}
