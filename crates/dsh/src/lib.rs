//! Read-only discovery and historical normalization of DeepSeek Harness logs.

mod compaction;
mod identity;
mod metadata;
mod normalize;
mod session_source;
mod sqlite;
mod storage;

pub use identity::read_session_header_exact;
pub use session_source::DshSessionSource;
pub use sqlite::storage_dependency_paths;
