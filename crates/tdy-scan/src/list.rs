//! One folder's entries with their sizes, for interactive listings. Large
//! folders are stat-ed on several threads, like in a full scan, on a small
//! pool of their own: work queued on rayon's global pool (hashing, deletes)
//! must not hold up a listing the screen is waiting for.

use crate::entry::{Kind, ReadOptions};
use crate::path::path_to_cbytes;
use crate::platform::Reader;
use std::io;
use std::path::Path;

/// One entry of a folder listing.
#[derive(Clone, Debug)]
pub struct Listed {
    pub name: Box<[u8]>,
    pub kind: Kind,
    pub logical: u64,
    pub alloc: u64,
    pub private: Option<u64>,
    pub mtime: i64,
    pub flags: u32,
    /// Device of the entry (a folder on another device is a mount point).
    pub dev: u64,
    pub ino: u64,
    pub nlink: u32,
}

fn pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8);
        rayon::ThreadPoolBuilder::new().num_threads(n).thread_name(|i| format!("tdy-list-{i}")).build().ok()
    })
    .as_ref()
}

fn listed(e: &crate::entry::EntryInfo<'_>) -> Listed {
    Listed {
        name: e.name.into(),
        kind: e.kind,
        logical: e.logical,
        alloc: e.alloc,
        private: e.private,
        mtime: e.mtime,
        flags: e.flags,
        dev: e.dev,
        ino: e.ino,
        nlink: e.nlink,
    }
}

/// Every entry directly inside `dir` (not `.`/`..`), in no particular order.
pub fn list_dir(dir: &Path, opts: ReadOptions) -> io::Result<Vec<Listed>> {
    use rayon::prelude::*;
    let c = path_to_cbytes(dir);
    let cstr = std::ffi::CStr::from_bytes_with_nul(&c).map_err(io::Error::other)?;
    let mut reader = Reader::new(opts);
    let mut out = Vec::new();
    let rest = reader.read_split(cstr, 4096, &mut |e| out.push(listed(e)))?;
    if let Some(rest) = rest {
        let n = rest.len();
        let stat = || -> Vec<Vec<Listed>> {
            (0..n.div_ceil(2048))
                .into_par_iter()
                .map(|c| {
                    let mut v = Vec::with_capacity(2048);
                    rest.stat_range(c * 2048..((c + 1) * 2048).min(n), &mut |_, e| v.push(listed(e)));
                    v
                })
                .collect()
        };
        let chunks = match pool() {
            Some(p) => p.install(stat),
            None => stat(),
        };
        out.extend(chunks.into_iter().flatten());
    }
    Ok(out)
}
