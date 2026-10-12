//! On-disk snapshots of an [`Index`] for fast reload and diffs.
//!
//! Layout (little-endian):
//!   magic "TIDYIDX\x01" | u32 header_len | header JSON | pad to 8
//!   | nodes: [DirNode; n] | names: [u8; m] | top files JSON
//! Nodes are POD, so loading is one read plus a cast.

use crate::index::{DirNode, FileHit, Index, ScanStats, SizeKind};
use crate::path::{bytes_to_path, path_bytes};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"TIDYIDX\x01";

#[derive(Serialize, Deserialize)]
struct Header {
    root: Vec<u8>,
    /// When the file was written (seconds).
    created: i64,
    nodes: u64,
    names: u64,
    top_by: String,
    files: u64,
    dirs: u64,
    elapsed_ms: u64,
    backend: String,
    private_measured: bool,
    // Added after the first release; older snapshots read as defaults.
    #[serde(default)]
    cross_mounts: bool,
    #[serde(default)]
    top_k: usize,
    /// When the scan started (ms); 0 in older snapshots (use `created`).
    #[serde(default)]
    started_ms: i64,
    #[serde(default)]
    errors: u64,
    #[serde(default)]
    mounts_skipped: u64,
    #[serde(default)]
    symlinks: u64,
    #[serde(default)]
    hardlinks_deduped: u64,
}

/// A snapshot's header, read without loading the index.
#[derive(Clone, Debug)]
pub struct Peek {
    pub root: PathBuf,
    /// When the file was written (seconds).
    pub created: i64,
    /// When its scan started (ms), and how long it took.
    pub started_ms: i64,
    pub elapsed_ms: u64,
    pub private_measured: bool,
    pub cross_mounts: bool,
    pub top_k: usize,
    pub top_by: SizeKind,
    pub nodes: u64,
}

#[derive(Serialize, Deserialize)]
struct TopFile {
    dir: u32,
    name: Vec<u8>,
    logical: u64,
    alloc: u64,
    private: u64,
    mtime: i64,
    flags: u32,
}

pub fn save(idx: &Index, path: &Path) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // A private temporary name: two processes saving the same index must
    // not write into one file.
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let tmp = path.with_extension(format!("{}.{nonce}.tmp", std::process::id()));
    let res = write_to(idx, &tmp).and_then(|()| std::fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

fn write_to(idx: &Index, tmp: &Path) -> io::Result<()> {
    let mut f = io::BufWriter::with_capacity(1 << 20, std::fs::File::create(tmp)?);
    let hdr = Header {
        root: path_bytes(&idx.root).to_vec(),
        created: crate::fmt::now_secs(),
        nodes: idx.nodes.len() as u64,
        names: idx.names.len() as u64,
        top_by: format!("{:?}", idx.top_by).to_lowercase(),
        files: idx.stats.files,
        dirs: idx.stats.dirs,
        elapsed_ms: idx.stats.elapsed_ms,
        backend: idx.stats.backend.to_string(),
        private_measured: idx.stats.private_measured,
        cross_mounts: idx.stats.cross_mounts,
        top_k: idx.stats.top_k,
        started_ms: idx.stats.started_ms,
        errors: idx.stats.errors,
        mounts_skipped: idx.stats.mounts_skipped,
        symlinks: idx.stats.symlinks,
        hardlinks_deduped: idx.stats.hardlinks_deduped,
    };
    let hj = serde_json::to_vec(&hdr)?;
    f.write_all(MAGIC)?;
    f.write_all(&(hj.len() as u32).to_le_bytes())?;
    f.write_all(&hj)?;
    let pad = (8 - (12 + hj.len()) % 8) % 8;
    f.write_all(&[0u8; 8][..pad])?;
    f.write_all(bytemuck::cast_slice(&idx.nodes))?;
    f.write_all(&idx.names)?;
    let tops: Vec<TopFile> = idx
        .top_files
        .iter()
        .map(|h| TopFile {
            dir: h.dir,
            name: h.name.to_vec(),
            logical: h.logical,
            alloc: h.alloc,
            private: h.private,
            mtime: h.mtime,
            flags: h.flags,
        })
        .collect();
    serde_json::to_writer(&mut f, &tops)?;
    f.flush()?;
    Ok(())
}

pub struct Loaded {
    pub index: Index,
    pub created: i64,
}

fn read_header(f: &mut impl Read) -> io::Result<Header> {
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a tidy snapshot"));
    }
    let mut lb = [0u8; 4];
    f.read_exact(&mut lb)?;
    let hl = u32::from_le_bytes(lb) as usize;
    if hl > 1 << 20 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "snapshot header too large"));
    }
    let mut hj = vec![0u8; hl];
    f.read_exact(&mut hj)?;
    let hdr: Header = serde_json::from_slice(&hj).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let pad = (8 - (12 + hl) % 8) % 8;
    let mut padb = [0u8; 8];
    f.read_exact(&mut padb[..pad])?;
    Ok(hdr)
}

fn started_ms(h: &Header) -> i64 {
    if h.started_ms > 0 { h.started_ms } else { h.created * 1000 }
}

/// Read only a snapshot's header.
pub fn peek(path: &Path) -> io::Result<Peek> {
    let mut f = io::BufReader::with_capacity(8 << 10, std::fs::File::open(path)?);
    let h = read_header(&mut f)?;
    Ok(Peek {
        root: bytes_to_path(&h.root),
        created: h.created,
        started_ms: started_ms(&h),
        elapsed_ms: h.elapsed_ms,
        private_measured: h.private_measured,
        cross_mounts: h.cross_mounts,
        top_k: h.top_k,
        top_by: h.top_by.parse().unwrap_or_default(),
        nodes: h.nodes,
    })
}

pub fn load(path: &Path) -> io::Result<Loaded> {
    let file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let mut f = io::BufReader::with_capacity(1 << 20, file);
    let hdr = read_header(&mut f)?;
    // Don't trust the counts with an allocation the file can't back.
    if hdr.nodes.saturating_mul(std::mem::size_of::<DirNode>() as u64).saturating_add(hdr.names) > size {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "snapshot is truncated"));
    }
    let mut nodes = vec![DirNode::default(); hdr.nodes as usize];
    f.read_exact(bytemuck::cast_slice_mut(&mut nodes))?;
    let mut names = vec![0u8; hdr.names as usize];
    f.read_exact(&mut names)?;
    let mut rest = Vec::new();
    f.read_to_end(&mut rest)?;
    let tops: Vec<TopFile> = serde_json::from_slice(&rest).map_err(io::Error::other)?;
    let top_files = tops
        .into_iter()
        .map(|t| FileHit {
            dir: t.dir,
            name: t.name.into(),
            logical: t.logical,
            alloc: t.alloc,
            private: t.private,
            mtime: t.mtime,
            flags: t.flags,
        })
        .collect();
    let stats = ScanStats {
        files: hdr.files,
        dirs: hdr.dirs,
        elapsed_ms: hdr.elapsed_ms,
        backend: "snapshot",
        private_measured: hdr.private_measured,
        cross_mounts: hdr.cross_mounts,
        top_k: hdr.top_k,
        started_ms: started_ms(&hdr),
        errors: hdr.errors,
        mounts_skipped: hdr.mounts_skipped,
        symlinks: hdr.symlinks,
        hardlinks_deduped: hdr.hardlinks_deduped,
        ..Default::default()
    };
    let top_by: SizeKind = hdr.top_by.parse().unwrap_or_default();
    let index = Index::new(bytes_to_path(&hdr.root), nodes, names, top_files, top_by, stats);
    index.validate().map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("corrupt snapshot: {e}")))?;
    Ok(Loaded { index, created: hdr.created })
}

/// Default snapshot directory: `$XDG_CACHE_HOME/tidy` or `~/Library/Caches/tidy` / `~/.cache/tidy`.
pub fn cache_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("TIDY_CACHE_DIR") {
        return PathBuf::from(x);
    }
    if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(x).join("tidy");
    }
    let home = crate::path::home_dir();
    if cfg!(target_os = "macos") { home.join("Library/Caches/tidy") } else { home.join(".cache/tidy") }
}

/// Where everything saved for one root lives.
pub fn snapshot_dir(root: &Path) -> PathBuf {
    let h = blake_like_hash(path_bytes(root));
    cache_dir().join("snapshots").join(format!("{h:016x}"))
}

/// Snapshot path for a root: one rolling "latest" plus timestamped history.
pub fn snapshot_path(root: &Path, ts: Option<i64>) -> PathBuf {
    let dir = snapshot_dir(root);
    match ts {
        Some(t) => dir.join(format!("{t}.idx")),
        None => dir.join("latest.idx"),
    }
}

fn blake_like_hash(b: &[u8]) -> u64 {
    // FNV-1a; stable across runs and platforms.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Directories whose subtree size changed, sorted by absolute delta.
pub fn diff(old: &Index, new: &Index, k: SizeKind, min_delta: u64) -> Vec<(PathBuf, i64)> {
    use rustc_hash::FxHashMap;
    let mut old_by_path: FxHashMap<PathBuf, u64> = FxHashMap::default();
    for i in 0..old.nodes.len() as u32 {
        old_by_path.insert(old.path(i), old.nodes[i as usize].size(k));
    }
    let mut out = Vec::new();
    for i in 0..new.nodes.len() as u32 {
        let p = new.path(i);
        let now = new.nodes[i as usize].size(k) as i64;
        let before = old_by_path.remove(&p).unwrap_or(0) as i64;
        let d = now - before;
        if d.unsigned_abs() >= min_delta {
            out.push((p, d));
        }
    }
    for (p, before) in old_by_path {
        if before >= min_delta {
            out.push((p, -(before as i64)));
        }
    }
    out.sort_unstable_by_key(|(_, d)| std::cmp::Reverse(d.unsigned_abs()));
    out
}
