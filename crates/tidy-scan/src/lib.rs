//! Tidy's scan engine: platform-specific bulk directory readers, a parallel
//! work-stealing walker, and a compact directory-only index.

pub mod entry;
pub mod fmt;
pub mod index;
pub mod path;
pub mod platform;
pub mod snapshot;
pub mod visit;
pub mod walk;

pub use entry::{EntryInfo, Kind, ReadOptions};
pub use index::{DirNode, FileHit, Index, ScanStats, SizeKind, TreeRow};
pub use walk::{default_threads, scan, ScanError, ScanOptions};
