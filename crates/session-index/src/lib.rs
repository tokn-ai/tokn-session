//! A compact, app-owned SQLite index for historical agent sessions.
//!
//! The index deliberately stores only source checkpoints, session identity, and
//! bounded caller-supplied metadata. It never stores normalized events, native
//! provider payloads, reasoning, tool inputs, or tool outputs. Titles and
//! bounded previews are the caller-supplied presentation text the index permits.
//! Latest-message JSON contains compact display metadata, never full records;
//! callers must bound each field before indexing.
//!
//! A source replacement is transactional. Callers only invoke it after a
//! complete provider scan succeeds, which means sessions omitted from the
//! replacement can safely be tombstoned without treating a transient source
//! failure as deletion. When more than one process can refresh the index,
//! callers also attach the cursor observed before scanning as an optimistic
//! precondition, so a stale scan cannot overwrite a newer replacement.

mod projects;

use std::{
  collections::{HashMap, HashSet},
  error::Error,
  fmt,
  path::Path,
  sync::{Mutex, MutexGuard},
  time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, ErrorCode, OptionalExtension, Transaction, TransactionBehavior, params, params_from_iter};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const APPLICATION_ID: i32 = 0x544f_4b4e; // "TOKN"

const MIGRATION_001: &str = r#"
CREATE TABLE sources (
  id INTEGER PRIMARY KEY,
  provider TEXT NOT NULL,
  source_key TEXT NOT NULL,
  cursor TEXT NOT NULL,
  scanned_at_ms INTEGER NOT NULL,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  UNIQUE(provider, source_key)
);

CREATE TABLE sessions (
  id INTEGER PRIMARY KEY,
  source_id INTEGER NOT NULL REFERENCES sources(id) ON DELETE RESTRICT,
  session_id TEXT NOT NULL,
  source_path TEXT NOT NULL,
  title TEXT,
  preview TEXT,
  cwd TEXT,
  timestamp TEXT,
  updated_at TEXT,
  updated_at_ms INTEGER,
  parent_session_id TEXT,
  agent_path TEXT,
  agent_nickname TEXT,
  agent_role TEXT,
  attention_marker TEXT,
  attention_revision INTEGER NOT NULL DEFAULT 0 CHECK(attention_revision >= 0),
  seen_attention_revision INTEGER NOT NULL DEFAULT 0
    CHECK(seen_attention_revision >= 0 AND seen_attention_revision <= attention_revision),
  seen_at_ms INTEGER,
  present INTEGER NOT NULL DEFAULT 1 CHECK(present IN (0, 1)),
  first_indexed_at_ms INTEGER NOT NULL,
  indexed_at_ms INTEGER NOT NULL,
  UNIQUE(source_id, session_id)
);

CREATE INDEX sessions_by_source_and_updated
  ON sessions(source_id, present, updated_at_ms DESC, session_id);
CREATE INDEX sessions_by_source_and_parent
  ON sessions(source_id, parent_session_id, present, session_id);
CREATE INDEX sessions_by_attention
  ON sessions(present, attention_revision, seen_attention_revision);
"#;

// These fields distinguish a known-empty attention marker from a marker that
// has not been established yet. Existing indexes were written only after a
// complete body scan, so they migrate to the completed, non-notifying state.
const MIGRATION_002: &str = r#"
ALTER TABLE sessions
  ADD COLUMN attention_baselined INTEGER NOT NULL DEFAULT 1
    CHECK(attention_baselined IN (0, 1));

ALTER TABLE sessions
  ADD COLUMN notify_on_baseline INTEGER NOT NULL DEFAULT 0
    CHECK(notify_on_baseline IN (0, 1));
"#;

// `sessions` stays normalized through `sources`: a provider/source pair has
// one authoritative identity, and duplicating it into every row would make
// source replacement need to maintain a redundant invariant. The view is the
// stable, convenient SQL read surface for diagnostics and future consumers
// that need a session row with its provider alongside it.
const MIGRATION_003: &str = r#"
CREATE VIEW indexed_sessions AS
SELECT
  source.provider AS provider,
  source.source_key AS source_key,
  session.id AS id,
  session.source_id AS source_id,
  session.session_id AS session_id,
  session.source_path AS source_path,
  session.title AS title,
  session.preview AS preview,
  session.cwd AS cwd,
  session.timestamp AS timestamp,
  session.updated_at AS updated_at,
  session.updated_at_ms AS updated_at_ms,
  session.parent_session_id AS parent_session_id,
  session.agent_path AS agent_path,
  session.agent_nickname AS agent_nickname,
  session.agent_role AS agent_role,
  session.attention_marker AS attention_marker,
  session.attention_baselined AS attention_baselined,
  session.notify_on_baseline AS notify_on_baseline,
  session.attention_revision AS attention_revision,
  session.seen_attention_revision AS seen_attention_revision,
  session.seen_at_ms AS seen_at_ms,
  session.present AS present,
  session.first_indexed_at_ms AS first_indexed_at_ms,
  session.indexed_at_ms AS indexed_at_ms
FROM sessions AS session
INNER JOIN sources AS source ON source.id = session.source_id;
"#;

// Provider header scans and body inspections observe different representations
// of one session. Keep their presentation values separate so a delayed body
// completion cannot overwrite a newer catalog title, while a catalog with no
// title can still use the body-derived fallback. Existing rows predate this
// distinction, so retain their displayed title/preview as the initial fallback
// during migration instead of making upgraded sidebars suddenly untitled.
const MIGRATION_004: &str = r#"
ALTER TABLE sessions
  ADD COLUMN body_title TEXT;

ALTER TABLE sessions
  ADD COLUMN body_preview TEXT;

UPDATE sessions
SET body_title = title,
    body_preview = preview
WHERE body_title IS NULL
  AND body_preview IS NULL;

DROP VIEW indexed_sessions;

CREATE VIEW indexed_sessions AS
SELECT
  source.provider AS provider,
  source.source_key AS source_key,
  session.id AS id,
  session.source_id AS source_id,
  session.session_id AS session_id,
  session.source_path AS source_path,
  COALESCE(session.title, session.body_title) AS title,
  COALESCE(session.preview, session.body_preview) AS preview,
  session.title AS catalog_title,
  session.preview AS catalog_preview,
  session.body_title AS body_title,
  session.body_preview AS body_preview,
  session.cwd AS cwd,
  session.timestamp AS timestamp,
  session.updated_at AS updated_at,
  session.updated_at_ms AS updated_at_ms,
  session.parent_session_id AS parent_session_id,
  session.agent_path AS agent_path,
  session.agent_nickname AS agent_nickname,
  session.agent_role AS agent_role,
  session.attention_marker AS attention_marker,
  session.attention_baselined AS attention_baselined,
  session.notify_on_baseline AS notify_on_baseline,
  session.attention_revision AS attention_revision,
  session.seen_attention_revision AS seen_attention_revision,
  session.seen_at_ms AS seen_at_ms,
  session.present AS present,
  session.first_indexed_at_ms AS first_indexed_at_ms,
  session.indexed_at_ms AS indexed_at_ms
FROM sessions AS session
INNER JOIN sources AS source ON source.id = session.source_id;
"#;

// A source's provider cursor can legitimately stay the same while a catalog
// writer discovers newer lightweight metadata. Keep an index-owned mutation
// generation alongside it so optimistic replacements can distinguish that
// case from an untouched source.
const MIGRATION_005: &str = r#"
ALTER TABLE sources
  ADD COLUMN generation INTEGER NOT NULL DEFAULT 0
    CHECK(generation >= 0);
"#;

// File notifications need to detect another source claiming one session
// identity without scanning every provider's catalog.
const MIGRATION_006: &str = r#"
CREATE INDEX sessions_by_identity
  ON sessions(session_id, present, source_id);
"#;

// Project order is a durable discovery record, independent of session activity.
// Seed already-known projects from their earliest session time; later discovery
// never rewrites an existing anchor, even after all its sessions disappear.
const MIGRATION_007: &str = r#"
CREATE TABLE projects (
  project_key TEXT PRIMARY KEY NOT NULL,
  discovered_at_ms INTEGER NOT NULL,
  sort_at_ms INTEGER NOT NULL
);
INSERT INTO projects(project_key, discovered_at_ms, sort_at_ms)
SELECT trim(cwd), MIN(first_indexed_at_ms), MIN(COALESCE(
  CASE
    WHEN length(trim(timestamp)) > 0
      AND ltrim(trim(timestamp), '+-') NOT GLOB '*[^0-9.]*'
      AND ltrim(trim(timestamp), '+-') GLOB '[0-9]*'
    THEN CAST(CAST(timestamp AS REAL) * CASE WHEN abs(CAST(timestamp AS REAL)) < 100000000000 THEN 1000 ELSE 1 END AS INTEGER)
    ELSE CAST(round((julianday(timestamp) - 2440587.5) * 86400000) AS INTEGER)
  END, first_indexed_at_ms))
FROM sessions WHERE cwd IS NOT NULL AND trim(cwd) != '' GROUP BY trim(cwd);
"#;

const MIGRATION_008: &str = r#"
CREATE TABLE project_aliases (
  cwd TEXT PRIMARY KEY NOT NULL,
  project_key TEXT NOT NULL REFERENCES projects(project_key)
);
"#;

// Add a compact latest-assistant projection without invalidating completed
// body baselines. SQL NULL means not inspected by this version; JSON null is
// an authoritative body inspection with no visible assistant message.
const MIGRATION_009: &str = r#"
ALTER TABLE sessions
  ADD COLUMN latest_message TEXT CHECK(latest_message IS NULL OR
    (length(latest_message) <= 4096 AND json_valid(latest_message)));

DROP VIEW indexed_sessions;

CREATE VIEW indexed_sessions AS
SELECT
  source.provider AS provider,
  source.source_key AS source_key,
  source.cursor AS source_cursor,
  source.generation AS source_generation,
  session.id AS id,
  session.source_id AS source_id,
  session.session_id AS session_id,
  session.source_path AS source_path,
  COALESCE(session.title, session.body_title) AS title,
  COALESCE(session.preview, session.body_preview) AS preview,
  session.title AS catalog_title,
  session.preview AS catalog_preview,
  session.body_title AS body_title,
  session.body_preview AS body_preview,
  session.latest_message AS latest_message,
  session.cwd AS cwd,
  session.timestamp AS timestamp,
  session.updated_at AS updated_at,
  session.updated_at_ms AS updated_at_ms,
  session.parent_session_id AS parent_session_id,
  session.agent_path AS agent_path,
  session.agent_nickname AS agent_nickname,
  session.agent_role AS agent_role,
  session.attention_marker AS attention_marker,
  session.attention_baselined AS attention_baselined,
  session.notify_on_baseline AS notify_on_baseline,
  session.attention_revision AS attention_revision,
  session.seen_attention_revision AS seen_attention_revision,
  session.seen_at_ms AS seen_at_ms,
  session.present AS present,
  session.first_indexed_at_ms AS first_indexed_at_ms,
  session.indexed_at_ms AS indexed_at_ms
FROM sessions AS session
INNER JOIN sources AS source ON source.id = session.source_id;
"#;

#[derive(Clone, Debug)]
pub struct ProjectGroup {
  pub project_key: String,
  pub order_ms: i64,
}

struct Migration {
  version: i64,
  sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
  Migration {
    version: 1,
    sql: MIGRATION_001,
  },
  Migration {
    version: 2,
    sql: MIGRATION_002,
  },
  Migration {
    version: 3,
    sql: MIGRATION_003,
  },
  Migration {
    version: 4,
    sql: MIGRATION_004,
  },
  Migration {
    version: 5,
    sql: MIGRATION_005,
  },
  Migration {
    version: 6,
    sql: MIGRATION_006,
  },
  Migration {
    version: 7,
    sql: MIGRATION_007,
  },
  Migration {
    version: 8,
    sql: MIGRATION_008,
  },
  Migration {
    version: 9,
    sql: MIGRATION_009,
  },
];

/// Errors returned by [`SessionIndex`].
#[derive(Debug)]
pub enum SessionIndexError {
  /// SQLite rejected an operation.
  Sqlite(rusqlite::Error),
  /// The index's serialized connection was poisoned by a previous panic.
  ConnectionPoisoned,
  /// A replacement does not describe one complete source snapshot.
  InvalidReplacement(String),
  /// A previously applied migration no longer matches the embedded migration.
  MigrationChecksumMismatch {
    version: i64,
    expected: String,
    found: String,
  },
  /// The database migration ledger is not a valid prefix of this crate's
  /// migrations, so continuing could misinterpret cached data.
  MigrationHistoryInvalid(String),
  /// The target is an existing SQLite database owned by another application.
  UnexpectedDatabase(i32),
  /// A session's monotonically increasing attention revision reached `i64::MAX`.
  AttentionRevisionOverflow,
  /// Another index writer changed a source after this replacement was planned.
  SourceCursorConflict {
    source: SourceKey,
    expected: SourceCursorPrecondition,
    actual: Option<SourceCheckpoint>,
  },
}

impl fmt::Display for SessionIndexError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Sqlite(error) => write!(formatter, "SQLite index error: {error}"),
      Self::ConnectionPoisoned => formatter.write_str("SQLite index connection lock was poisoned"),
      Self::InvalidReplacement(message) => write!(formatter, "invalid source replacement: {message}"),
      Self::MigrationChecksumMismatch {
        version,
        expected,
        found,
      } => write!(
        formatter,
        "migration {version} checksum mismatch (expected {expected}, found {found})"
      ),
      Self::MigrationHistoryInvalid(message) => write!(formatter, "invalid migration history: {message}"),
      Self::UnexpectedDatabase(application_id) => write!(
        formatter,
        "database belongs to another application (application_id {application_id})"
      ),
      Self::AttentionRevisionOverflow => formatter.write_str("attention revision overflow"),
      Self::SourceCursorConflict { source, .. } => write!(
        formatter,
        "source cursor changed before replacement for {}/{} could commit",
        source.provider, source.source_key
      ),
    }
  }
}

impl Error for SessionIndexError {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Sqlite(error) => Some(error),
      _ => None,
    }
  }
}

impl From<rusqlite::Error> for SessionIndexError {
  fn from(error: rusqlite::Error) -> Self {
    Self::Sqlite(error)
  }
}

/// Result type returned by the index store.
pub type Result<T> = std::result::Result<T, SessionIndexError>;

/// Opaque provider-specific identity of one scanned source.
///
/// `source_key` is not required to be a path. For example, an app can use a
/// stable key for an OpenCode database while individual indexed sessions retain
/// their actual `source_path` for reopening.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SourceKey {
  pub provider: String,
  pub source_key: String,
}

impl SourceKey {
  pub fn new(provider: impl Into<String>, source_key: impl Into<String>) -> Self {
    Self {
      provider: provider.into(),
      source_key: source_key.into(),
    }
  }
}

/// Opaque source-aware identity of an indexed session.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionKey {
  pub provider: String,
  pub source_key: String,
  pub session_id: String,
}

impl SessionKey {
  pub fn new(provider: impl Into<String>, source_key: impl Into<String>, session_id: impl Into<String>) -> Self {
    Self {
      provider: provider.into(),
      source_key: source_key.into(),
      session_id: session_id.into(),
    }
  }

  pub fn source_key(&self) -> SourceKey {
    SourceKey {
      provider: self.provider.clone(),
      source_key: self.source_key.clone(),
    }
  }
}

/// Last successful scan state for one provider source.
///
/// `cursor` is intentionally opaque. It can represent a provider-specific
/// source revision, database/WAL fingerprint, or other safe checkpoint; this
/// crate never interprets it as a universal byte offset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceState {
  pub key: SourceKey,
  pub cursor: String,
  pub scanned_at_ms: i64,
  /// Monotonically increases for every committed replacement or baseline
  /// completion, even when the provider-owned cursor is unchanged.
  ///
  /// A freshly constructed state describes a proposed next value and starts at
  /// zero. States read from [`SessionIndex`] carry the durable generation that
  /// optimistic callers must use as their precondition.
  pub generation: i64,
}

impl SourceState {
  pub fn new(key: SourceKey, cursor: impl Into<String>, scanned_at_ms: i64) -> Self {
    Self {
      key,
      cursor: cursor.into(),
      scanned_at_ms,
      generation: 0,
    }
  }
}

/// Durable identity of a source state observed before a replacement.
///
/// The provider-owned cursor alone is not sufficient: lightweight catalog
/// metadata can change without advancing it. `generation` is maintained by
/// the index and makes those same-cursor writes conflict safely.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceCheckpoint {
  pub cursor: String,
  pub generation: i64,
}

impl From<&SourceState> for SourceCheckpoint {
  fn from(source: &SourceState) -> Self {
    Self {
      cursor: source.cursor.clone(),
      generation: source.generation,
    }
  }
}

/// A condition that must match the source state immediately before a
/// replacement commits.
///
/// Use [`SourceCursorPrecondition::Exact`] with the checkpoint from a prior
/// [`SourceState`] to prevent a stale scan from overwriting a newer snapshot
/// committed by another process. `Exact(None)` requires the source not to
/// have been indexed yet. [`SourceCursorPrecondition::Any`] is the
/// compatibility default for callers that do not need optimistic concurrency
/// control.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum SourceCursorPrecondition {
  /// Do not compare the current source state before replacing it.
  #[default]
  Any,
  /// Require the current cursor and index generation to exactly match this
  /// checkpoint. `None` requires that the source does not exist in the index.
  Exact(Option<SourceCheckpoint>),
}

impl SourceCursorPrecondition {
  /// Requires an already indexed source to still match this observed state.
  pub fn existing(source: &SourceState) -> Self {
    Self::Exact(Some(SourceCheckpoint::from(source)))
  }

  /// Requires the source not to have been indexed yet.
  pub const fn missing() -> Self {
    Self::Exact(None)
  }

  fn is_satisfied_by(&self, actual: Option<&SourceCheckpoint>) -> bool {
    match self {
      Self::Any => true,
      Self::Exact(expected) => expected.as_ref() == actual,
    }
  }
}

/// Session metadata persisted by the index.
///
/// The title and preview are catalog-owned values copied exactly as supplied by
/// the caller. The store does not sanitize, derive, or inspect either field.
/// A body completion can persist a separate fallback; read APIs prefer these
/// catalog values whenever they exist. `attention_marker` must be an opaque
/// token (for example, a message ID or fingerprint), never a message body.
/// `attention_baselined` records whether the caller has inspected the body
/// enough to establish that marker; a missing marker is meaningful only when it
/// is true. `notify_on_baseline` lets a caller defer the unread decision until
/// that first body inspection. `has_new_attention` is the separate,
/// caller-controlled signal that one or more eligible attention items
/// have been observed. Counted callers also supply `new_attention_count`. Metadata changes, marker rewrites,
/// and history reductions must leave it false.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionMetadata {
  pub key: SessionKey,
  /// The actual source path or other reopenable source locator serialized as a
  /// string. It is distinct from the source's opaque `source_key`.
  pub source_path: String,
  pub title: Option<String>,
  pub preview: Option<String>,
  /// A completed body-derived presentation carried across a source relocation.
  /// Ordinary catalog replacements leave these empty, preserving the fallback
  /// already stored for an existing row. New rows may use them when a provider
  /// moves a session to a new source before its body can be read again.
  pub body_title: Option<String>,
  pub body_preview: Option<String>,
  /// Bounded latest-assistant JSON carried across source relocation.
  pub latest_message: Option<String>,
  pub cwd: Option<String>,
  /// Provider-native creation time, retained as supplied.
  pub timestamp: Option<String>,
  /// Provider-native last-update representation, retained as supplied.
  pub updated_at: Option<String>,
  /// Canonical Unix milliseconds used for local ordering when available.
  pub updated_at_ms: Option<i64>,
  pub parent_session_id: Option<String>,
  pub agent_path: Option<String>,
  pub agent_nickname: Option<String>,
  pub agent_role: Option<String>,
  pub attention_marker: Option<String>,
  /// Whether `attention_marker` reflects a completed body inspection.
  pub attention_baselined: bool,
  /// Whether the first completed body inspection should make eligible visible
  /// conversation activity unread. This is only valid while not baselined.
  pub notify_on_baseline: bool,
  /// Advances the unread revision only when the caller observed new eligible
  /// visible conversation activity. The index cannot infer this from metadata.
  pub has_new_attention: bool,
  /// Number of newly observed replies; zero keeps the legacy single-update increment.
  pub new_attention_count: u64,
}

impl SessionMetadata {
  pub fn new(key: SessionKey, source_path: impl Into<String>) -> Self {
    Self {
      key,
      source_path: source_path.into(),
      title: None,
      preview: None,
      body_title: None,
      body_preview: None,
      latest_message: None,
      cwd: None,
      timestamp: None,
      updated_at: None,
      updated_at_ms: None,
      parent_session_id: None,
      agent_path: None,
      agent_nickname: None,
      agent_role: None,
      attention_marker: None,
      attention_baselined: true,
      notify_on_baseline: false,
      has_new_attention: false,
      new_attention_count: 0,
    }
  }
}

/// Whether a source replacement establishes a notification baseline or tracks
/// new visible conversation activity.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AttentionMode {
  /// Store incoming markers but do not leave them unread. Use for an explicit
  /// initial baseline or a deliberate acknowledgement reset.
  Baseline,
  /// Increment attention revisions only when the caller reports new eligible
  /// visible conversation activity.
  #[default]
  TrackChanges,
}

/// A complete, successfully scanned source inventory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceReplacement {
  pub source: SourceState,
  pub sessions: Vec<SessionMetadata>,
  pub attention_mode: AttentionMode,
  /// Optional optimistic guard for the source state read before scanning.
  /// Constructors default this to [`SourceCursorPrecondition::Any`] for
  /// backward-compatible convenience. New cross-process callers should set
  /// it to an exact state with [`Self::with_source_cursor_precondition`].
  pub source_cursor_precondition: SourceCursorPrecondition,
}

impl SourceReplacement {
  pub fn new(source: SourceState, sessions: Vec<SessionMetadata>) -> Self {
    Self {
      source,
      sessions,
      attention_mode: AttentionMode::TrackChanges,
      source_cursor_precondition: SourceCursorPrecondition::Any,
    }
  }

  pub fn baseline(source: SourceState, sessions: Vec<SessionMetadata>) -> Self {
    Self {
      source,
      sessions,
      attention_mode: AttentionMode::Baseline,
      source_cursor_precondition: SourceCursorPrecondition::Any,
    }
  }

  /// Adds an optimistic source-state guard to this replacement.
  ///
  /// Capture the expected cursor before scanning, then use
  /// [`SourceCursorPrecondition::existing`] for a known source or
  /// [`SourceCursorPrecondition::missing`] for a source that was absent. If
  /// another writer changes that source before this replacement commits,
  /// [`SessionIndex::replace_source`] or [`SessionIndex::replace_sources`] returns
  /// [`SessionIndexError::SourceCursorConflict`] without changing the index.
  pub fn with_source_cursor_precondition(mut self, source_cursor_precondition: SourceCursorPrecondition) -> Self {
    self.source_cursor_precondition = source_cursor_precondition;
    self
  }
}

/// Metadata read back from the index, including derived notification state.
///
/// `title` and `preview` are effective display values: a non-empty catalog
/// value wins, otherwise the completed body-derived fallback is returned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedSession {
  pub key: SessionKey,
  /// Source checkpoint read atomically with this session row.
  pub source_cursor: String,
  pub source_generation: i64,
  pub source_path: String,
  /// Effective display title: catalog value when present, otherwise body fallback.
  pub title: Option<String>,
  /// Effective display preview: catalog value when present, otherwise body fallback.
  pub preview: Option<String>,
  /// Raw title supplied by the latest provider catalog pass.
  pub catalog_title: Option<String>,
  /// Raw preview supplied by the latest provider catalog pass.
  pub catalog_preview: Option<String>,
  /// Raw fallback title captured from a completed session body.
  pub body_title: Option<String>,
  /// Raw fallback preview captured from a completed session body.
  pub body_preview: Option<String>,
  /// Bounded latest-assistant JSON carried across source relocation.
  pub latest_message: Option<String>,
  pub cwd: Option<String>,
  pub timestamp: Option<String>,
  pub updated_at: Option<String>,
  pub updated_at_ms: Option<i64>,
  pub parent_session_id: Option<String>,
  pub agent_path: Option<String>,
  pub agent_nickname: Option<String>,
  pub agent_role: Option<String>,
  pub attention_marker: Option<String>,
  /// Whether `attention_marker` reflects a completed body inspection.
  pub attention_baselined: bool,
  /// Whether a first completed body inspection should surface eligible visible
  /// conversation activity as unread.
  pub notify_on_baseline: bool,
  pub attention_revision: i64,
  pub seen_attention_revision: i64,
  pub seen_at_ms: Option<i64>,
  pub present: bool,
}

impl IndexedSession {
  pub fn has_unread(&self) -> bool {
    self.attention_revision > self.seen_attention_revision
  }
}

/// A compact count of present sessions whose attention baseline is still
/// pending for one provider.
///
/// The cursor prefix is selected by the caller because body-work staging is
/// app-specific. This summary deliberately contains no session metadata, so a
/// status surface can describe outstanding work without materializing or
/// sorting historical rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingSessionBaselineCount {
  pub provider: String,
  pub pending_sessions: usize,
}

/// A compact, per-source snapshot of staged body-baseline work.
///
/// The index intentionally leaves cursor interpretation to its caller. A
/// caller that owns a versioned staged cursor can combine its generation with
/// `pending_sessions` to recover durable progress across process restarts,
/// while callers without that convention can conservatively use
/// `present_sessions - pending_sessions`.
///
/// This is grouped by source rather than session, so a status surface can
/// inspect progress without loading or sorting historical metadata rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagedSessionBaselineSourceCount {
  pub provider: String,
  pub source_cursor: String,
  pub present_sessions: usize,
  pub pending_sessions: usize,
}

/// Result details from one committed source replacement.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReplaceSummary {
  pub inserted: usize,
  pub updated: usize,
  pub tombstoned: usize,
  pub attention_changed: usize,
  pub baseline_established: bool,
}

/// Outcome of completing one cataloged session's attention baseline.
///
/// [`Self::Stale`] is expected when a later catalog replacement changed the
/// source cursor, removed the session, or another worker already completed the
/// pending baseline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionBaselineCompletion {
  Applied {
    /// Whether this completion advanced the unread attention revision.
    attention_changed: bool,
    /// Durable source state after this completion. Callers that queue sibling
    /// work or construct a later exact precondition must use this state rather
    /// than the proposed `next_source` they passed into the completion.
    source: SourceState,
  },
  Stale,
}

impl SessionBaselineCompletion {
  pub const fn was_applied(&self) -> bool {
    matches!(self, Self::Applied { .. })
  }

  pub const fn attention_changed(&self) -> bool {
    matches!(
      self,
      Self::Applied {
        attention_changed: true,
        ..
      }
    )
  }

  /// Returns the durable source state produced by an applied completion.
  pub const fn committed_source(&self) -> Option<&SourceState> {
    match self {
      Self::Applied { source, .. } => Some(source),
      Self::Stale => None,
    }
  }
}

/// Bounded presentation metadata derived from a successfully loaded session
/// body.
///
/// This is separate from the catalog record because some providers cannot
/// expose a useful title or first-message preview without reading the session.
/// A completion only applies while its staged source cursor still matches, so
/// this data cannot overwrite a newer catalog revision. `None` fields are
/// intentional and clear a previously body-derived value.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionPresentation {
  pub title: Option<String>,
  pub preview: Option<String>,
  /// Compact latest-assistant JSON; JSON null clears a known message, while
  /// SQL NULL denotes an unknown projection. Limited to 4096 characters.
  pub latest_message: Option<String>,
}

/// Body-derived data used to complete one cataloged session baseline.
///
/// Completion intentionally does not change source-owned identity or
/// relationship metadata such as the source path, parent, timestamps, or agent
/// fields. It may atomically backfill the bounded presentation fields when a
/// provider needs a body read to derive them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionBaselineCompletionRequest {
  /// Establish a quiet baseline when changing attention accounting semantics.
  pub reset_attention: bool,
  /// Unseen replies removed by a provider history rollback.
  pub retired_attention_count: u64,
  pub key: SessionKey,
  /// Opaque body-derived marker, such as the newest eligible message ID.
  pub attention_marker: Option<String>,
  /// Optional title/preview captured from the same successfully loaded body.
  /// `None` preserves the catalog values for callers that only establish
  /// attention state.
  pub presentation: Option<SessionPresentation>,
  /// Whether the completed baseline should advance the unread revision.
  pub has_new_attention: bool,
  /// Number of newly observed replies; zero keeps the legacy single-update increment.
  pub new_attention_count: u64,
}

impl SessionBaselineCompletionRequest {
  pub fn new(key: SessionKey, attention_marker: Option<String>) -> Self {
    Self {
      key,
      reset_attention: false,
      retired_attention_count: 0,
      attention_marker,
      presentation: None,
      has_new_attention: false,
      new_attention_count: 0,
    }
  }
}

/// An app-owned SQLite session index.
///
/// All access takes a single connection lock. That deliberately serializes
/// writers and readers in-process, while SQLite WAL and the busy timeout make
/// a concurrent process fail predictably rather than corrupting cached state.
pub struct SessionIndex {
  connection: Mutex<Connection>,
}

impl fmt::Debug for SessionIndex {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("SessionIndex").finish_non_exhaustive()
  }
}

impl SessionIndex {
  /// Opens (and migrates) an app-owned index database.
  pub fn open(path: impl AsRef<Path>) -> Result<Self> {
    Self::from_connection(Connection::open(path)?)
  }

  /// Opens an in-memory index. This is useful for tests and isolated previews.
  pub fn open_in_memory() -> Result<Self> {
    Self::from_connection(Connection::open_in_memory()?)
  }

  fn from_connection(mut connection: Connection) -> Result<Self> {
    configure_connection(&mut connection)?;
    ensure_ownership(&mut connection)?;
    configure_owned_connection(&mut connection)?;
    migrate(&mut connection)?;
    Ok(Self {
      connection: Mutex::new(connection),
    })
  }

  /// Returns immutable project ordering anchors. External catalogs can register
  /// keys here too; ordinary indexed projects are registered during catalog writes.
  /// Existing projects are read without a write transaction on the refresh path.
  pub fn project_order(&self, keys: &[String], discovered_at_ms: i64) -> Result<HashMap<String, i64>> {
    let mut connection = self.connection()?;
    let mut order = {
      let mut statement = connection.prepare("SELECT project_key, sort_at_ms FROM projects")?;
      statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?
    };
    let missing = keys
      .iter()
      .filter(|key| !key.is_empty() && !order.contains_key(*key))
      .collect::<HashSet<_>>();
    if !missing.is_empty() {
      let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
      for key in missing {
        remember_project(&transaction, key, discovered_at_ms)?;
        // A concurrent indexer may have registered this key before our write lock.
        let time = transaction.query_row("SELECT sort_at_ms FROM projects WHERE project_key = ?1", [key], |row| {
          row.get(0)
        })?;
        order.insert(key.clone(), time);
      }
      transaction.commit()?;
    }
    Ok(order)
  }

  /// Shared repository identities and durable anchors for all known working directories.
  /// Resolve the complete catalog so provider/search filters cannot change identity.
  pub fn project_groups(&self, keys: &[String], discovered_at_ms: i64) -> Result<HashMap<String, ProjectGroup>> {
    let order = self.project_order(keys, discovered_at_ms)?;
    let mut connection = self.connection()?;
    let mut aliases = {
      let mut statement = connection.prepare("SELECT cwd, project_key FROM project_aliases")?;
      statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?
    };
    let mut discovered = HashMap::new();
    let mut registered = HashMap::new();
    for cwd in order.keys().filter(|cwd| !aliases.contains_key(*cwd)) {
      if let Some(root) = projects::repository_root(cwd) {
        registered.extend(projects::registered_worktrees(&root));
        discovered.insert(cwd.clone(), root.to_string_lossy().into_owned());
      }
    }
    // Cached repositories also know about subsequently removed worktrees.
    for root in aliases.values().collect::<HashSet<_>>() {
      registered.extend(projects::registered_worktrees(Path::new(root)));
    }
    // A live checkout wins over a stale worktree registration at the same path.
    for (cwd, root) in registered {
      discovered.entry(cwd).or_insert(root);
    }
    discovered.retain(|cwd, _| order.contains_key(cwd) && !aliases.contains_key(cwd));
    if !discovered.is_empty() {
      let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
      let mut anchors: HashMap<&str, i64> = HashMap::new();
      for (cwd, root) in &discovered {
        let time = order[cwd];
        anchors
          .entry(root)
          .and_modify(|anchor| *anchor = (*anchor).min(time))
          .or_insert(time);
      }
      for (root, time) in anchors {
        remember_project(&transaction, root, time)?;
      }
      for (cwd, root) in &discovered {
        transaction.execute(
          "INSERT INTO project_aliases(cwd, project_key) VALUES(?1, ?2) ON CONFLICT(cwd) DO NOTHING",
          params![cwd, root],
        )?;
      }
      transaction.commit()?;
      aliases.extend(discovered);
    }
    let mut statement = connection.prepare("SELECT project_key, sort_at_ms FROM projects")?;
    let anchors = statement
      .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?
      .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    Ok(
      order
        .into_iter()
        .map(|(cwd, time)| {
          let root = aliases.get(&cwd).cloned().unwrap_or_else(|| cwd.clone());
          let order_ms = anchors.get(&root).copied().unwrap_or(time);
          (
            cwd,
            ProjectGroup {
              project_key: root,
              order_ms,
            },
          )
        })
        .collect(),
    )
  }

  /// Returns the last successful state for one source.
  pub fn source_state(&self, key: &SourceKey) -> Result<Option<SourceState>> {
    let connection = self.connection()?;
    connection
      .query_row(
        "SELECT cursor, scanned_at_ms, generation FROM sources WHERE provider = ?1 AND source_key = ?2",
        params![key.provider, key.source_key],
        |row| {
          Ok(SourceState {
            key: key.clone(),
            cursor: row.get(0)?,
            scanned_at_ms: row.get(1)?,
            generation: row.get(2)?,
          })
        },
      )
      .optional()
      .map_err(Into::into)
  }

  /// Returns SQLite's per-connection data version for this index.
  ///
  /// The value changes when another SQLite connection commits to the same
  /// database. It is intentionally not a durable application cursor; callers
  /// use it only to notice that a sibling process updated shared index rows and
  /// should trigger a fresh read.
  pub fn data_version(&self) -> Result<i64> {
    let connection = self.connection()?;
    connection
      .pragma_query_value(None, "data_version", |row| row.get(0))
      .map_err(Into::into)
  }

  /// Lists every successfully indexed source for a provider.
  pub fn list_sources(&self, provider: &str) -> Result<Vec<SourceState>> {
    let connection = self.connection()?;
    let mut statement = connection.prepare(
      "SELECT source_key, cursor, scanned_at_ms, generation
       FROM sources
       WHERE provider = ?1
       ORDER BY source_key ASC",
    )?;
    let rows = statement.query_map(params![provider], |row| {
      Ok(SourceState {
        key: SourceKey {
          provider: provider.to_owned(),
          source_key: row.get(0)?,
        },
        cursor: row.get(1)?,
        scanned_at_ms: row.get(2)?,
        generation: row.get(3)?,
      })
    })?;
    collect_rows(rows)
  }

  /// Reports whether this provider has committed at least one successful source
  /// replacement. Apps that need whole-provider readiness should persist their
  /// own empty catalog sentinel only after every source baseline succeeds.
  pub fn has_sources_for_provider(&self, provider: &str) -> Result<bool> {
    let connection = self.connection()?;
    let exists: i64 = connection.query_row(
      "SELECT EXISTS(SELECT 1 FROM sources WHERE provider = ?1)",
      params![provider],
      |row| row.get(0),
    )?;
    Ok(exists != 0)
  }

  /// Transactionally replaces the complete inventory for one successfully
  /// scanned source.
  ///
  /// Sessions not included in `replacement.sessions` are tombstoned only after
  /// the whole transaction commits. A failed scan should never call this method.
  /// This is the single-source compatibility wrapper for
  /// [`Self::replace_sources`].
  pub fn replace_source(&self, replacement: SourceReplacement) -> Result<ReplaceSummary> {
    let mut summaries = self.replace_sources(std::slice::from_ref(&replacement))?;
    Ok(
      summaries
        .pop()
        .expect("a single source replacement must produce one summary"),
    )
  }

  /// Transactionally replaces the complete inventories for every successfully
  /// scanned source in `replacements`.
  ///
  /// The batch rejects duplicate source keys because each replacement must be
  /// one source's complete snapshot. It validates every replacement and every
  /// optimistic cursor precondition before writing any source, then commits
  /// every source update together. Therefore, a stale source anywhere in the
  /// batch leaves every source unchanged.
  ///
  /// Sessions omitted from one replacement are tombstoned only after the whole
  /// batch commits. A failed scan should never include that source in a batch.
  pub fn replace_sources(&self, replacements: &[SourceReplacement]) -> Result<Vec<ReplaceSummary>> {
    validate_replacements(replacements)?;

    let mut connection = self.connection()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    validate_source_cursor_preconditions(&transaction, replacements)?;
    let summaries = replacements
      .iter()
      .map(|replacement| replace_source_in_transaction(&transaction, replacement))
      .collect::<Result<Vec<_>>>()?;
    let mut projects = HashSet::new();
    for replacement in replacements {
      for session in &replacement.sessions {
        if let Some(key) = session.cwd.as_deref().map(str::trim).filter(|key| !key.is_empty()) {
          if projects.insert(key) {
            remember_project(&transaction, key, replacement.source.scanned_at_ms)?;
          }
        }
      }
    }
    transaction.commit()?;
    Ok(summaries)
  }

  /// Completes the pending attention baseline for exactly one cataloged
  /// session, without replacing or tombstoning sibling sessions.
  ///
  /// `expected_source` must be the staged source state captured by the catalog
  /// pass. `next_source` must have the same key and a new cursor. The completion
  /// atomically advances to that cursor so a catalog replacement still guarded
  /// by the staged cursor cannot regress the completed attention state. The
  /// completion applies only while the staged cursor still matches and the
  /// target session remains present with `attention_baselined == false`.
  /// Callers provide only body-derived attention data; catalog-owned metadata
  /// remains unchanged. A mismatched or already-completed row returns
  /// [`SessionBaselineCompletion::Stale`] rather than overwriting newer data.
  pub fn complete_session_baseline(
    &self,
    expected_source: &SourceState,
    next_source: &SourceState,
    request: SessionBaselineCompletionRequest,
  ) -> Result<SessionBaselineCompletion> {
    validate_session_baseline_completion(expected_source, next_source, &request)?;

    let mut connection = self.connection()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let source_id = transaction
      .query_row(
        "SELECT id FROM sources
         WHERE provider = ?1 AND source_key = ?2 AND cursor = ?3 AND generation = ?4",
        params![
          expected_source.key.provider,
          expected_source.key.source_key,
          expected_source.cursor,
          expected_source.generation,
        ],
        |row| row.get::<_, i64>(0),
      )
      .optional()?;
    let Some(source_id) = source_id else {
      return Ok(SessionBaselineCompletion::Stale);
    };

    let pending = pending_session_for_completion(&transaction, source_id, &request.key.session_id)?;
    let Some(pending) = pending else {
      return Ok(SessionBaselineCompletion::Stale);
    };
    if !pending.present || pending.attention_baselined {
      return Ok(SessionBaselineCompletion::Stale);
    }

    let attention_changed = request.has_new_attention;
    let attention_revision = if attention_changed {
      advance_attention_revision(pending.attention_revision, request.new_attention_count.max(1))?
    } else {
      pending.attention_revision
    };
    let committed_source = SourceState {
      key: next_source.key.clone(),
      cursor: next_source.cursor.clone(),
      scanned_at_ms: next_source.scanned_at_ms,
      generation: next_source_generation(expected_source.generation)?,
    };
    complete_session_attention_baseline(
      &transaction,
      pending.id,
      &request,
      attention_revision,
      if request.reset_attention {
        pending.attention_revision
      } else {
        pending
          .seen_attention_revision
          .saturating_add(request.retired_attention_count.min(i64::MAX as u64) as i64)
          .min(pending.attention_revision)
      },
    )?;
    advance_source_state(&transaction, source_id, &committed_source)?;
    transaction.commit()?;
    Ok(SessionBaselineCompletion::Applied {
      attention_changed,
      source: committed_source,
    })
  }

  /// Returns one session, including tombstoned rows when present in the index.
  pub fn session(&self, key: &SessionKey) -> Result<Option<IndexedSession>> {
    let connection = self.connection()?;
    select_session(
      &connection,
      "WHERE source.provider = ?1 AND source.source_key = ?2 AND session.session_id = ?3",
      params![key.provider, key.source_key, key.session_id],
    )
  }

  /// Returns a source's sole present session, or `None` when its membership
  /// is empty or ambiguous. Reads at most two rows regardless of catalog size.
  pub fn unique_present_session_for_source(&self, key: &SourceKey) -> Result<Option<IndexedSession>> {
    let connection = self.connection()?;
    let sql = format!(
      "SELECT {SESSION_COLUMNS}
       FROM sessions AS session
       INNER JOIN sources AS source ON source.id = session.source_id
       WHERE source.provider = ?1 AND source.source_key = ?2 AND session.present = 1
       LIMIT 2"
    );
    let mut statement = connection.prepare(&sql)?;
    let mut sessions =
      collect_rows(statement.query_map(params![key.provider, key.source_key], indexed_session_from_row)?)?;
    Ok((sessions.len() == 1).then(|| sessions.remove(0)))
  }

  /// Checks whether another present source in the same provider claims this
  /// identity. The identity index keeps ordinary file appends independent of
  /// the number of unrelated sessions.
  pub fn has_other_present_session_source(&self, key: &SessionKey) -> Result<bool> {
    let connection = self.connection()?;
    connection
      .query_row(
        "SELECT EXISTS(
           SELECT 1 FROM sessions AS session
           INNER JOIN sources AS source ON source.id = session.source_id
           WHERE session.session_id = ?1 AND session.present = 1
             AND source.provider = ?2 AND source.source_key != ?3
         )",
        params![key.session_id, key.provider, key.source_key],
        |row| row.get(0),
      )
      .map_err(Into::into)
  }

  /// Lists all currently present sessions in stable update order.
  pub fn list_present_sessions(&self) -> Result<Vec<IndexedSession>> {
    let connection = self.connection()?;
    select_sessions(&connection, "WHERE session.present = 1", [])
  }

  /// Lists currently present sessions for one provider in stable update order.
  ///
  /// Consumers that expose one provider at a time can use this instead of
  /// loading every provider's rows and filtering in memory.
  pub fn list_present_sessions_for_provider(&self, provider: &str) -> Result<Vec<IndexedSession>> {
    let connection = self.connection()?;
    select_sessions(
      &connection,
      "WHERE session.present = 1 AND source.provider = ?1",
      params![provider],
    )
  }

  /// Lists currently present sessions that have not completed their initial
  /// attention baseline. Viewer body backfill uses this narrow query so its
  /// idle polling cost follows the pending queue rather than total history.
  pub fn list_unbaselined_present_sessions(&self) -> Result<Vec<IndexedSession>> {
    let connection = self.connection()?;
    select_sessions(
      &connection,
      "WHERE session.present = 1 AND session.attention_baselined = 0",
      [],
    )
  }

  /// Lists every indexed session, including tombstoned rows.
  pub fn list_all_sessions(&self) -> Result<Vec<IndexedSession>> {
    let connection = self.connection()?;
    select_sessions(&connection, "", [])
  }

  /// Lists one provider's complete inventory, including tombstones needed
  /// for relocation and attention recovery, in the usual stable update order.
  pub fn list_all_sessions_for_provider(&self, provider: &str) -> Result<Vec<IndexedSession>> {
    let connection = self.connection()?;
    select_sessions(&connection, "WHERE source.provider = ?1", [provider])
  }

  /// Counts present, unbaselined sessions by provider for sources whose
  /// opaque cursor begins with one of `source_cursor_prefixes`.
  ///
  /// This is intentionally a grouped aggregate rather than a session listing:
  /// callers can initialize a progress indicator without allocating or
  /// sorting all historical rows. Empty prefixes produce an empty summary.
  pub fn pending_session_baseline_counts(
    &self,
    source_cursor_prefixes: &[&str],
  ) -> Result<Vec<PendingSessionBaselineCount>> {
    if source_cursor_prefixes.is_empty() {
      return Ok(Vec::new());
    }

    let cursor_matches = source_cursor_prefixes
      .iter()
      .enumerate()
      .map(|(index, _)| {
        let parameter = index + 1;
        format!("substr(source.cursor, 1, length(?{parameter})) = ?{parameter}")
      })
      .collect::<Vec<_>>()
      .join(" OR ");
    let statement = format!(
      "SELECT source.provider, COUNT(*)
       FROM sessions AS session
       INNER JOIN sources AS source ON source.id = session.source_id
       WHERE session.present = 1
         AND session.attention_baselined = 0
         AND ({cursor_matches})
       GROUP BY source.provider
       ORDER BY source.provider ASC"
    );
    let connection = self.connection()?;
    let mut statement = connection.prepare(&statement)?;
    let rows = statement.query_map(params_from_iter(source_cursor_prefixes.iter()), |row| {
      let pending_sessions = row.get::<_, i64>(1)?;
      Ok(PendingSessionBaselineCount {
        provider: row.get(0)?,
        pending_sessions: usize::try_from(pending_sessions)
          .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, pending_sessions))?,
      })
    })?;
    collect_rows(rows)
  }

  /// Counts present and unbaselined sessions for every staged source whose
  /// opaque cursor begins with one of `source_cursor_prefixes`.
  ///
  /// Unlike [`Self::pending_session_baseline_counts`], this preserves each
  /// source cursor. The caller owns the cursor schema and can therefore
  /// recover application-specific cumulative progress without this generic
  /// index interpreting opaque provider state.
  pub fn staged_session_baseline_source_counts(
    &self,
    source_cursor_prefixes: &[&str],
  ) -> Result<Vec<StagedSessionBaselineSourceCount>> {
    if source_cursor_prefixes.is_empty() {
      return Ok(Vec::new());
    }

    let cursor_matches = source_cursor_prefixes
      .iter()
      .enumerate()
      .map(|(index, _)| {
        let parameter = index + 1;
        format!("substr(source.cursor, 1, length(?{parameter})) = ?{parameter}")
      })
      .collect::<Vec<_>>()
      .join(" OR ");
    let statement = format!(
      "SELECT source.provider,
              source.cursor,
              COUNT(*),
              SUM(CASE WHEN session.attention_baselined = 0 THEN 1 ELSE 0 END)
       FROM sessions AS session
       INNER JOIN sources AS source ON source.id = session.source_id
       WHERE session.present = 1
         AND ({cursor_matches})
       GROUP BY source.id
       ORDER BY source.provider ASC, source.source_key ASC"
    );
    let connection = self.connection()?;
    let mut statement = connection.prepare(&statement)?;
    let rows = statement.query_map(params_from_iter(source_cursor_prefixes.iter()), |row| {
      let present_sessions = row.get::<_, i64>(2)?;
      let pending_sessions = row.get::<_, i64>(3)?;
      Ok(StagedSessionBaselineSourceCount {
        provider: row.get(0)?,
        source_cursor: row.get(1)?,
        present_sessions: usize::try_from(present_sessions)
          .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(2, present_sessions))?,
        pending_sessions: usize::try_from(pending_sessions)
          .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(3, pending_sessions))?,
      })
    })?;
    collect_rows(rows)
  }

  /// Acknowledges attention only through the revision captured by a successful
  /// event-page response. Call this only after the app has accepted that page.
  ///
  /// The acknowledgement is monotonic and clamps to the current revision, so a
  /// stale or cancelled page request cannot erase newer attention indexed while
  /// it was loading. Returns whether the seen revision actually advanced.
  pub fn mark_seen_through(&self, key: &SessionKey, attention_revision: i64, seen_at_ms: i64) -> Result<bool> {
    if attention_revision < 0 {
      return Err(SessionIndexError::InvalidReplacement(
        "seen attention revision cannot be negative".to_owned(),
      ));
    }
    let connection = self.connection()?;
    let updated = connection.execute(
      "UPDATE sessions
       SET seen_attention_revision = MIN(attention_revision, ?4),
           seen_at_ms = ?5
       WHERE id = (
         SELECT session.id
         FROM sessions AS session
         INNER JOIN sources AS source ON source.id = session.source_id
         WHERE source.provider = ?1
           AND source.source_key = ?2
           AND session.session_id = ?3
       )
       AND seen_attention_revision < MIN(attention_revision, ?4)",
      params![
        key.provider,
        key.source_key,
        key.session_id,
        attention_revision,
        seen_at_ms
      ],
    )?;
    Ok(updated != 0)
  }

  fn connection(&self) -> Result<MutexGuard<'_, Connection>> {
    self
      .connection
      .lock()
      .map_err(|_| SessionIndexError::ConnectionPoisoned)
  }
}

fn configure_connection(connection: &mut Connection) -> Result<()> {
  connection.busy_timeout(BUSY_TIMEOUT)?;
  Ok(())
}

fn configure_owned_connection(connection: &mut Connection) -> Result<()> {
  connection.pragma_update(None, "foreign_keys", "ON")?;
  // Switching a database into WAL mode needs an exclusive SQLite lock. Two
  // viewer processes can arrive here just after ownership is established, so
  // retry the transient lock rather than making one startup fail.
  let deadline = Instant::now() + BUSY_TIMEOUT;
  loop {
    match connection.pragma_update(None, "journal_mode", "WAL") {
      Ok(()) => break,
      Err(error) if sqlite_lock_is_transient(&error) && Instant::now() < deadline => {
        std::thread::sleep(Duration::from_millis(10));
      }
      Err(error) => return Err(error.into()),
    }
  }
  Ok(())
}

fn sqlite_lock_is_transient(error: &rusqlite::Error) -> bool {
  matches!(
    error,
    rusqlite::Error::SqliteFailure(code, _)
      if matches!(code.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
  )
}

fn ensure_ownership(connection: &mut Connection) -> Result<()> {
  let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
  let application_id: i32 = transaction.pragma_query_value(None, "application_id", |row| row.get(0))?;
  if application_id == APPLICATION_ID {
    transaction.commit()?;
    return Ok(());
  }
  if application_id != 0 {
    return Err(SessionIndexError::UnexpectedDatabase(application_id));
  }

  let existing_object_count: i64 = transaction.query_row(
    "SELECT COUNT(*)
     FROM sqlite_schema
     WHERE name NOT LIKE 'sqlite_%'",
    [],
    |row| row.get(0),
  )?;
  if existing_object_count != 0 {
    return Err(SessionIndexError::UnexpectedDatabase(application_id));
  }
  transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
  transaction.commit()?;
  Ok(())
}

fn migrate(connection: &mut Connection) -> Result<()> {
  // Hold one write transaction from ledger creation through every pending
  // migration. A second viewer process otherwise can read the same old ledger
  // just before the first process applies an ALTER TABLE, then fail on a
  // duplicate column after it acquires its own transaction.
  let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
  transaction.execute_batch(
    "CREATE TABLE IF NOT EXISTS schema_migrations (
       version INTEGER PRIMARY KEY,
       checksum TEXT NOT NULL,
       applied_at_ms INTEGER NOT NULL
     );",
  )?;

  let applied = {
    let mut statement = transaction.prepare("SELECT version, checksum FROM schema_migrations ORDER BY version ASC")?;
    let rows = statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?;
    collect_rows(rows)?
  };

  if applied.len() > MIGRATIONS.len() {
    return Err(SessionIndexError::MigrationHistoryInvalid(format!(
      "database has {} migrations but this build knows {}",
      applied.len(),
      MIGRATIONS.len()
    )));
  }

  for (index, (version, found_checksum)) in applied.iter().enumerate() {
    let migration = MIGRATIONS
      .get(index)
      .ok_or_else(|| SessionIndexError::MigrationHistoryInvalid(format!("unexpected migration {version}")))?;
    if *version != migration.version {
      return Err(SessionIndexError::MigrationHistoryInvalid(format!(
        "expected migration {} at position {}, found {version}",
        migration.version,
        index + 1
      )));
    }

    let expected_checksum = migration_checksum(migration.sql);
    if *found_checksum != expected_checksum {
      return Err(SessionIndexError::MigrationChecksumMismatch {
        version: *version,
        expected: expected_checksum,
        found: found_checksum.clone(),
      });
    }
  }

  for migration in MIGRATIONS.iter().skip(applied.len()) {
    transaction.execute_batch(migration.sql)?;
    transaction.execute(
      "INSERT INTO schema_migrations(version, checksum, applied_at_ms) VALUES(?1, ?2, ?3)",
      params![migration.version, migration_checksum(migration.sql), current_time_ms()],
    )?;
  }

  transaction.commit()?;
  Ok(())
}

fn migration_checksum(sql: &str) -> String {
  // FNV-1a is sufficient here as an immutable-migration drift guard and avoids
  // bringing a cryptographic dependency into every app that uses the index.
  let mut hash = 0xcbf2_9ce4_8422_2325_u64;
  for byte in sql.bytes() {
    hash ^= u64::from(byte);
    hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
  }
  format!("{hash:016x}")
}

fn current_time_ms() -> i64 {
  let milliseconds = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap_or_default()
    .as_millis();
  i64::try_from(milliseconds).unwrap_or(i64::MAX)
}

fn validate_replacement(replacement: &SourceReplacement) -> Result<()> {
  let mut session_ids = HashSet::with_capacity(replacement.sessions.len());
  for session in &replacement.sessions {
    if session.key.provider != replacement.source.key.provider
      || session.key.source_key != replacement.source.key.source_key
    {
      return Err(SessionIndexError::InvalidReplacement(format!(
        "session {} does not belong to source {}/{}",
        session.key.session_id, replacement.source.key.provider, replacement.source.key.source_key
      )));
    }
    if session.attention_baselined && session.notify_on_baseline {
      return Err(SessionIndexError::InvalidReplacement(format!(
        "session {} cannot notify on an already established attention baseline",
        session.key.session_id
      )));
    }
    if !session_ids.insert(session.key.session_id.as_str()) {
      return Err(SessionIndexError::InvalidReplacement(format!(
        "source contains duplicate session id {}",
        session.key.session_id
      )));
    }
  }
  Ok(())
}

fn validate_replacements(replacements: &[SourceReplacement]) -> Result<()> {
  let mut source_keys = HashSet::with_capacity(replacements.len());
  for replacement in replacements {
    validate_replacement(replacement)?;
    if !source_keys.insert(&replacement.source.key) {
      return Err(SessionIndexError::InvalidReplacement(format!(
        "batch contains duplicate source {}/{}",
        replacement.source.key.provider, replacement.source.key.source_key
      )));
    }
  }
  Ok(())
}

fn validate_session_baseline_completion(
  expected_source: &SourceState,
  next_source: &SourceState,
  request: &SessionBaselineCompletionRequest,
) -> Result<()> {
  if request.key.provider != expected_source.key.provider || request.key.source_key != expected_source.key.source_key {
    return Err(SessionIndexError::InvalidReplacement(format!(
      "session {} does not belong to staged source {}/{}",
      request.key.session_id, expected_source.key.provider, expected_source.key.source_key
    )));
  }
  if next_source.key != expected_source.key {
    return Err(SessionIndexError::InvalidReplacement(format!(
      "next source {}/{} does not match staged source {}/{}",
      next_source.key.provider,
      next_source.key.source_key,
      expected_source.key.provider,
      expected_source.key.source_key
    )));
  }
  if next_source.cursor == expected_source.cursor {
    return Err(SessionIndexError::InvalidReplacement(format!(
      "next source cursor for {}/{} must advance beyond the staged cursor",
      expected_source.key.provider, expected_source.key.source_key
    )));
  }
  Ok(())
}

fn validate_source_cursor_preconditions(
  transaction: &Transaction<'_>,
  replacements: &[SourceReplacement],
) -> Result<()> {
  for replacement in replacements {
    let actual = transaction
      .query_row(
        "SELECT cursor, generation FROM sources WHERE provider = ?1 AND source_key = ?2",
        params![replacement.source.key.provider, replacement.source.key.source_key],
        |row| {
          Ok(SourceCheckpoint {
            cursor: row.get(0)?,
            generation: row.get(1)?,
          })
        },
      )
      .optional()?;
    if !replacement.source_cursor_precondition.is_satisfied_by(actual.as_ref()) {
      return Err(SessionIndexError::SourceCursorConflict {
        source: replacement.source.key.clone(),
        expected: replacement.source_cursor_precondition.clone(),
        actual,
      });
    }
  }
  Ok(())
}

fn replace_source_in_transaction(
  transaction: &Transaction<'_>,
  replacement: &SourceReplacement,
) -> Result<ReplaceSummary> {
  let existing_source = transaction
    .query_row(
      "SELECT id, generation FROM sources WHERE provider = ?1 AND source_key = ?2",
      params![replacement.source.key.provider, replacement.source.key.source_key],
      |row| {
        Ok(ExistingSource {
          id: row.get(0)?,
          generation: row.get(1)?,
        })
      },
    )
    .optional()?;

  let baseline_established = replacement.attention_mode == AttentionMode::Baseline;
  let source_id = match existing_source {
    Some(source) => {
      transaction.execute(
        "UPDATE sources
         SET cursor = ?3,
             scanned_at_ms = ?4,
             updated_at_ms = ?4,
             generation = ?5
         WHERE provider = ?1 AND source_key = ?2",
        params![
          replacement.source.key.provider,
          replacement.source.key.source_key,
          replacement.source.cursor,
          replacement.source.scanned_at_ms,
          next_source_generation(source.generation)?,
        ],
      )?;
      source.id
    }
    None => {
      transaction.execute(
        "INSERT INTO sources(
           provider, source_key, cursor, scanned_at_ms, created_at_ms, updated_at_ms, generation
         ) VALUES(?1, ?2, ?3, ?4, ?4, ?4, 1)",
        params![
          replacement.source.key.provider,
          replacement.source.key.source_key,
          replacement.source.cursor,
          replacement.source.scanned_at_ms,
        ],
      )?;
      transaction.last_insert_rowid()
    }
  };

  let previously_present = {
    let mut statement = transaction.prepare("SELECT session_id FROM sessions WHERE source_id = ?1 AND present = 1")?;
    let rows = statement.query_map(params![source_id], |row| row.get::<_, String>(0))?;
    collect_rows(rows)?.into_iter().collect::<HashSet<_>>()
  };
  let incoming_session_ids = replacement
    .sessions
    .iter()
    .map(|session| session.key.session_id.as_str())
    .collect::<HashSet<_>>();
  let tombstoned = previously_present
    .iter()
    .filter(|session_id| !incoming_session_ids.contains(session_id.as_str()))
    .count();

  // Marking before upsert keeps the tombstone statement simple. The surrounding
  // transaction means readers observe either the old complete inventory or the
  // new complete inventory, never this temporary state.
  transaction.execute(
    "UPDATE sessions SET present = 0, indexed_at_ms = ?2 WHERE source_id = ?1 AND present = 1",
    params![source_id, replacement.source.scanned_at_ms],
  )?;

  let mut summary = ReplaceSummary {
    tombstoned,
    baseline_established,
    ..ReplaceSummary::default()
  };
  for session in &replacement.sessions {
    let existing = existing_session_attention(transaction, source_id, &session.key.session_id)?;
    match existing {
      Some(existing) => {
        summary.updated += 1;
        let attention_revision = if session.has_new_attention && !baseline_established {
          let revision = advance_attention_revision(existing.attention_revision, session.new_attention_count.max(1))?;
          summary.attention_changed += 1;
          revision
        } else {
          existing.attention_revision
        };
        let seen_attention_revision = if baseline_established {
          attention_revision
        } else {
          existing.seen_attention_revision
        };
        update_session(
          transaction,
          existing.id,
          session,
          attention_revision,
          seen_attention_revision,
          replacement.source.scanned_at_ms,
        )?;
      }
      None => {
        summary.inserted += 1;
        let attention_revision = if session.has_new_attention && !baseline_established {
          advance_attention_revision(0, session.new_attention_count.max(1))?
        } else {
          0
        };
        let seen_attention_revision = 0;
        if session.has_new_attention && !baseline_established {
          summary.attention_changed += 1;
        }
        insert_session(
          transaction,
          source_id,
          session,
          attention_revision,
          seen_attention_revision,
          replacement.source.scanned_at_ms,
        )?;
      }
    }
  }

  Ok(summary)
}

#[derive(Debug)]
struct ExistingSource {
  id: i64,
  generation: i64,
}

#[derive(Debug)]
struct ExistingSessionAttention {
  id: i64,
  attention_revision: i64,
  seen_attention_revision: i64,
}

#[derive(Debug)]
struct PendingSessionCompletion {
  id: i64,
  attention_revision: i64,
  seen_attention_revision: i64,
  present: bool,
  attention_baselined: bool,
}

fn existing_session_attention(
  transaction: &Transaction<'_>,
  source_id: i64,
  session_id: &str,
) -> Result<Option<ExistingSessionAttention>> {
  transaction
    .query_row(
      "SELECT id, attention_revision, seen_attention_revision
       FROM sessions
       WHERE source_id = ?1 AND session_id = ?2",
      params![source_id, session_id],
      |row| {
        Ok(ExistingSessionAttention {
          id: row.get(0)?,
          attention_revision: row.get(1)?,
          seen_attention_revision: row.get(2)?,
        })
      },
    )
    .optional()
    .map_err(Into::into)
}

fn pending_session_for_completion(
  transaction: &Transaction<'_>,
  source_id: i64,
  session_id: &str,
) -> Result<Option<PendingSessionCompletion>> {
  transaction
    .query_row(
      "SELECT id, attention_revision, seen_attention_revision, present, attention_baselined
       FROM sessions
       WHERE source_id = ?1 AND session_id = ?2",
      params![source_id, session_id],
      |row| {
        Ok(PendingSessionCompletion {
          id: row.get(0)?,
          attention_revision: row.get(1)?,
          seen_attention_revision: row.get(2)?,
          present: row.get(3)?,
          attention_baselined: row.get(4)?,
        })
      },
    )
    .optional()
    .map_err(Into::into)
}

fn complete_session_attention_baseline(
  transaction: &Transaction<'_>,
  id: i64,
  request: &SessionBaselineCompletionRequest,
  attention_revision: i64,
  seen_attention_revision: i64,
) -> Result<()> {
  if let Some(presentation) = &request.presentation {
    transaction.execute(
      "UPDATE sessions
       SET attention_marker = ?2,
           attention_baselined = 1,
           notify_on_baseline = 0,
           attention_revision = ?3,
           seen_attention_revision = ?4,
           body_title = ?5,
           body_preview = ?6,
           latest_message = ?7
       WHERE id = ?1",
      params![
        id,
        request.attention_marker,
        attention_revision,
        seen_attention_revision,
        presentation.title,
        presentation.preview,
        presentation.latest_message,
      ],
    )?;
  } else {
    transaction.execute(
      "UPDATE sessions
       SET attention_marker = ?2,
           attention_baselined = 1,
           notify_on_baseline = 0,
           attention_revision = ?3,
           seen_attention_revision = ?4
       WHERE id = ?1",
      params![
        id,
        request.attention_marker,
        attention_revision,
        seen_attention_revision,
      ],
    )?;
  }
  Ok(())
}

fn advance_source_state(transaction: &Transaction<'_>, source_id: i64, next_source: &SourceState) -> Result<()> {
  transaction.execute(
    "UPDATE sources
     SET cursor = ?2,
         scanned_at_ms = ?3,
         updated_at_ms = ?3,
         generation = ?4
     WHERE id = ?1",
    params![
      source_id,
      next_source.cursor,
      next_source.scanned_at_ms,
      next_source.generation,
    ],
  )?;
  Ok(())
}

fn next_source_generation(generation: i64) -> Result<i64> {
  generation
    .checked_add(1)
    .ok_or_else(|| SessionIndexError::InvalidReplacement("source generation overflow".to_owned()))
}

fn advance_attention_revision(revision: i64, count: u64) -> Result<i64> {
  let count = i64::try_from(count).map_err(|_| SessionIndexError::AttentionRevisionOverflow)?;
  revision
    .checked_add(count)
    .ok_or(SessionIndexError::AttentionRevisionOverflow)
}

fn remember_project(transaction: &Transaction<'_>, key: &str, discovered_at_ms: i64) -> Result<()> {
  transaction.execute(
    "INSERT INTO projects(project_key, discovered_at_ms, sort_at_ms) VALUES(?1, ?2, ?2)
     ON CONFLICT(project_key) DO NOTHING",
    params![key, discovered_at_ms],
  )?;
  Ok(())
}

fn insert_session(
  transaction: &Transaction<'_>,
  source_id: i64,
  session: &SessionMetadata,
  attention_revision: i64,
  seen_attention_revision: i64,
  indexed_at_ms: i64,
) -> Result<()> {
  transaction.execute(
    "INSERT INTO sessions(
       source_id, session_id, source_path, title, preview, body_title, body_preview, cwd, timestamp,
       updated_at, updated_at_ms, parent_session_id, agent_path, agent_nickname,
       agent_role, attention_marker, attention_baselined, notify_on_baseline,
       attention_revision, seen_attention_revision, seen_at_ms, present,
       first_indexed_at_ms, indexed_at_ms, latest_message
     ) VALUES(
       ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
       ?17, ?18, ?19, ?20, NULL, 1, ?21, ?21, ?22
     )",
    params![
      source_id,
      session.key.session_id,
      session.source_path,
      session.title,
      session.preview,
      session.body_title,
      session.body_preview,
      session.cwd,
      session.timestamp,
      session.updated_at,
      session.updated_at_ms,
      session.parent_session_id,
      session.agent_path,
      session.agent_nickname,
      session.agent_role,
      session.attention_marker,
      session.attention_baselined,
      session.notify_on_baseline,
      attention_revision,
      seen_attention_revision,
      indexed_at_ms,
      session.latest_message,
    ],
  )?;
  Ok(())
}

fn update_session(
  transaction: &Transaction<'_>,
  id: i64,
  session: &SessionMetadata,
  attention_revision: i64,
  seen_attention_revision: i64,
  indexed_at_ms: i64,
) -> Result<()> {
  transaction.execute(
    "UPDATE sessions
     SET source_path = ?2,
         title = ?3,
         preview = ?4,
         body_title = COALESCE(?5, body_title),
         body_preview = COALESCE(?6, body_preview),
         cwd = ?7,
         timestamp = ?8,
         updated_at = ?9,
         updated_at_ms = ?10,
         parent_session_id = ?11,
         agent_path = ?12,
         agent_nickname = ?13,
         agent_role = ?14,
         attention_marker = ?15,
         attention_baselined = ?16,
         notify_on_baseline = ?17,
         attention_revision = ?18,
         seen_attention_revision = ?19,
         present = 1,
         indexed_at_ms = ?20,
         latest_message = COALESCE(?21, latest_message)
     WHERE id = ?1",
    params![
      id,
      session.source_path,
      session.title,
      session.preview,
      session.body_title,
      session.body_preview,
      session.cwd,
      session.timestamp,
      session.updated_at,
      session.updated_at_ms,
      session.parent_session_id,
      session.agent_path,
      session.agent_nickname,
      session.agent_role,
      session.attention_marker,
      session.attention_baselined,
      session.notify_on_baseline,
      attention_revision,
      seen_attention_revision,
      indexed_at_ms,
      session.latest_message,
    ],
  )?;
  Ok(())
}

const SESSION_COLUMNS: &str = "
  source.provider,
  source.source_key,
  session.session_id,
  session.source_path,
  COALESCE(session.title, session.body_title),
  COALESCE(session.preview, session.body_preview),
  session.title,
  session.preview,
  session.body_title,
  session.body_preview,
  session.cwd,
  session.timestamp,
  session.updated_at,
  session.updated_at_ms,
  session.parent_session_id,
  session.agent_path,
  session.agent_nickname,
  session.agent_role,
  session.attention_marker,
  session.attention_baselined,
  session.notify_on_baseline,
  session.attention_revision,
  session.seen_attention_revision,
  session.seen_at_ms,
  session.present,
  session.latest_message,
  source.cursor,
  source.generation";

fn select_session<P>(connection: &Connection, where_clause: &str, parameters: P) -> Result<Option<IndexedSession>>
where
  P: rusqlite::Params,
{
  let sql = format!(
    "SELECT {SESSION_COLUMNS}
     FROM sessions AS session
     INNER JOIN sources AS source ON source.id = session.source_id
     {where_clause}"
  );
  connection
    .query_row(&sql, parameters, indexed_session_from_row)
    .optional()
    .map_err(Into::into)
}

fn select_sessions<P>(connection: &Connection, where_clause: &str, parameters: P) -> Result<Vec<IndexedSession>>
where
  P: rusqlite::Params,
{
  let sql = format!(
    "SELECT {SESSION_COLUMNS}
     FROM sessions AS session
     INNER JOIN sources AS source ON source.id = session.source_id
     {where_clause}
     ORDER BY
       CASE WHEN session.updated_at_ms IS NULL THEN 1 ELSE 0 END ASC,
       session.updated_at_ms DESC,
       session.session_id ASC,
       source.provider ASC,
       source.source_key ASC"
  );
  let mut statement = connection.prepare(&sql)?;
  let rows = statement.query_map(parameters, indexed_session_from_row)?;
  collect_rows(rows)
}

fn indexed_session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexedSession> {
  let present: i64 = row.get(24)?;
  Ok(IndexedSession {
    key: SessionKey {
      provider: row.get(0)?,
      source_key: row.get(1)?,
      session_id: row.get(2)?,
    },
    source_path: row.get(3)?,
    title: row.get(4)?,
    preview: row.get(5)?,
    catalog_title: row.get(6)?,
    catalog_preview: row.get(7)?,
    body_title: row.get(8)?,
    body_preview: row.get(9)?,
    cwd: row.get(10)?,
    timestamp: row.get(11)?,
    updated_at: row.get(12)?,
    updated_at_ms: row.get(13)?,
    parent_session_id: row.get(14)?,
    agent_path: row.get(15)?,
    agent_nickname: row.get(16)?,
    agent_role: row.get(17)?,
    attention_marker: row.get(18)?,
    attention_baselined: row.get(19)?,
    notify_on_baseline: row.get(20)?,
    attention_revision: row.get(21)?,
    seen_attention_revision: row.get(22)?,
    seen_at_ms: row.get(23)?,
    present: present != 0,
    latest_message: row.get(25)?,
    source_cursor: row.get(26)?,
    source_generation: row.get(27)?,
  })
}

fn collect_rows<T>(
  rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> Result<Vec<T>> {
  rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

#[cfg(test)]
mod tests {
  use rusqlite::Connection;
  use tempfile::tempdir;

  use super::*;

  const PROVIDER: &str = "codex";
  const SOURCE_KEY: &str = "source-a";

  fn source(cursor: &str, scanned_at_ms: i64) -> SourceState {
    source_for(PROVIDER, SOURCE_KEY, cursor, scanned_at_ms)
  }

  fn source_for(provider: &str, source_key: &str, cursor: &str, scanned_at_ms: i64) -> SourceState {
    SourceState::new(SourceKey::new(provider, source_key), cursor, scanned_at_ms)
  }

  fn session(id: &str, marker: Option<&str>) -> SessionMetadata {
    session_for(PROVIDER, SOURCE_KEY, id, marker)
  }

  fn session_for(provider: &str, source_key: &str, id: &str, marker: Option<&str>) -> SessionMetadata {
    let mut session = SessionMetadata::new(
      SessionKey::new(provider, source_key, id),
      format!("/sessions/{id}.jsonl"),
    );
    session.title = Some(format!("Title {id}"));
    session.preview = Some(format!("Preview {id}"));
    session.timestamp = Some("2026-09-01T00:00:00Z".to_owned());
    session.updated_at = Some("provider-update".to_owned());
    session.updated_at_ms = Some(1_000);
    session.attention_marker = marker.map(str::to_owned);
    session
  }

  fn replacement(cursor: &str, scanned_at_ms: i64, sessions: Vec<SessionMetadata>) -> SourceReplacement {
    SourceReplacement::new(source(cursor, scanned_at_ms), sessions)
  }

  fn baseline_replacement(cursor: &str, scanned_at_ms: i64, sessions: Vec<SessionMetadata>) -> SourceReplacement {
    SourceReplacement::baseline(source(cursor, scanned_at_ms), sessions)
  }

  fn pending_session(id: &str, notify_on_baseline: bool) -> SessionMetadata {
    let mut session = session(id, None);
    session.attention_baselined = false;
    session.notify_on_baseline = notify_on_baseline;
    session
  }

  fn completion_request(id: &str, marker: Option<&str>, has_new_attention: bool) -> SessionBaselineCompletionRequest {
    let mut request =
      SessionBaselineCompletionRequest::new(SessionKey::new(PROVIDER, SOURCE_KEY, id), marker.map(str::to_owned));
    request.has_new_attention = has_new_attention;
    request
  }

  fn completion_request_with_presentation(
    id: &str,
    marker: Option<&str>,
    title: Option<&str>,
    preview: Option<&str>,
  ) -> SessionBaselineCompletionRequest {
    let mut request = completion_request(id, marker, false);
    request.presentation = Some(SessionPresentation {
      title: title.map(str::to_owned),
      preview: preview.map(str::to_owned),
      latest_message: None,
    });
    request
  }

  #[test]
  fn counted_replies_survive_reopen_and_stale_acknowledgements() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("counts.sqlite");
    let key = SessionKey::new(PROVIDER, SOURCE_KEY, "a");
    let index = SessionIndex::open(&path).unwrap();
    index
      .replace_source(SourceReplacement::baseline(
        source("baseline", 0),
        vec![session("a", Some("zero"))],
      ))
      .unwrap();
    let mut unread = session("a", Some("three"));
    unread.has_new_attention = true;
    unread.new_attention_count = 3;
    index.replace_source(replacement("three", 1, vec![unread])).unwrap();
    assert_eq!(index.session(&key).unwrap().unwrap().attention_revision, 3);
    let pending = pending_session("a", false);
    index.replace_source(replacement("pending", 2, vec![pending])).unwrap();
    let source = index.source_state(&key.source_key()).unwrap().unwrap();
    let mut completed = completion_request("a", Some("five"), true);
    completed.new_attention_count = 2;
    index
      .complete_session_baseline(&source, &SourceState::new(source.key.clone(), "done", 3), completed)
      .unwrap();
    assert!(index.mark_seen_through(&key, 3, 4).unwrap());
    drop(index);
    let index = SessionIndex::open(&path).unwrap();
    let current = index.session(&key).unwrap().unwrap();
    assert_eq!(current.attention_revision, 5);
    assert_eq!(current.attention_revision - current.seen_attention_revision, 2);
    assert!(!index.mark_seen_through(&key, 2, 5).unwrap());
  }

  #[test]
  fn rollback_and_accounting_upgrade_retire_unread_without_rewinding_revisions() {
    let index = SessionIndex::open_in_memory().unwrap();
    let key = SessionKey::new(PROVIDER, SOURCE_KEY, "a");
    index
      .replace_source(SourceReplacement::baseline(
        source("baseline", 0),
        vec![session("a", None)],
      ))
      .unwrap();
    let mut unread = pending_session("a", false);
    unread.has_new_attention = true;
    unread.new_attention_count = 4;
    index.replace_source(replacement("pending", 1, vec![unread])).unwrap();
    let source = index.source_state(&key.source_key()).unwrap().unwrap();
    let mut request = completion_request("a", Some("remaining"), false);
    request.retired_attention_count = 2;
    index
      .complete_session_baseline(&source, &SourceState::new(source.key.clone(), "done", 2), request)
      .unwrap();
    let row = index.session(&key).unwrap().unwrap();
    assert_eq!((row.attention_revision, row.seen_attention_revision), (4, 2));
    index
      .replace_source(replacement("upgrade", 3, vec![pending_session("a", false)]))
      .unwrap();
    let source = index.source_state(&key.source_key()).unwrap().unwrap();
    let mut request = completion_request("a", Some("new accounting baseline"), false);
    request.reset_attention = true;
    index
      .complete_session_baseline(&source, &SourceState::new(source.key.clone(), "done", 4), request)
      .unwrap();
    let row = index.session(&key).unwrap().unwrap();
    assert_eq!((row.attention_revision, row.seen_attention_revision), (4, 4));
  }

  #[test]
  fn worktrees_and_subdirectories_share_a_persistent_project() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("main/repo");
    let checkout = directory.path().join("worktrees/repo");
    let other = directory.path().join("unrelated/repo");
    let registration = root.join(".git/worktrees/task");
    std::fs::create_dir_all(&registration).unwrap();
    std::fs::create_dir_all(checkout.join("src")).unwrap();
    std::fs::create_dir_all(other.join(".git")).unwrap();
    std::fs::write(checkout.join(".git"), format!("gitdir: {}", registration.display())).unwrap();
    std::fs::write(registration.join("commondir"), "../..").unwrap();
    std::fs::write(
      registration.join("gitdir"),
      checkout.join(".git").to_string_lossy().as_bytes(),
    )
    .unwrap();
    let root = root.canonicalize().unwrap().to_string_lossy().into_owned();
    let checkout_key = checkout.to_string_lossy().into_owned();
    let nested = checkout.join("src").to_string_lossy().into_owned();
    let other = other.canonicalize().unwrap().to_string_lossy().into_owned();
    let path = directory.path().join("index.sqlite");
    let index = SessionIndex::open(&path).unwrap();
    index.project_order(&[root.clone()], 10).unwrap();
    let groups = index
      .project_groups(&[checkout_key.clone(), nested.clone(), other.clone()], 20)
      .unwrap();
    assert_eq!(groups[&checkout_key].project_key, root);
    assert_eq!(groups[&nested].project_key, root);
    assert_eq!(groups[&checkout_key].order_ms, 10);
    assert_eq!(groups[&other].project_key, other);
    drop(index);
    std::fs::remove_dir_all(checkout).unwrap();
    std::fs::remove_dir_all(registration).unwrap();
    let index = SessionIndex::open(&path).unwrap();
    let groups = index.project_groups(&[nested.clone()], 30).unwrap();
    assert_eq!(groups[&nested].project_key, root);
    assert_eq!(groups[&nested].order_ms, 10);
  }

  #[test]
  fn catalog_project_anchor_survives_removal_and_older_history() {
    let index = SessionIndex::open_in_memory().unwrap();
    let mut row = session("a", None);
    row.cwd = Some(" /project ".into());
    index
      .replace_source(baseline_replacement("one", 100, vec![row.clone()]))
      .unwrap();
    index.replace_source(replacement("two", 200, vec![])).unwrap();
    row.timestamp = Some("2000-01-01T00:00:00Z".into());
    index.replace_source(replacement("three", 300, vec![row])).unwrap();
    assert_eq!(index.project_order(&[], 400).unwrap()["/project"], 100);
  }

  #[test]
  fn project_discovery_order_survives_reopen_and_repeated_discovery() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("projects.sqlite3");
    let index = SessionIndex::open(&path).unwrap();
    assert_eq!(index.project_order(&["/a/repo".into()], 100).unwrap()["/a/repo"], 100);
    drop(index);
    let index = SessionIndex::open(&path).unwrap();
    let projects = index.project_order(&["/a/repo".into(), "/b/repo".into()], 200).unwrap();
    assert_eq!(projects["/a/repo"], 100);
    assert_eq!(projects["/b/repo"], 200);
    assert_eq!(index.project_order(&["/a/repo".into()], 1).unwrap()["/a/repo"], 100);
  }

  #[test]
  fn creates_and_records_migrations() {
    let directory = tempdir().expect("temporary directory should exist");
    let path = directory.path().join("session-index.sqlite3");
    let index = SessionIndex::open(&path).expect("index should open");
    drop(index);

    let connection = Connection::open(&path).expect("database should reopen");
    let applied: Vec<(i64, String)> = connection
      .prepare("SELECT version, checksum FROM schema_migrations ORDER BY version")
      .expect("migration query should prepare")
      .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
      .expect("migration query should run")
      .collect::<rusqlite::Result<_>>()
      .expect("migration rows should decode");
    assert_eq!(
      applied,
      vec![
        (1, migration_checksum(MIGRATION_001)),
        (2, migration_checksum(MIGRATION_002)),
        (3, migration_checksum(MIGRATION_003)),
        (4, migration_checksum(MIGRATION_004)),
        (5, migration_checksum(MIGRATION_005)),
        (6, migration_checksum(MIGRATION_006)),
        (7, migration_checksum(MIGRATION_007)),
        (8, migration_checksum(MIGRATION_008)),
        (9, migration_checksum(MIGRATION_009)),
      ]
    );
  }

  #[test]
  fn latest_message_migration_preserves_completed_baselines_without_requeueing() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("v8.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection
      .pragma_update(None, "application_id", APPLICATION_ID)
      .unwrap();
    connection.execute_batch("CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY, checksum TEXT NOT NULL, applied_at_ms INTEGER NOT NULL);").unwrap();
    for migration in MIGRATIONS.iter().take(8) {
      connection.execute_batch(migration.sql).unwrap();
      connection
        .execute(
          "INSERT INTO schema_migrations VALUES(?1, ?2, 0)",
          params![migration.version, migration_checksum(migration.sql)],
        )
        .unwrap();
    }
    connection.execute("INSERT INTO sources(id, provider, source_key, cursor, scanned_at_ms, created_at_ms, updated_at_ms) VALUES(1, 'codex', 'fixture', 'body.v3.existing', 0, 0, 0)", []).unwrap();
    connection.execute("INSERT INTO sessions(source_id, session_id, source_path, title, attention_baselined, first_indexed_at_ms, indexed_at_ms) VALUES(1, 'existing', '/fixture', 'Existing title', 1, 0, 0)", []).unwrap();
    drop(connection);
    let index = SessionIndex::open(&path).unwrap();
    let row = index
      .session(&SessionKey::new("codex", "fixture", "existing"))
      .unwrap()
      .unwrap();
    assert_eq!(row.title.as_deref(), Some("Existing title"));
    assert!(row.attention_baselined);
    assert!(
      row.latest_message.is_none(),
      "migration does not invent a known-empty projection"
    );
    assert_eq!(row.source_cursor, "body.v3.existing");
    assert!(index.list_unbaselined_present_sessions().unwrap().is_empty());
  }

  #[test]
  fn latest_message_persists_through_catalog_updates_and_clears_on_body_reset() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("compact.sqlite3");
    let index = SessionIndex::open(&path).unwrap();
    index
      .replace_source(replacement("catalog-one", 0, vec![pending_session("a", false)]))
      .unwrap();
    let staged = index
      .source_state(&SourceKey::new(PROVIDER, SOURCE_KEY))
      .unwrap()
      .unwrap();
    let message =
      r#"{"message_id":"progress","delivery":"commentary","phase":"finished","preview":"Checking schema."}"#;
    let mut request = completion_request_with_presentation("a", Some("same-marker"), None, None);
    request.presentation.as_mut().unwrap().latest_message = Some(message.into());
    index
      .complete_session_baseline(&staged, &source("body-one", 1), request)
      .unwrap();
    drop(index);
    let index = SessionIndex::open(&path).unwrap();
    let key = SessionKey::new(PROVIDER, SOURCE_KEY, "a");
    let saved = index.session(&key).unwrap().unwrap();
    assert_eq!(saved.latest_message.as_deref(), Some(message));
    assert!(saved.source_generation > staged.generation);
    index
      .replace_source(replacement("catalog-two", 2, vec![pending_session("a", false)]))
      .unwrap();
    assert_eq!(
      index.session(&key).unwrap().unwrap().latest_message.as_deref(),
      Some(message)
    );
    let staged = index.source_state(&key.source_key()).unwrap().unwrap();
    let mut request = completion_request_with_presentation("a", None, None, None);
    request.presentation.as_mut().unwrap().latest_message = Some("null".into());
    index
      .complete_session_baseline(&staged, &source("body-two", 3), request)
      .unwrap();
    assert_eq!(
      index.session(&key).unwrap().unwrap().latest_message.as_deref(),
      Some("null")
    );
    let connection = Connection::open(&path).unwrap();
    let compact: String = connection
      .query_row(
        "SELECT latest_message FROM indexed_sessions WHERE session_id = 'a'",
        [],
        |row| row.get(0),
      )
      .unwrap();
    assert_eq!(compact, "null");
  }

  #[test]
  fn relocated_rows_carry_compact_messages_and_reject_unbounded_or_invalid_json() {
    let index = SessionIndex::open_in_memory().unwrap();
    let message = r#"{"message_id":null,"delivery":"final","phase":"finished","preview":"Done."}"#;
    let mut relocated = session("a", None);
    relocated.latest_message = Some(message.into());
    index
      .replace_source(replacement("relocated", 0, vec![relocated.clone()]))
      .unwrap();
    assert_eq!(
      index
        .session(&relocated.key)
        .unwrap()
        .unwrap()
        .latest_message
        .as_deref(),
      Some(message)
    );
    for invalid in ["not JSON".to_string(), format!("\"{}\"", "x".repeat(4097))] {
      relocated.latest_message = Some(invalid);
      assert!(
        index
          .replace_source(replacement("invalid", 1, vec![relocated.clone()]))
          .is_err()
      );
      assert_eq!(
        index
          .session(&relocated.key)
          .unwrap()
          .unwrap()
          .latest_message
          .as_deref(),
        Some(message)
      );
    }
  }

  #[test]
  fn migration_preserves_existing_rows_as_completed_non_notifying_baselines() {
    let directory = tempdir().expect("temporary directory should exist");
    let path = directory.path().join("session-index.sqlite3");
    let connection = Connection::open(&path).expect("database should open");
    connection
      .pragma_update(None, "application_id", APPLICATION_ID)
      .expect("database should become index-owned");
    connection
      .execute_batch(
        "CREATE TABLE schema_migrations (
           version INTEGER PRIMARY KEY,
           checksum TEXT NOT NULL,
           applied_at_ms INTEGER NOT NULL
         );",
      )
      .expect("migration ledger should be created");
    connection
      .execute_batch(MIGRATION_001)
      .expect("original schema should be created");
    connection
      .execute(
        "INSERT INTO schema_migrations(version, checksum, applied_at_ms) VALUES(?1, ?2, ?3)",
        params![1, migration_checksum(MIGRATION_001), 0],
      )
      .expect("first migration should be recorded");
    connection
      .execute(
        "INSERT INTO sources(provider, source_key, cursor, scanned_at_ms, created_at_ms, updated_at_ms)
         VALUES(?1, ?2, ?3, ?4, ?4, ?4)",
        params![PROVIDER, SOURCE_KEY, "one", 10],
      )
      .expect("source should be inserted");
    connection
      .execute(
        "INSERT INTO sessions(
           source_id, session_id, source_path, title, preview,
           attention_revision, seen_attention_revision, present,
           first_indexed_at_ms, indexed_at_ms
         ) VALUES(1, ?1, ?2, ?3, ?4, 0, 0, 1, 10, 10)",
        params!["a", "/sessions/a.jsonl", "legacy title", "legacy preview"],
      )
      .expect("old session should be inserted");
    connection
      .execute(
        "UPDATE sessions SET cwd = '/legacy', timestamp = '2020-01-01T00:00:00Z'",
        [],
      )
      .unwrap();
    drop(connection);

    let index = SessionIndex::open(&path).expect("original index should migrate");
    assert_eq!(
      index.project_order(&["/legacy".into()], 999).unwrap()["/legacy"],
      1_577_836_800_000
    );

    let indexed = index
      .session(&SessionKey::new(PROVIDER, SOURCE_KEY, "a"))
      .expect("session query should work")
      .expect("migrated session should exist");
    assert!(indexed.attention_baselined);
    assert!(!indexed.notify_on_baseline);
    assert_eq!(indexed.title.as_deref(), Some("legacy title"));
    assert_eq!(indexed.preview.as_deref(), Some("legacy preview"));
    drop(index);

    let connection = Connection::open(&path).expect("migrated database should reopen");
    let provider: String = connection
      .query_row(
        "SELECT provider FROM indexed_sessions WHERE session_id = 'a'",
        [],
        |row| row.get(0),
      )
      .expect("joined session view should expose migrated provider");
    assert_eq!(provider, PROVIDER);
  }

  #[test]
  fn concurrent_processes_upgrade_one_v3_index_without_racing_migrations() {
    let directory = tempdir().expect("temporary directory should exist");
    let path = directory.path().join("session-index.sqlite3");
    let connection = Connection::open(&path).expect("database should open");
    connection
      .pragma_update(None, "application_id", APPLICATION_ID)
      .expect("database should become index-owned");
    connection
      .execute_batch(
        "CREATE TABLE schema_migrations (
           version INTEGER PRIMARY KEY,
           checksum TEXT NOT NULL,
           applied_at_ms INTEGER NOT NULL
         );",
      )
      .expect("migration ledger should be created");
    for migration in [
      Migration {
        version: 1,
        sql: MIGRATION_001,
      },
      Migration {
        version: 2,
        sql: MIGRATION_002,
      },
      Migration {
        version: 3,
        sql: MIGRATION_003,
      },
    ] {
      connection
        .execute_batch(migration.sql)
        .expect("legacy migration should apply");
      connection
        .execute(
          "INSERT INTO schema_migrations(version, checksum, applied_at_ms) VALUES(?1, ?2, 0)",
          params![migration.version, migration_checksum(migration.sql)],
        )
        .expect("legacy migration should be recorded");
    }
    drop(connection);

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let open_index = |barrier: std::sync::Arc<std::sync::Barrier>, path: std::path::PathBuf| {
      std::thread::spawn(move || {
        barrier.wait();
        SessionIndex::open(path).map(|_| ()).map_err(|error| error.to_string())
      })
    };
    let first = open_index(std::sync::Arc::clone(&barrier), path.clone());
    let second = open_index(barrier, path.clone());
    first
      .join()
      .expect("first migration thread should not panic")
      .expect("first migration should succeed");
    second
      .join()
      .expect("second migration thread should not panic")
      .expect("second migration should succeed");

    let connection = Connection::open(&path).expect("migrated database should reopen");
    let versions = connection
      .prepare("SELECT version FROM schema_migrations ORDER BY version")
      .expect("migration query should prepare")
      .query_map([], |row| row.get::<_, i64>(0))
      .expect("migration query should run")
      .collect::<rusqlite::Result<Vec<_>>>()
      .expect("migration versions should decode");
    assert_eq!(versions, vec![1, 2, 3, 4, 5, 6, 7, 8, 9]);
  }

  #[test]
  fn indexed_sessions_view_exposes_provider_and_source_key() {
    let directory = tempdir().expect("temporary directory should exist");
    let path = directory.path().join("session-index.sqlite3");
    {
      let index = SessionIndex::open(&path).expect("index should open");
      index
        .replace_source(SourceReplacement::baseline(
          source_for("codex", "codex-source", "one", 10),
          vec![session_for("codex", "codex-source", "codex-session", Some("message"))],
        ))
        .expect("Codex source should be indexed");
      index
        .replace_source(SourceReplacement::baseline(
          source_for("pi", "pi-source", "one", 10),
          vec![session_for("pi", "pi-source", "pi-session", Some("message"))],
        ))
        .expect("Pi source should be indexed");
    }

    let connection = Connection::open(&path).expect("index database should reopen");
    let rows = connection
      .prepare(
        "SELECT provider, source_key, session_id
         FROM indexed_sessions
         WHERE present = 1
         ORDER BY provider, session_id",
      )
      .expect("view query should prepare")
      .query_map([], |row| {
        Ok((
          row.get::<_, String>(0)?,
          row.get::<_, String>(1)?,
          row.get::<_, String>(2)?,
        ))
      })
      .expect("view query should run")
      .collect::<rusqlite::Result<Vec<_>>>()
      .expect("view rows should decode");
    assert_eq!(
      rows,
      vec![
        (
          "codex".to_string(),
          "codex-source".to_string(),
          "codex-session".to_string()
        ),
        ("pi".to_string(), "pi-source".to_string(), "pi-session".to_string()),
      ]
    );
  }

  #[test]
  fn lists_present_sessions_for_one_provider() {
    let index = SessionIndex::open_in_memory().expect("index should open");
    index
      .replace_source(SourceReplacement::baseline(
        source_for("codex", "codex-source", "one", 10),
        vec![session_for("codex", "codex-source", "codex-session", Some("message"))],
      ))
      .expect("Codex source should be indexed");
    index
      .replace_source(SourceReplacement::baseline(
        source_for("pi", "pi-source", "one", 10),
        vec![session_for("pi", "pi-source", "pi-session", Some("message"))],
      ))
      .expect("Pi source should be indexed");

    let codex = index
      .list_present_sessions_for_provider("codex")
      .expect("provider session query should work");
    assert_eq!(codex.len(), 1);
    assert_eq!(codex[0].key.provider, "codex");
    assert_eq!(codex[0].key.session_id, "codex-session");
  }

  #[test]
  fn bounded_source_lookup_rejects_ambiguous_membership_and_ignores_tombstones() {
    let index = SessionIndex::open_in_memory().unwrap();
    let key = SourceKey::new(PROVIDER, SOURCE_KEY);
    assert!(index.unique_present_session_for_source(&key).unwrap().is_none());
    index
      .replace_source(SourceReplacement::baseline(source("one", 1), vec![session("a", None)]))
      .unwrap();
    assert_eq!(
      index
        .unique_present_session_for_source(&key)
        .unwrap()
        .unwrap()
        .key
        .session_id,
      "a"
    );
    index
      .replace_source(SourceReplacement::baseline(
        source("two", 2),
        vec![session("a", None), session("b", None)],
      ))
      .unwrap();
    assert!(index.unique_present_session_for_source(&key).unwrap().is_none());
    index
      .replace_source(SourceReplacement::baseline(
        source("three", 3),
        vec![session("b", None)],
      ))
      .unwrap();
    assert_eq!(
      index
        .unique_present_session_for_source(&key)
        .unwrap()
        .unwrap()
        .key
        .session_id,
      "b"
    );
    index
      .replace_source(SourceReplacement::baseline(
        source_for("pi", "other", "one", 4),
        vec![session_for("pi", "other", "unrelated", None)],
      ))
      .unwrap();
    let rows = index.list_all_sessions_for_provider(PROVIDER).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].key.session_id, "a");
    assert!(!rows[0].present);
    assert_eq!(rows[1].key.session_id, "b");
    assert!(rows[1].present);
  }

  #[test]
  fn duplicate_source_lookup_is_provider_scoped_and_ignores_tombstones() {
    let index = SessionIndex::open_in_memory().unwrap();
    let key = SessionKey::new(PROVIDER, SOURCE_KEY, "shared");
    index
      .replace_sources(&[
        SourceReplacement::baseline(source("one", 1), vec![session("shared", None)]),
        SourceReplacement::baseline(
          source_for("pi", "other", "one", 1),
          vec![session_for("pi", "other", "shared", None)],
        ),
      ])
      .unwrap();
    assert!(!index.has_other_present_session_source(&key).unwrap());
    index
      .replace_source(SourceReplacement::baseline(
        source_for(PROVIDER, "other", "one", 1),
        vec![session_for(PROVIDER, "other", "shared", None)],
      ))
      .unwrap();
    assert!(index.has_other_present_session_source(&key).unwrap());
    index
      .replace_source(SourceReplacement::baseline(
        source_for(PROVIDER, "other", "two", 2),
        vec![],
      ))
      .unwrap();
    assert!(!index.has_other_present_session_source(&key).unwrap());
  }

  #[test]
  fn counts_pending_session_baselines_by_provider_and_cursor_prefix() {
    let index = SessionIndex::open_in_memory().expect("index should open");
    let mut codex_first = session_for("codex", "codex-pending", "first", None);
    codex_first.attention_baselined = false;
    let mut codex_second = session_for("codex", "codex-pending", "second", None);
    codex_second.attention_baselined = false;
    index
      .replace_source(SourceReplacement::new(
        source_for("codex", "codex-pending", "pending.v3.current", 10),
        vec![
          codex_first,
          codex_second,
          session_for("codex", "codex-pending", "done", None),
        ],
      ))
      .expect("pending Codex source should be indexed");

    let mut pi_pending = session_for("pi", "pi-pending", "waiting", None);
    pi_pending.attention_baselined = false;
    index
      .replace_source(SourceReplacement::new(
        source_for("pi", "pi-pending", "pending.v2.legacy", 10),
        vec![pi_pending],
      ))
      .expect("pending Pi source should be indexed");

    let mut completed_pending = session_for("zcode", "zcode-complete", "stale", None);
    completed_pending.attention_baselined = false;
    index
      .replace_source(SourceReplacement::new(
        source_for("zcode", "zcode-complete", "completed.v3.current", 10),
        vec![completed_pending],
      ))
      .expect("completed ZCode source should be indexed");

    let unbaselined = index
      .list_unbaselined_present_sessions()
      .expect("unbaselined session query should work");
    assert_eq!(
      unbaselined
        .iter()
        .map(|session| session.key.session_id.as_str())
        .collect::<Vec<_>>(),
      vec!["first", "second", "stale", "waiting"]
    );

    let counts = index
      .pending_session_baseline_counts(&["pending.v3.", "pending.v2."])
      .expect("pending baseline counts should query");
    assert_eq!(
      counts,
      vec![
        PendingSessionBaselineCount {
          provider: "codex".to_owned(),
          pending_sessions: 2,
        },
        PendingSessionBaselineCount {
          provider: "pi".to_owned(),
          pending_sessions: 1,
        },
      ]
    );
    assert!(
      index
        .pending_session_baseline_counts(&[])
        .expect("empty prefix count should query")
        .is_empty()
    );

    let source_counts = index
      .staged_session_baseline_source_counts(&["pending.v3.", "pending.v2."])
      .expect("per-source pending baseline counts should query");
    assert_eq!(
      source_counts,
      vec![
        StagedSessionBaselineSourceCount {
          provider: "codex".to_owned(),
          source_cursor: "pending.v3.current".to_owned(),
          present_sessions: 3,
          pending_sessions: 2,
        },
        StagedSessionBaselineSourceCount {
          provider: "pi".to_owned(),
          source_cursor: "pending.v2.legacy".to_owned(),
          present_sessions: 1,
          pending_sessions: 1,
        },
      ]
    );
    assert!(
      index
        .staged_session_baseline_source_counts(&[])
        .expect("empty source count should query")
        .is_empty()
    );
  }

  #[test]
  fn persists_pending_attention_baseline_state() {
    let directory = tempdir().expect("temporary directory should exist");
    let path = directory.path().join("session-index.sqlite3");
    let mut pending = session("a", None);
    assert!(pending.attention_baselined);
    assert!(!pending.notify_on_baseline);
    pending.attention_baselined = false;
    pending.notify_on_baseline = true;

    {
      let index = SessionIndex::open(&path).expect("index should open");
      index
        .replace_source(replacement("one", 10, vec![pending]))
        .expect("pending baseline should be stored");
    }

    let index = SessionIndex::open(&path).expect("index should reopen");
    let key = SessionKey::new(PROVIDER, SOURCE_KEY, "a");
    let stored_pending = index
      .session(&key)
      .expect("session query should work")
      .expect("pending session should exist");
    assert!(!stored_pending.attention_baselined);
    assert!(stored_pending.notify_on_baseline);

    let mut completed = session("a", Some("message-a"));
    completed.attention_baselined = true;
    completed.notify_on_baseline = false;
    index
      .replace_source(replacement("two", 20, vec![completed]))
      .expect("completed baseline should be stored");
    let stored_completed = index
      .session(&key)
      .expect("session query should work")
      .expect("completed session should exist");
    assert!(stored_completed.attention_baselined);
    assert!(!stored_completed.notify_on_baseline);
  }

  #[test]
  fn rejects_notify_on_an_already_established_baseline() {
    let index = SessionIndex::open_in_memory().expect("index should open");
    let mut invalid = session("a", None);
    invalid.notify_on_baseline = true;

    assert!(matches!(
      index.replace_source(replacement("one", 10, vec![invalid])),
      Err(SessionIndexError::InvalidReplacement(message))
        if message.contains("already established attention baseline")
    ));
  }

  #[test]
  fn completing_one_pending_baseline_does_not_touch_siblings_or_increment_twice() {
    let index = SessionIndex::open_in_memory().expect("index should open");
    let staged_source = source("catalog-one", 10);
    let completed_source = source("body-one", 11);
    let mut cataloged = pending_session("a", true);
    cataloged.title = Some("new catalog title".to_owned());
    cataloged.source_path = "/catalog/a.jsonl".to_owned();
    index
      .replace_source(SourceReplacement::new(
        staged_source.clone(),
        vec![cataloged, pending_session("b", false)],
      ))
      .expect("catalog replacement should succeed");
    let staged_source = index
      .source_state(&staged_source.key)
      .expect("source query should work")
      .expect("catalog source should exist");

    let first = index
      .complete_session_baseline(
        &staged_source,
        &completed_source,
        completion_request("a", Some("message-a"), true),
      )
      .expect("first completion should run");
    assert!(matches!(
      first,
      SessionBaselineCompletion::Applied {
        attention_changed: true,
        ..
      }
    ));
    assert!(first.was_applied());
    assert!(first.attention_changed());

    let second = index
      .complete_session_baseline(
        &staged_source,
        &source("body-two", 12),
        completion_request("a", Some("message-b"), true),
      )
      .expect("second completion should run");
    assert_eq!(second, SessionBaselineCompletion::Stale);
    assert!(!second.was_applied());
    assert!(!second.attention_changed());

    let completed = index
      .session(&SessionKey::new(PROVIDER, SOURCE_KEY, "a"))
      .expect("session query should work")
      .expect("completed session should exist");
    assert!(completed.attention_baselined);
    assert!(!completed.notify_on_baseline);
    assert_eq!(completed.title.as_deref(), Some("new catalog title"));
    assert_eq!(completed.source_path, "/catalog/a.jsonl");
    assert_eq!(completed.attention_marker.as_deref(), Some("message-a"));
    assert_eq!(completed.attention_revision, 1);
    assert!(completed.has_unread());

    let sibling = index
      .session(&SessionKey::new(PROVIDER, SOURCE_KEY, "b"))
      .expect("session query should work")
      .expect("sibling session should remain indexed");
    assert!(sibling.present);
    assert!(!sibling.attention_baselined);
    assert!(!sibling.has_unread());
    let completed_state = index
      .source_state(&staged_source.key)
      .expect("source query should work")
      .expect("completed source should exist");
    assert_eq!(completed_state.cursor, completed_source.cursor);
    assert_eq!(completed_state.scanned_at_ms, completed_source.scanned_at_ms);
    assert_eq!(completed_state.generation, 2);
    assert_eq!(first.committed_source(), Some(&completed_state));
  }

  #[test]
  fn completing_a_pending_baseline_can_backfill_body_presentation() {
    let index = SessionIndex::open_in_memory().expect("index should open");
    let staged_source = source("catalog-one", 10);
    let completed_source = source("body-one", 11);
    let mut cataloged = pending_session("a", false);
    cataloged.title = None;
    cataloged.preview = None;
    index
      .replace_source(SourceReplacement::new(staged_source.clone(), vec![cataloged]))
      .expect("catalog replacement should succeed");
    let staged_source = index
      .source_state(&staged_source.key)
      .expect("source query should work")
      .expect("catalog source should exist");

    assert!(matches!(
      index
        .complete_session_baseline(
          &staged_source,
          &completed_source,
          completion_request_with_presentation("a", Some("message-a"), Some("Body title"), Some("Body preview"),),
        )
        .expect("completion should succeed"),
      SessionBaselineCompletion::Applied {
        attention_changed: false,
        ..
      }
    ));

    let completed = index
      .session(&SessionKey::new(PROVIDER, SOURCE_KEY, "a"))
      .expect("session query should work")
      .expect("completed session should exist");
    assert_eq!(completed.title.as_deref(), Some("Body title"));
    assert_eq!(completed.preview.as_deref(), Some("Body preview"));

    assert_eq!(
      index
        .complete_session_baseline(
          &completed_source,
          &source("body-two", 12),
          completion_request_with_presentation("a", Some("message-b"), Some("Stale title"), None),
        )
        .expect("stale completion should not fail"),
      SessionBaselineCompletion::Stale
    );
    let unchanged = index
      .session(&SessionKey::new(PROVIDER, SOURCE_KEY, "a"))
      .expect("session query should work")
      .expect("completed session should exist");
    assert_eq!(unchanged.title.as_deref(), Some("Body title"));
    assert_eq!(unchanged.preview.as_deref(), Some("Body preview"));
  }

  #[test]
  fn catalog_presentation_wins_over_a_delayed_body_completion_and_can_be_removed() {
    let index = SessionIndex::open_in_memory().expect("index should open");
    let staged_source = source("catalog-one", 10);
    let completed_source = source("body-one", 11);
    let mut pending = pending_session("a", false);
    pending.title = None;
    pending.preview = None;
    index
      .replace_source(SourceReplacement::new(staged_source.clone(), vec![pending]))
      .expect("initial catalog should commit");
    let initial_staged_source = index
      .source_state(&staged_source.key)
      .expect("source query should work")
      .expect("initial source should exist");

    // A later header scan can update presentation without changing the body
    // source cursor. The delayed body completion must not overwrite it.
    let mut newer_catalog = pending_session("a", false);
    newer_catalog.title = Some("New catalog title".to_owned());
    newer_catalog.preview = Some("New catalog preview".to_owned());
    index
      .replace_source(SourceReplacement::new(staged_source.clone(), vec![newer_catalog]))
      .expect("same-cursor catalog metadata should commit");
    assert_eq!(
      index
        .complete_session_baseline(
          &initial_staged_source,
          &completed_source,
          completion_request_with_presentation("a", Some("message-a"), Some("Stale body title"), None),
        )
        .expect("delayed body completion should stay safe"),
      SessionBaselineCompletion::Stale,
      "a same-cursor catalog mutation must invalidate a body job planned before it"
    );
    let staged_source = index
      .source_state(&staged_source.key)
      .expect("source query should work")
      .expect("newer catalog source should exist");
    index
      .complete_session_baseline(
        &staged_source,
        &completed_source,
        completion_request_with_presentation("a", Some("message-a"), Some("Body title"), Some("Body preview")),
      )
      .expect("delayed body completion should apply");

    let catalog_wins = index
      .session(&SessionKey::new(PROVIDER, SOURCE_KEY, "a"))
      .expect("session query should work")
      .expect("completed session should exist");
    assert_eq!(catalog_wins.title.as_deref(), Some("New catalog title"));
    assert_eq!(catalog_wins.preview.as_deref(), Some("New catalog preview"));

    // Removing the catalog values exposes the completed body fallback without
    // requiring a second provider body read.
    let mut removed_catalog = session("a", Some("message-a"));
    removed_catalog.title = None;
    removed_catalog.preview = None;
    index
      .replace_source(SourceReplacement::new(completed_source, vec![removed_catalog]))
      .expect("same-cursor catalog removal should commit");
    let body_fallback = index
      .session(&SessionKey::new(PROVIDER, SOURCE_KEY, "a"))
      .expect("session query should work")
      .expect("completed session should exist");
    assert_eq!(body_fallback.title.as_deref(), Some("Body title"));
    assert_eq!(body_fallback.preview.as_deref(), Some("Body preview"));
  }

  #[test]
  fn completing_a_pending_baseline_rejects_another_source_and_stale_cursor() {
    let index = SessionIndex::open_in_memory().expect("index should open");
    let staged_source = source("catalog-one", 10);
    let completed_source = source("body-one", 11);
    index
      .replace_source(replacement("catalog-one", 10, vec![pending_session("a", true)]))
      .expect("catalog replacement should succeed");

    assert!(matches!(
      index.complete_session_baseline(
        &staged_source,
        &staged_source,
        completion_request("a", Some("message-a"), true),
      ),
      Err(SessionIndexError::InvalidReplacement(message)) if message.contains("must advance")
    ));
    assert!(matches!(
      index.complete_session_baseline(
        &staged_source,
        &source_for(PROVIDER, "other-source", "body-one", 11),
        completion_request("a", Some("message-a"), true),
      ),
      Err(SessionIndexError::InvalidReplacement(message)) if message.contains("does not match staged source")
    ));

    let wrong_source = SessionBaselineCompletionRequest::new(
      SessionKey::new(PROVIDER, "other-source", "a"),
      Some("message-a".to_owned()),
    );
    assert!(matches!(
      index.complete_session_baseline(&staged_source, &completed_source, wrong_source),
      Err(SessionIndexError::InvalidReplacement(message))
        if message.contains("does not belong to staged source")
    ));

    let stale_source = source("catalog-two", 20);
    assert_eq!(
      index
        .complete_session_baseline(
          &stale_source,
          &source("body-two", 21),
          completion_request("a", Some("message-a"), true),
        )
        .expect("stale completion should not fail"),
      SessionBaselineCompletion::Stale
    );
    let pending = index
      .session(&SessionKey::new(PROVIDER, SOURCE_KEY, "a"))
      .expect("session query should work")
      .expect("pending session should remain indexed");
    assert!(!pending.attention_baselined);
    assert_eq!(pending.attention_revision, 0);
  }

  #[test]
  fn baseline_completion_advancing_cursor_rejects_a_prepared_catalog_replacement() {
    let directory = tempdir().expect("temporary directory should exist");
    let path = directory.path().join("session-index.sqlite3");
    let first = SessionIndex::open(&path).expect("first index should open");
    let second = SessionIndex::open(&path).expect("second index should open");
    let staged_source = source("catalog-one", 10);
    let completed_source = source("body-one", 11);
    first
      .replace_source(SourceReplacement::new(
        staged_source.clone(),
        vec![pending_session("a", true)],
      ))
      .expect("catalog replacement should succeed");
    let staged_source = first
      .source_state(&staged_source.key)
      .expect("source query should work")
      .expect("catalog source should exist");

    let prepared_catalog_replacement =
      SourceReplacement::new(source("catalog-two", 20), vec![pending_session("a", true)])
        .with_source_cursor_precondition(SourceCursorPrecondition::existing(&staged_source));

    assert!(matches!(
      first
        .complete_session_baseline(
          &staged_source,
          &completed_source,
          completion_request("a", Some("message-a"), true),
        )
        .expect("completion should succeed"),
      SessionBaselineCompletion::Applied {
        attention_changed: true,
        ..
      }
    ));

    let error = second
      .replace_source(prepared_catalog_replacement)
      .expect_err("catalog replacement prepared at the old cursor must conflict");
    assert!(matches!(
      error,
      SessionIndexError::SourceCursorConflict {
        source: conflict_source,
        expected: SourceCursorPrecondition::Exact(Some(expected)),
        actual: Some(actual),
      } if conflict_source == staged_source.key
        && expected.cursor == "catalog-one"
        && expected.generation == 1
        && actual.cursor == "body-one"
        && actual.generation == 2
    ));
  }

  #[test]
  fn initial_source_baseline_has_no_unread_dot() {
    let index = SessionIndex::open_in_memory().expect("index should open");
    let result = index
      .replace_source(baseline_replacement("one", 10, vec![session("a", Some("message-a"))]))
      .expect("baseline replacement should succeed");

    assert!(result.baseline_established);
    assert_eq!(result.attention_changed, 0);
    let indexed = index
      .session(&SessionKey::new(PROVIDER, SOURCE_KEY, "a"))
      .expect("session query should work")
      .expect("session should be indexed");
    assert_eq!(indexed.attention_revision, 0);
    assert_eq!(indexed.seen_attention_revision, 0);
    assert!(!indexed.has_unread());
    assert!(
      index
        .has_sources_for_provider(PROVIDER)
        .expect("provider query should work")
    );
  }

  #[test]
  fn caller_reported_new_attention_makes_session_unread_and_mark_seen_through_clears_it() {
    let index = SessionIndex::open_in_memory().expect("index should open");
    index
      .replace_source(baseline_replacement("one", 10, vec![session("a", Some("message-a"))]))
      .expect("baseline replacement should succeed");
    let mut changed = session("a", Some("message-b"));
    changed.has_new_attention = true;
    let result = index
      .replace_source(replacement("two", 20, vec![changed]))
      .expect("changed marker replacement should succeed");

    assert_eq!(result.attention_changed, 1);
    let key = SessionKey::new(PROVIDER, SOURCE_KEY, "a");
    let unread = index
      .session(&key)
      .expect("session query should work")
      .expect("session should exist");
    assert_eq!(unread.attention_revision, 1);
    assert_eq!(unread.seen_attention_revision, 0);
    assert!(unread.has_unread());

    assert!(
      index
        .mark_seen_through(&key, unread.attention_revision, 30)
        .expect("mark seen should work")
    );
    let seen = index
      .session(&key)
      .expect("session query should work")
      .expect("session should exist");
    assert!(!seen.has_unread());
    assert_eq!(seen.seen_at_ms, Some(30));
  }

  #[test]
  fn track_changes_allows_a_later_discovered_source_to_be_unread() {
    let index = SessionIndex::open_in_memory().expect("index should open");
    let mut newly_discovered = session("a", Some("message-a"));
    newly_discovered.has_new_attention = true;

    let result = index
      .replace_source(replacement("one", 10, vec![newly_discovered]))
      .expect("tracked replacement should succeed");

    assert!(!result.baseline_established);
    assert_eq!(result.attention_changed, 1);
    let indexed = index
      .session(&SessionKey::new(PROVIDER, SOURCE_KEY, "a"))
      .expect("session query should work")
      .expect("session should exist");
    assert_eq!(indexed.attention_revision, 1);
    assert_eq!(indexed.seen_attention_revision, 0);
    assert!(indexed.has_unread());
  }

  #[test]
  fn stale_acknowledgement_cannot_clear_newer_attention() {
    let index = SessionIndex::open_in_memory().expect("index should open");
    let key = SessionKey::new(PROVIDER, SOURCE_KEY, "a");
    index
      .replace_source(baseline_replacement("one", 10, vec![session("a", Some("message-a"))]))
      .expect("baseline replacement should succeed");

    let mut first_new_message = session("a", Some("message-b"));
    first_new_message.has_new_attention = true;
    index
      .replace_source(replacement("two", 20, vec![first_new_message]))
      .expect("first update should succeed");
    let captured_revision = index
      .session(&key)
      .expect("session query should work")
      .expect("session should exist")
      .attention_revision;

    let mut second_new_message = session("a", Some("message-c"));
    second_new_message.has_new_attention = true;
    index
      .replace_source(replacement("three", 30, vec![second_new_message]))
      .expect("second update should succeed");

    assert!(
      index
        .mark_seen_through(&key, captured_revision, 40)
        .expect("stale acknowledgement should run")
    );
    let after_stale_ack = index
      .session(&key)
      .expect("session query should work")
      .expect("session should exist");
    assert_eq!(after_stale_ack.attention_revision, 2);
    assert_eq!(after_stale_ack.seen_attention_revision, 1);
    assert!(after_stale_ack.has_unread());
    assert!(
      !index
        .mark_seen_through(&key, captured_revision, 41)
        .expect("repeat acknowledgement should not change state")
    );
    assert!(
      index
        .mark_seen_through(&key, after_stale_ack.attention_revision, 42)
        .expect("current acknowledgement should advance state")
    );
    assert!(
      !index
        .session(&key)
        .expect("session query should work")
        .expect("session should exist")
        .has_unread()
    );
  }

  #[test]
  fn metadata_or_marker_only_change_does_not_make_session_unread() {
    let index = SessionIndex::open_in_memory().expect("index should open");
    index
      .replace_source(baseline_replacement("one", 10, vec![session("a", Some("message-a"))]))
      .expect("baseline replacement should succeed");

    let mut changed_metadata = session("a", Some("rewritten-marker"));
    changed_metadata.title = Some("A better title".to_owned());
    changed_metadata.preview = Some("A different bounded preview".to_owned());
    changed_metadata.cwd = Some("/workspace".to_owned());
    changed_metadata.updated_at = Some("another-provider-update".to_owned());
    changed_metadata.updated_at_ms = Some(2_000);
    index
      .replace_source(replacement("two", 20, vec![changed_metadata]))
      .expect("metadata replacement should succeed");

    let indexed = index
      .session(&SessionKey::new(PROVIDER, SOURCE_KEY, "a"))
      .expect("session query should work")
      .expect("session should exist");
    assert_eq!(indexed.title.as_deref(), Some("A better title"));
    assert_eq!(indexed.preview.as_deref(), Some("A different bounded preview"));
    assert_eq!(indexed.attention_marker.as_deref(), Some("rewritten-marker"));
    assert_eq!(indexed.attention_revision, 0);
    assert_eq!(indexed.seen_attention_revision, 0);
    assert!(!indexed.has_unread());
  }

  #[test]
  fn only_a_successful_complete_replacement_tombstones_missing_sessions() {
    let index = SessionIndex::open_in_memory().expect("index should open");
    index
      .replace_source(baseline_replacement(
        "one",
        10,
        vec![session("a", Some("message-a")), session("b", Some("message-b"))],
      ))
      .expect("baseline replacement should succeed");

    let invalid = SourceReplacement::new(
      source("failed", 20),
      vec![SessionMetadata::new(
        SessionKey::new(PROVIDER, "different-source", "a"),
        "/sessions/a.jsonl",
      )],
    );
    assert!(matches!(
      index.replace_source(invalid),
      Err(SessionIndexError::InvalidReplacement(_))
    ));
    assert_eq!(index.list_present_sessions().expect("list should work").len(), 2);

    let result = index
      .replace_source(replacement("two", 30, vec![session("a", Some("message-a"))]))
      .expect("complete replacement should succeed");
    assert_eq!(result.tombstoned, 1);
    assert_eq!(index.list_present_sessions().expect("list should work").len(), 1);
    let missing = index
      .session(&SessionKey::new(PROVIDER, SOURCE_KEY, "b"))
      .expect("session query should work")
      .expect("tombstoned session should remain indexed");
    assert!(!missing.present);
  }

  #[test]
  fn index_persists_source_cursor_metadata_and_attention_after_reopen() {
    let directory = tempdir().expect("temporary directory should exist");
    let path = directory.path().join("session-index.sqlite3");
    {
      let index = SessionIndex::open(&path).expect("index should open");
      index
        .replace_source(baseline_replacement("one", 10, vec![session("a", Some("message-a"))]))
        .expect("baseline replacement should succeed");
      let mut changed = session("a", Some("message-b"));
      changed.has_new_attention = true;
      index
        .replace_source(replacement("two", 20, vec![changed]))
        .expect("changed marker replacement should succeed");
    }

    let index = SessionIndex::open(&path).expect("index should reopen");
    let source = index
      .source_state(&SourceKey::new(PROVIDER, SOURCE_KEY))
      .expect("source query should work")
      .expect("source should persist");
    assert_eq!(source.cursor, "two");
    assert_eq!(source.scanned_at_ms, 20);
    assert_eq!(source.generation, 2);
    let indexed = index
      .session(&SessionKey::new(PROVIDER, SOURCE_KEY, "a"))
      .expect("session query should work")
      .expect("session should persist");
    assert_eq!(indexed.source_path, "/sessions/a.jsonl");
    assert_eq!(indexed.timestamp.as_deref(), Some("2026-09-01T00:00:00Z"));
    assert_eq!(indexed.updated_at.as_deref(), Some("provider-update"));
    assert!(indexed.has_unread());
  }

  #[test]
  fn source_cursor_preconditions_reject_a_stale_writer_without_tombstoning_newer_rows() {
    let directory = tempdir().expect("temporary directory should exist");
    let path = directory.path().join("session-index.sqlite3");
    let first_writer = SessionIndex::open(&path).expect("first index should open");
    let second_writer = SessionIndex::open(&path).expect("second index should open");
    let source_key = SourceKey::new(PROVIDER, SOURCE_KEY);

    let initial_snapshot = first_writer
      .source_state(&source_key)
      .expect("source query should work");
    first_writer
      .replace_source(
        baseline_replacement("one", 10, vec![session("a", Some("message-a"))]).with_source_cursor_precondition(
          SourceCursorPrecondition::Exact(initial_snapshot.as_ref().map(SourceCheckpoint::from)),
        ),
      )
      .expect("an absent-source precondition should match an absent source");

    let stale_snapshot = first_writer
      .source_state(&source_key)
      .expect("source query should work")
      .expect("source should be indexed");
    let mut newer_session = session("a", Some("message-b"));
    newer_session.title = Some("newer writer title".to_owned());
    second_writer
      .replace_source(
        replacement("two", 20, vec![newer_session])
          .with_source_cursor_precondition(SourceCursorPrecondition::existing(&stale_snapshot)),
      )
      .expect("the current cursor should satisfy the second writer");

    let stale_replacement = replacement("three", 30, vec![session("a", Some("message-c"))])
      .with_source_cursor_precondition(SourceCursorPrecondition::existing(&stale_snapshot));
    let error = first_writer
      .replace_source(stale_replacement)
      .expect_err("a stale source cursor must reject the replacement");
    assert!(matches!(
      error,
      SessionIndexError::SourceCursorConflict {
        source,
        expected: SourceCursorPrecondition::Exact(Some(expected)),
        actual: Some(actual),
      } if source == source_key
        && expected.cursor == "one"
        && expected.generation == 1
        && actual.cursor == "two"
        && actual.generation == 2
    ));

    let current_source = first_writer
      .source_state(&source_key)
      .expect("source query should work")
      .expect("newer source should remain indexed");
    assert_eq!(current_source.cursor, "two");
    assert_eq!(current_source.scanned_at_ms, 20);
    assert_eq!(current_source.generation, 2);
    let current = first_writer
      .session(&SessionKey::new(PROVIDER, SOURCE_KEY, "a"))
      .expect("session query should work")
      .expect("newer row should remain indexed");
    assert_eq!(current.title.as_deref(), Some("newer writer title"));
  }

  #[test]
  fn source_generation_rejects_a_same_cursor_stale_metadata_writer() {
    let directory = tempdir().expect("temporary directory should exist");
    let path = directory.path().join("session-index.sqlite3");
    let first_writer = SessionIndex::open(&path).expect("first index should open");
    let second_writer = SessionIndex::open(&path).expect("second index should open");
    let source_key = SourceKey::new(PROVIDER, SOURCE_KEY);

    first_writer
      .replace_source(baseline_replacement(
        "unchanged-provider-cursor",
        10,
        vec![session("a", Some("message-a"))],
      ))
      .expect("initial catalog should commit");
    let stale_snapshot = first_writer
      .source_state(&source_key)
      .expect("source query should work")
      .expect("source should be indexed");

    let mut winning_session = session("a", Some("message-a"));
    winning_session.title = Some("newer catalog title".to_owned());
    winning_session.parent_session_id = Some("newer-parent".to_owned());
    second_writer
      .replace_source(
        replacement("unchanged-provider-cursor", 20, vec![winning_session])
          .with_source_cursor_precondition(SourceCursorPrecondition::existing(&stale_snapshot)),
      )
      .expect("same-cursor metadata update should commit");

    let mut stale_session = session("a", Some("message-a"));
    stale_session.title = Some("stale catalog title".to_owned());
    stale_session.parent_session_id = Some("stale-parent".to_owned());
    let error = first_writer
      .replace_source(
        replacement("unchanged-provider-cursor", 30, vec![stale_session])
          .with_source_cursor_precondition(SourceCursorPrecondition::existing(&stale_snapshot)),
      )
      .expect_err("same provider cursor must not permit a stale metadata overwrite");
    assert!(matches!(
      error,
      SessionIndexError::SourceCursorConflict {
        source,
        expected: SourceCursorPrecondition::Exact(Some(expected)),
        actual: Some(actual),
      } if source == source_key
        && expected.cursor == "unchanged-provider-cursor"
        && expected.generation == 1
        && actual.cursor == "unchanged-provider-cursor"
        && actual.generation == 2
    ));

    let current = first_writer
      .session(&SessionKey::new(PROVIDER, SOURCE_KEY, "a"))
      .expect("session query should work")
      .expect("winning session should remain indexed");
    assert_eq!(current.title.as_deref(), Some("newer catalog title"));
    assert_eq!(current.parent_session_id.as_deref(), Some("newer-parent"));
  }

  #[test]
  fn data_version_observes_a_commit_from_another_connection() {
    let directory = tempdir().expect("temporary directory should exist");
    let path = directory.path().join("session-index.sqlite3");
    let observer = SessionIndex::open(&path).expect("observer index should open");
    let writer = SessionIndex::open(&path).expect("writer index should open");
    let before = observer.data_version().expect("data version should query");

    writer
      .replace_source(baseline_replacement("one", 10, vec![session("a", Some("message-a"))]))
      .expect("writer replacement should commit");

    let after = observer.data_version().expect("data version should query");
    assert_ne!(after, before);
  }

  #[test]
  fn batch_replacement_keeps_every_source_unchanged_when_a_later_precondition_is_stale() {
    let index = SessionIndex::open_in_memory().expect("index should open");
    let first_source_key = "source-a";
    let second_source_key = "source-b";
    let first_key = SourceKey::new(PROVIDER, first_source_key);
    let second_key = SourceKey::new(PROVIDER, second_source_key);

    index
      .replace_sources(&[
        SourceReplacement::baseline(
          source_for(PROVIDER, first_source_key, "one", 10),
          vec![
            session_for(PROVIDER, first_source_key, "keep", Some("message-keep")),
            session_for(PROVIDER, first_source_key, "removed", Some("message-removed")),
          ],
        ),
        SourceReplacement::baseline(
          source_for(PROVIDER, second_source_key, "one", 10),
          vec![session_for(PROVIDER, second_source_key, "other", Some("message-other"))],
        ),
      ])
      .expect("initial batch should succeed");
    let first_snapshot = index
      .source_state(&first_key)
      .expect("first source query should work")
      .expect("first source should be indexed");
    let second_snapshot = index
      .source_state(&second_key)
      .expect("second source query should work")
      .expect("second source should be indexed");

    let mut current_second_session = session_for(PROVIDER, second_source_key, "other", Some("message-current"));
    current_second_session.title = Some("current second title".to_owned());
    index
      .replace_source(
        SourceReplacement::new(
          source_for(PROVIDER, second_source_key, "two", 20),
          vec![current_second_session],
        )
        .with_source_cursor_precondition(SourceCursorPrecondition::existing(&second_snapshot)),
      )
      .expect("second source should advance before the stale batch");

    let mut first_pending_session = session_for(PROVIDER, first_source_key, "keep", Some("message-new"));
    first_pending_session.title = Some("must not commit".to_owned());
    first_pending_session.has_new_attention = true;
    let stale_batch = [
      SourceReplacement::new(
        source_for(PROVIDER, first_source_key, "two", 30),
        vec![first_pending_session],
      )
      .with_source_cursor_precondition(SourceCursorPrecondition::existing(&first_snapshot)),
      SourceReplacement::new(
        source_for(PROVIDER, second_source_key, "three", 30),
        vec![session_for(PROVIDER, second_source_key, "other", Some("message-stale"))],
      )
      .with_source_cursor_precondition(SourceCursorPrecondition::existing(&second_snapshot)),
    ];

    let error = index
      .replace_sources(&stale_batch)
      .expect_err("a stale later source must reject the whole batch");
    assert!(matches!(
      error,
      SessionIndexError::SourceCursorConflict {
        source,
        expected: SourceCursorPrecondition::Exact(Some(expected)),
        actual: Some(actual),
      } if source == second_key
        && expected.cursor == "one"
        && expected.generation == 1
        && actual.cursor == "two"
        && actual.generation == 2
    ));

    let retained_source = index
      .source_state(&first_key)
      .expect("first source query should work")
      .expect("first source should remain indexed");
    assert_eq!(retained_source.cursor, "one");
    assert_eq!(retained_source.scanned_at_ms, 10);
    assert_eq!(retained_source.generation, 1);
    let retained = index
      .session(&SessionKey::new(PROVIDER, first_source_key, "keep"))
      .expect("first session query should work")
      .expect("first session should remain indexed");
    assert_eq!(retained.title.as_deref(), Some("Title keep"));
    assert_eq!(retained.attention_revision, 0);
    assert!(
      index
        .session(&SessionKey::new(PROVIDER, first_source_key, "removed"))
        .expect("tombstoned session query should work")
        .expect("omitted session should remain indexed")
        .present
    );
    let current_second_source = index
      .source_state(&second_key)
      .expect("second source query should work")
      .expect("second source should remain indexed");
    assert_eq!(current_second_source.cursor, "two");
    assert_eq!(current_second_source.scanned_at_ms, 20);
    assert_eq!(current_second_source.generation, 2);
  }

  #[test]
  fn list_order_is_total_when_sessions_share_update_time_and_id() {
    let index = SessionIndex::open_in_memory().expect("index should open");
    for (provider, source_key) in [("codex", "source-b"), ("codex", "source-a"), ("pi", "source-a")] {
      index
        .replace_source(SourceReplacement::baseline(
          source_for(provider, source_key, "one", 10),
          vec![session_for(provider, source_key, "shared-session", Some("message"))],
        ))
        .expect("replacement should succeed");
    }

    let keys = index
      .list_present_sessions()
      .expect("list should work")
      .into_iter()
      .map(|session| (session.key.provider, session.key.source_key, session.key.session_id))
      .collect::<Vec<_>>();
    assert_eq!(
      keys,
      vec![
        ("codex".to_owned(), "source-a".to_owned(), "shared-session".to_owned()),
        ("codex".to_owned(), "source-b".to_owned(), "shared-session".to_owned()),
        ("pi".to_owned(), "source-a".to_owned(), "shared-session".to_owned()),
      ]
    );
  }

  #[test]
  fn migration_checksum_drift_is_rejected() {
    let directory = tempdir().expect("temporary directory should exist");
    let path = directory.path().join("session-index.sqlite3");
    let index = SessionIndex::open(&path).expect("index should open");
    drop(index);

    let connection = Connection::open(&path).expect("database should reopen");
    connection
      .execute("UPDATE schema_migrations SET checksum = 'bad' WHERE version = 1", [])
      .expect("migration checksum should update for test");
    drop(connection);

    assert!(matches!(
      SessionIndex::open(&path),
      Err(SessionIndexError::MigrationChecksumMismatch { version: 1, .. })
    ));
  }

  #[test]
  fn refuses_to_migrate_an_unowned_existing_database() {
    let directory = tempdir().expect("temporary directory should exist");
    let path = directory.path().join("unowned.sqlite3");
    let connection = Connection::open(&path).expect("database should open");
    connection
      .execute("CREATE TABLE unrelated_cache(id INTEGER PRIMARY KEY)", [])
      .expect("unrelated schema should be created");
    let journal_before: String = connection
      .pragma_query_value(None, "journal_mode", |row| row.get(0))
      .expect("journal mode should be readable");
    drop(connection);

    assert!(matches!(
      SessionIndex::open(&path),
      Err(SessionIndexError::UnexpectedDatabase(0))
    ));

    let connection = Connection::open(&path).expect("database should reopen");
    let journal_after: String = connection
      .pragma_query_value(None, "journal_mode", |row| row.get(0))
      .expect("journal mode should remain readable");
    assert_eq!(journal_after, journal_before);
  }
}
