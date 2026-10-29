mod bounded_lines;
mod compaction;
pub mod event;
mod history;
pub mod normalize;
mod records;
mod rollout_path;
pub mod session_source;
mod storage;

pub use history::{
  CodexHistoryLimits, CodexHistoryProjectionReader, CodexHistoryProjectionUpdate, CodexHistoryReadStats,
  CodexHistoryReader, CodexHistorySegment, CodexHistoryUpdate, history_header,
};
pub use session_source::CodexSessionSource;
pub use storage::CodexScratchBudget;

/// Resolves a retained native JSONL path to its compressed sibling when needed.
pub fn resolve_rollout_path(path: &std::path::Path) -> std::path::PathBuf {
  storage::resolve_path(path)
}
