//! Qoder CLI's project-scoped, chained JSONL transcripts.
mod normalize;
mod session_source;
mod storage;

pub use session_source::{QoderSessionSource, read_session_header_exact};
pub use storage::{default_session_root, project_identifier};
