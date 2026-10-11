//! Parallel visitor over files (not just directories), for tools that need
//! per-file records — duplicate finding, compressibility sampling. Same
//! work-stealing structure and platform readers as the scanner.

use crate::entry::{EntryInfo, Kind, ReadOptions};
use crate::path::{bytes_to_path, join_cbytes, path_to_cbytes};
use crate::platform::Reader;
use std::ffi::CStr;
use std::path::{Path, PathBuf};

pub struct VisitOptions {
    pub threads: usize,
    pub cross_mounts: bool,
    pub private: bool,
    /// Skip directories whose name starts with '.'.
    pub skip_hidden_dirs: bool,
}

impl Default for VisitOptions {
    fn default() -> Self {
        VisitOptions { threads: 0, cross_mounts: false, private: false, skip_hidden_dirs: false }
    }
}

/// Call `f(dir, entry)` for every non-directory entry under `root`, from
/// many threads. `dir` is the containing directory.
pub fn visit_files<F>(root: &Path, opts: &VisitOptions, f: F) -> std::io::Result<()>
where
    F: Fn(&Path, &EntryInfo<'_>) + Sync,
{
    let root = std::fs::canonicalize(root)?;
    let rc = path_to_cbytes(&root);
    let dev = crate::platform::dev_of(CStr::from_bytes_with_nul(&rc).map_err(std::io::Error::other)?)?;
    let threads = if opts.threads == 0 { crate::walk::default_threads() } else { opts.threads };
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().map_err(std::io::Error::other)?;
    let ro = ReadOptions { private: opts.private };
    thread_local! {
        static READER: std::cell::RefCell<Option<Reader>> = const { std::cell::RefCell::new(None) };
    }
    fn go<'s, F: Fn(&Path, &EntryInfo<'_>) + Sync>(s: &rayon::Scope<'s>, dir: Box<[u8]>, ctx: &'s (F, u64, bool, bool, ReadOptions)) {
        let (f, dev, cross, skip_hidden, ro) = ctx;
        let mut subdirs = Vec::new();
        let dir_path = bytes_to_path(&dir[..dir.len() - 1]);
        READER.with(|r| {
            let mut r = r.borrow_mut();
            let reader = r.get_or_insert_with(|| Reader::new(*ro));
            let Ok(c) = CStr::from_bytes_with_nul(&dir) else { return };
            let _ = reader.read(c, &mut |e| {
                if e.kind == Kind::Dir {
                    if (!*cross && e.dev != *dev) || (*skip_hidden && e.name.first() == Some(&b'.')) {
                        return;
                    }
                    subdirs.push(join_cbytes(&dir, e.name));
                } else {
                    f(&dir_path, e);
                }
            });
        });
        for d in subdirs {
            s.spawn(move |s| go(s, d, ctx));
        }
    }
    let ctx = (f, dev, opts.cross_mounts, opts.skip_hidden_dirs, ro);
    pool.scope(|s| go(s, rc.into_boxed_slice(), &ctx));
    Ok(())
}

/// Join a directory and an entry name.
pub fn entry_path(dir: &Path, name: &[u8]) -> PathBuf {
    dir.join(bytes_to_path(name))
}
