//! Tidy's scan engine: platform-specific bulk directory readers, a parallel
//! work-stealing walker, and a compact directory-only index.

pub mod cache;
pub mod entry;
pub mod fmt;
pub mod index;
pub mod list;
pub mod patch;
pub mod path;
pub mod platform;
pub mod snapshot;
pub mod visit;
pub mod walk;

pub use entry::{EntryInfo, Kind, ReadOptions};
pub use index::{DirNode, FileHit, Index, ScanStats, SizeKind, TreeRow};
pub use list::{Listed, list_dir};
pub use patch::{Changes, Watch};
pub use walk::{ScanError, ScanOptions, ScanProgress, default_threads, scan};
