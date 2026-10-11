//! Fingerprints detect whether a path changed between planning and applying.
//!
//! Files: (dev, ino, size, mtime) plus an optional BLAKE3 content hash.
//! Directories: (dev, ino) plus a subtree summary — file count, apparent
//! bytes and newest mtime — recomputed with the parallel scanner.

use serde::{Deserialize, Serialize};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub kind: FKind,
    pub dev: u64,
    pub ino: u64,
    /// File: length. Dir: apparent bytes in subtree.
    pub size: u64,
    /// File: mtime. Dir: newest file mtime in subtree.
    pub mtime: i64,
    /// Dir only: number of files in subtree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<u64>,
    /// File only, when requested: hex BLAKE3 of content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drift {
    Gone,
    Replaced,
    KindChanged,
    Changed(String),
}

impl std::fmt::Display for Drift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Drift::Gone => write!(f, "no longer exists"),
            Drift::Replaced => write!(f, "was replaced by a different file (inode changed)"),
            Drift::KindChanged => write!(f, "changed type"),
            Drift::Changed(s) => write!(f, "changed since planning ({s})"),
        }
    }
}

/// Fingerprint `p` without following a final symlink. For directories,
/// `tree` supplies a precomputed (files, logical, mtime_max) summary — e.g.
/// from the scan index — to avoid rescanning at plan time.
pub fn fingerprint(p: &Path, tree: Option<(u64, u64, i64)>, hash: bool) -> std::io::Result<Fingerprint> {
    let md = std::fs::symlink_metadata(p)?;
    let ft = md.file_type();
    let kind = if ft.is_dir() {
        FKind::Dir
    } else if ft.is_symlink() {
        FKind::Symlink
    } else if ft.is_file() {
        FKind::File
    } else {
        FKind::Other
    };
    let mut fp = Fingerprint { kind, dev: md.dev(), ino: md.ino(), size: md.len(), mtime: md.mtime(), files: None, hash: None };
    match kind {
        FKind::Dir => {
            let (files, logical, mtime) = match tree {
                Some(t) => t,
                None => tree_summary(p)?,
            };
            fp.files = Some(files);
            fp.size = logical;
            fp.mtime = mtime;
        }
        FKind::File if hash => fp.hash = Some(hash_file(p)?),
        _ => {}
    }
    Ok(fp)
}

/// (files, apparent bytes, newest mtime) of a directory subtree.
pub fn tree_summary(p: &Path) -> std::io::Result<(u64, u64, i64)> {
    let opts = tidy_scan::ScanOptions { threads: 4, top_k: 0, ..Default::default() };
    let idx = tidy_scan::scan(p, &opts).map_err(|e| std::io::Error::other(e.to_string()))?;
    let r = idx.root_node();
    Ok((r.files, r.logical, r.mtime_max))
}

pub fn hash_file(p: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = blake3::Hasher::new();
    let len = f.metadata()?.len();
    if len > 16 * 1024 * 1024 {
        h.update_mmap_rayon(p)?;
    } else {
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
    }
    Ok(h.finalize().to_hex().to_string())
}

/// Compare the current state of `p` against a recorded fingerprint.
pub fn verify(p: &Path, want: &Fingerprint) -> Result<(), Drift> {
    let md = match std::fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(_) => return Err(Drift::Gone),
    };
    if md.dev() != want.dev || md.ino() != want.ino {
        return Err(Drift::Replaced);
    }
    let now = fingerprint(p, None, want.hash.is_some()).map_err(|_| Drift::Gone)?;
    if now.kind != want.kind {
        return Err(Drift::KindChanged);
    }
    let mut diffs = Vec::new();
    if now.size != want.size {
        diffs.push(format!("size {} → {}", want.size, now.size));
    }
    if now.mtime != want.mtime {
        diffs.push("modified".to_string());
    }
    if now.files != want.files {
        diffs.push(format!("files {:?} → {:?}", want.files.unwrap_or(0), now.files.unwrap_or(0)));
    }
    if want.hash.is_some() && now.hash != want.hash {
        diffs.push("content".to_string());
    }
    if diffs.is_empty() { Ok(()) } else { Err(Drift::Changed(diffs.join(", "))) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_drift() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("d");
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("a"), b"hello").unwrap();
        let fp = fingerprint(&d, None, false).unwrap();
        assert_eq!(fp.files, Some(1));
        assert!(verify(&d, &fp).is_ok());
        std::fs::write(d.join("b"), b"more").unwrap();
        assert!(matches!(verify(&d, &fp), Err(Drift::Changed(_))));
        let f = d.join("a");
        let ff = fingerprint(&f, None, true).unwrap();
        assert!(verify(&f, &ff).is_ok());
        std::fs::remove_file(&f).unwrap();
        assert_eq!(verify(&f, &ff), Err(Drift::Gone));
    }
}
