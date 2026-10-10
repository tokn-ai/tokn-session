use std::{
  path::PathBuf,
  sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
  },
  time::SystemTime,
};

use tokio_util::sync::CancellationToken;
use tokn_session_core::{Provider, SessionHeader, SessionRef};
use tokn_session_opencode::{CompactRecord, OpenCodeCompactCache, OpenCodeSessionSource};

use crate::{
  service_history::{History, check_cancelled},
  service_protocol::CatalogEntry,
};
use tokn_session_relay::{JsonlReader as FileState, RecordOperation, RelayRecord, SessionContext};

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub(crate) struct Snapshot {
  pub generation: String,
  pub revision: u64,
  pub records: History,
  pub entry: CatalogEntry,
  pub error: Option<String>,
  /// Opaque generation-scoped anchor, not a count of omitted events.
  pub event_base: usize,
  pub has_earlier: bool,
}

fn generation() -> String {
  static COUNTER: AtomicU64 = AtomicU64::new(0);
  format!(
    "{}-{}-{}",
    std::process::id(),
    SystemTime::now()
      .duration_since(SystemTime::UNIX_EPOCH)
      .unwrap_or_default()
      .as_nanos(),
    COUNTER.fetch_add(1, Ordering::Relaxed)
  )
}

pub(crate) struct SessionReader {
  file: Option<FileState>,
  database: Option<OpenCodeSessionSource>,
  database_cache: OpenCodeCompactCache,
  #[cfg(test)]
  database_reads: usize,
  native: bool,
  root: PathBuf,
  version: Vec<Option<FileVersion>>,
  codex_history: Option<tokn_session_codex::CodexHistoryReader>,
  window_anchor: Option<(String, usize)>,
  window_shapes: Vec<(String, usize)>,
  preserve_window_anchor: bool,
  cancel: Option<CancellationToken>,
  pub snapshot: Snapshot,
}

impl SessionReader {
  #[cfg(test)]
  pub fn new(entry: CatalogEntry, native: bool, root: PathBuf) -> Result<Self, String> {
    Self::new_with_cancel(entry, native, root, None)
  }

  pub fn new_cancellable(
    entry: CatalogEntry,
    native: bool,
    root: PathBuf,
    cancel: CancellationToken,
  ) -> Result<Self, String> {
    Self::new_with_cancel(entry, native, root, Some(cancel))
  }

  pub fn new_window_cancellable(
    entry: CatalogEntry,
    native: bool,
    root: PathBuf,
    cancel: CancellationToken,
  ) -> Result<Self, String> {
    Self::new_with_mode(entry, native, root, Some(cancel), true)
  }

  fn new_with_cancel(
    entry: CatalogEntry,
    native: bool,
    root: PathBuf,
    cancel: Option<CancellationToken>,
  ) -> Result<Self, String> {
    Self::new_with_mode(entry, native, root, cancel, false)
  }

  fn new_with_mode(
    entry: CatalogEntry,
    native: bool,
    root: PathBuf,
    cancel: Option<CancellationToken>,
    window: bool,
  ) -> Result<Self, String> {
    check_cancelled(cancel.as_ref())?;
    let mut reader = Self {
      file: if matches!(entry.provider, Provider::Codex | Provider::Pi)
        && !(window && entry.provider == Provider::Codex)
      {
        Some(FileState::for_snapshot(
          entry.header.path.clone(),
          entry.provider,
          native,
          &root,
        )?)
      } else {
        None
      },
      database: matches!(entry.provider, Provider::OpenCode | Provider::ZCode)
        .then(|| tokn_session_relay::providers::database(entry.provider, Some(entry.header.path.clone()))),
      database_cache: OpenCodeCompactCache::with_max_source_bytes(crate::service_protocol::MAX_SNAPSHOT_BYTES),
      #[cfg(test)]
      database_reads: 0,
      native,
      root,
      version: Vec::new(),
      codex_history: (window && entry.provider == Provider::Codex).then(|| {
        tokn_session_codex::CodexHistoryReader::new_window(
          entry.header.path.clone(),
          native,
          crate::service_protocol::MAX_SNAPSHOT_BYTES,
        )
      }),
      window_anchor: None,
      window_shapes: Vec::new(),
      preserve_window_anchor: false,
      cancel,
      snapshot: Snapshot {
        generation: generation(),
        revision: 0,
        records: History::new()?,
        entry,
        error: None,
        event_base: 0,
        has_earlier: false,
      },
    };
    reader.poll()?;
    Ok(reader)
  }

  pub fn poll(&mut self) -> Result<bool, String> {
    check_cancelled(self.cancel.as_ref())?;
    let result = self.poll_source();
    check_cancelled(self.cancel.as_ref())?;
    result
  }

  pub fn ensure_history(&mut self, window: Option<(Option<usize>, Option<usize>)>) -> Result<bool, String> {
    if !self.snapshot.has_earlier {
      return Ok(false);
    }
    let expand = match window {
      None => Some(None),
      Some((_, before)) if before.is_some_and(|before| before <= self.snapshot.event_base) => {
        Some(Some(crate::service_history::HISTORY_TURNS))
      }
      _ => None,
    };
    let Some(turns) = expand else {
      return Ok(false);
    };
    self
      .codex_history
      .as_mut()
      .ok_or("Missing lazy Codex reader")?
      .expand(turns);
    self.preserve_window_anchor = true;
    let result = self.poll();
    self.preserve_window_anchor = false;
    result
  }

  fn poll_source(&mut self) -> Result<bool, String> {
    if self.codex_history.is_some() {
      return self.poll_codex_history();
    }
    let path = &self.snapshot.entry.header.path;
    let mut version = versions(path, self.database.is_some());
    if self.snapshot.entry.provider == Provider::WorkBuddy {
      let database = tokn_session_workbuddy::WorkBuddySessionSource::new(Some(self.root.clone())).database_path()?;
      version.extend(versions(&database, true));
    }
    if self.database.is_none()
      && version[0]
        .as_ref()
        .is_some_and(|v| v.length > crate::service_protocol::MAX_SNAPSHOT_BYTES as u64)
    {
      return Err("Relay session exceeds the snapshot size limit".into());
    }
    if version == self.version && self.snapshot.error.is_none() {
      return Ok(false);
    }
    if self.database.is_some() {
      return self.poll_database(version);
    }
    if self.snapshot.entry.provider == Provider::Codex {
      // Detect inherited history from the owning header. The history reader
      // resolves and validates the lineage itself; doing it here would walk
      // the provider roots and verify every prefix twice on a cold open.
      let header = tokn_session_codex::history_header(path)?;
      if !header.native()["payload"]["history_base"].is_null() {
        self.codex_history = Some(tokn_session_codex::CodexHistoryReader::new(
          path.clone(),
          self.native,
          crate::service_protocol::MAX_SNAPSHOT_BYTES,
        ));
        return self.poll_codex_history();
      }
    }
    if matches!(self.snapshot.entry.provider, Provider::WorkBuddy | Provider::Dsh) {
      return self.poll_grouped_file(version);
    }
    let result = self.poll_file(version);
    if result.is_err() {
      // Decoding may advance the source before a malformed batch or journal
      // limit is rejected. Rebuild on retry without losing unpublished rows.
      self.file = None;
    }
    result
  }

  fn poll_file(&mut self, version: Vec<Option<FileVersion>>) -> Result<bool, String> {
    let path = &self.snapshot.entry.header.path;
    let was_grouped = self.file.is_none();
    if was_grouped && matches!(self.snapshot.entry.provider, Provider::Codex | Provider::Pi) {
      self.file = Some(FileState::for_snapshot(
        path.clone(),
        self.snapshot.entry.provider,
        self.native,
        &self.root,
      )?);
    }
    let (records, reset) = if let Some(file) = &mut self.file {
      // Same-length rewrites need a fresh reader; truncation/replacement is
      // also detected inside FileState before its next append read.
      let same_length_edit =
        !self.version.is_empty() && self.version[0].as_ref().map(|v| v.length) == version[0].as_ref().map(|v| v.length);
      if same_length_edit {
        *file = FileState::for_snapshot(path.clone(), self.snapshot.entry.provider, self.native, &self.root)?;
      }
      let (update, reset) = file.follow_snapshot()?;
      if !update.warnings.is_empty() {
        return Err(update.warnings.join("; "));
      }
      (update.records, reset || same_length_edit || was_grouped)
    } else {
      return Err("Provider does not support snapshot/follow".into());
    };
    if records
      .iter()
      .any(|record| record.session.session_id != self.snapshot.entry.header.id)
    {
      return Err("Relay session identity changed; refresh the catalog".into());
    }
    if records.is_empty() && !reset {
      self.version = version;
      return Ok(false);
    }
    self.commit_records(&records, reset)?;
    self.version = version;
    self.snapshot.revision += 1;
    self.snapshot.error = None;
    Ok(true)
  }

  fn commit_records(&mut self, records: &[RelayRecord], reset: bool) -> Result<(), String> {
    let mut history = if reset {
      History::new()?
    } else {
      self.snapshot.records.clone()
    };
    history.append_cancellable(records, self.cancel.as_ref())?;
    self.snapshot.records = history;
    if reset {
      self.snapshot.generation = generation();
    }
    Ok(())
  }

  fn poll_database(&mut self, version: Vec<Option<FileVersion>>) -> Result<bool, String> {
    #[cfg(test)]
    {
      self.database_reads += 1;
    }
    let loaded = self.database.as_ref().unwrap().load_session_records_compact_exact(
      &self.snapshot.entry.header.id,
      self.native,
      &mut self.database_cache,
    )?;
    let result = self.reconcile(loaded.reference, loaded.header, loaded.records, version, false);
    if result.is_err() {
      self.database_cache.invalidate();
    }
    result
  }

  fn poll_codex_history(&mut self) -> Result<bool, String> {
    let source = tokn_session_codex::CodexSessionSource::new(Some(self.root.clone()));
    let Some(update) = self.codex_history.as_mut().unwrap().poll(&source)? else {
      return Ok(false);
    };
    let result = self.commit_codex_history(update);
    if result.is_err() {
      // Publication limits/identity checks can fail after decoding. Rebuild on
      // retry rather than losing this batch behind an advanced source cursor.
      self.codex_history.as_mut().unwrap().invalidate();
    }
    result
  }

  fn commit_codex_history(&mut self, update: tokn_session_codex::CodexHistoryUpdate) -> Result<bool, String> {
    if update.reference.id != self.snapshot.entry.header.id {
      return Err("Relay session identity changed; refresh the catalog".into());
    }
    let shapes = if update.source_start.is_some() || self.window_anchor.is_some() {
      update
        .records
        .iter()
        .map(|record| (record.record_id.clone(), record.events.len()))
        .collect::<Vec<_>>()
    } else {
      Vec::new()
    };
    let mut base = self.snapshot.event_base;
    let mut anchor = self.window_anchor.clone();
    let mut preserve = false;
    if update.reset {
      if self.preserve_window_anchor
        && let Some((id, position)) = &anchor
        && let Some(index) = shapes.iter().position(|(candidate, _)| candidate == id)
        && shapes[index..] == self.window_shapes
      {
        base = position
          .checked_sub(shapes[..index].iter().map(|(_, events)| events).sum::<usize>())
          .ok_or("Codex history position overflow")?;
        preserve = true;
      } else {
        base = usize::try_from(update.source_start.unwrap_or(0)).map_err(|_| "Codex history position overflow")?;
      }
      anchor = shapes.first().map(|(id, _)| (id.clone(), base));
    }
    let context = SessionContext::from_session_ref(Provider::Codex, &update.reference);
    let mut records = Vec::with_capacity(update.records.len());
    for record in update.records {
      let record = RelayRecord {
        path: update.reference.path.clone(),
        topic: format!("codex.{}", context.session_id),
        session: context.clone(),
        operation: RecordOperation::Upsert,
        record,
      };
      records.push(record);
    }
    let generation = self.snapshot.generation.clone();
    self.commit_records(&records, update.reset)?;
    if preserve {
      self.snapshot.generation = generation;
    }
    self.snapshot.event_base = base;
    self.snapshot.has_earlier = update.source_start.is_some_and(|start| start > 0);
    self.window_anchor = anchor;
    if update.reset {
      self.window_shapes = shapes;
    } else {
      self.window_shapes.extend(shapes);
    }
    self.snapshot.entry.header.title = update.reference.title;
    self.snapshot.entry.header.preview = update.reference.preview;
    self.snapshot.entry.header.cwd = update.reference.cwd;
    self.snapshot.entry.header.parent_session_id = update.reference.parent_session_id;
    self.snapshot.revision += 1;
    self.snapshot.error = None;
    self.file = None;
    Ok(true)
  }

  fn poll_grouped_file(&mut self, version: Vec<Option<FileVersion>>) -> Result<bool, String> {
    let entry = &self.snapshot.entry;
    let max_bytes = crate::service_protocol::MAX_SNAPSHOT_BYTES;
    let loaded = match entry.provider {
      Provider::WorkBuddy => tokn_session_workbuddy::WorkBuddySessionSource::new(Some(self.root.clone()))
        .load_session_records_path(&entry.header.path, self.native, max_bytes)?,
      Provider::Dsh => tokn_session_dsh::DshSessionSource::new(Some(self.root.clone())).load_session_records_path(
        &entry.header.path,
        self.native,
        max_bytes,
      )?,
      _ => unreachable!(),
    };
    if loaded.reference.id != entry.header.id {
      return Err("Relay session identity changed; refresh the catalog".into());
    }
    let mut header = entry.header.clone();
    header.title = loaded.reference.title.clone();
    header.preview = loaded.reference.preview.clone();
    header.cwd = loaded.reference.cwd.clone();
    header.parent_session_id = loaded.reference.parent_session_id.clone();
    let changed = self.reconcile(
      loaded.reference,
      header,
      loaded
        .records
        .into_iter()
        .map(|record| CompactRecord::Changed(Arc::new(record)))
        .collect(),
      version,
      self.file.is_some(),
    )?;
    self.file = None;
    Ok(changed)
  }

  fn reconcile(
    &mut self,
    reference: SessionRef,
    header: SessionHeader,
    records: Vec<CompactRecord>,
    version: Vec<Option<FileVersion>>,
    force_reset: bool,
  ) -> Result<bool, String> {
    let mut prefix = 0;
    for (position, record) in records.iter().enumerate().take(self.snapshot.records.len()) {
      let unchanged = match record {
        CompactRecord::Reused(previous) => *previous == position,
        CompactRecord::Changed(record) => self.snapshot.records.matches(position, record)?,
      };
      if !unchanged {
        break;
      }
      prefix += 1;
    }
    #[cfg(unix)]
    let replaced = self.version.first().and_then(Option::as_ref).map(|v| v.identity)
      != version.first().and_then(Option::as_ref).map(|v| v.identity);
    #[cfg(not(unix))]
    let replaced = false;
    let reset = force_reset || replaced || prefix < self.snapshot.records.len();
    let header_changed = header != self.snapshot.entry.header;
    let changed =
      reset || records.len() != self.snapshot.records.len() || header_changed || self.snapshot.error.is_some();
    if !changed {
      self.version = version;
      return Ok(false);
    }
    let context = SessionContext::from_session_ref(self.snapshot.entry.provider, &reference);
    let mut additions = Vec::new();
    for record in records.iter().skip(if reset { 0 } else { prefix }) {
      let normalized = match record {
        CompactRecord::Reused(previous) => self.snapshot.records.read(*previous)?.0.record,
        CompactRecord::Changed(record) => record.as_ref().clone(),
      };
      let record = RelayRecord {
        path: reference.path.clone(),
        topic: format!(
          "{}.{}",
          tokn_session_relay::providers::source(context.provider).as_str(),
          context.session_id
        ),
        session: context.clone(),
        operation: RecordOperation::Upsert,
        record: normalized,
      };
      additions.push(record);
    }
    self.commit_records(&additions, reset)?;
    self.snapshot.entry.header = header;
    self.snapshot.revision += 1;
    self.snapshot.error = None;
    self.version = version;
    Ok(true)
  }
}

pub(crate) use tokn_session_relay::file_version::{FileVersion, versions};
