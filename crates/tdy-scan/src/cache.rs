//! The automatic index cache. Tidy keeps the index of every full scan it
//! makes of a folder (the newest 64, up to 1 GiB), so the next look at that
//! folder, or at anything inside it, can start from it at once while a
//! fresh scan runs, and `--cached` answers can skip the scan. Applying or
//! undoing a plan patches the cached indexes it touches ([`sync`]).
//!
//! Separate from the snapshots `tdy scan --save` keeps for `tdy diff`.

use crate::fmt::now_millis;
use crate::index::Index;
use crate::patch::Changes;
use crate::snapshot::{self, Peek};
use crate::walk::ScanOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const FILE: &str = "auto.idx";
const MAX_ENTRIES: usize = 64;
const MAX_BYTES: u64 = 1 << 30;

/// Where `root`'s cached index lives.
pub fn path_for(root: &Path) -> PathBuf {
    snapshot::snapshot_dir(root).join(FILE)
}

/// An index read from the cache.
pub struct Cached {
    pub index: Index,
    /// The folder whose scan it came from: the requested folder, or an
    /// ancestor it was cut out of.
    pub from: PathBuf,
}

impl Cached {
    /// Seconds since its scan started.
    pub fn age_secs(&self) -> i64 {
        age_secs(&self.index)
    }
}

/// Seconds since the scan behind `idx` started.
pub fn age_secs(idx: &Index) -> i64 {
    ((now_millis() - idx.stats.started_ms) / 1000).max(0)
}

/// Keep `idx` as its folder's cached index, and trim the cache.
pub fn store(idx: &Index) -> io::Result<()> {
    let p = path_for(&idx.root);
    {
        let _lock = lock(&p);
        snapshot::save(idx, &p)?;
    }
    prune(&p);
    Ok(())
}

/// Whether a cached scan made with these options can answer `want`.
fn suits(p: &Peek, want: &ScanOptions) -> bool {
    (p.private_measured || !want.private) && p.cross_mounts == want.cross_mounts && (want.top_k == 0 || p.top_by == want.top_by)
}

/// The freshest cached index for `root` that `want` can use: its own, or
/// one cut out of a cached ancestor's. `want.top_k == 0` accepts any
/// largest-files list; `want.private` needs unshared bytes.
pub fn load(root: &Path, want: &ScanOptions) -> Option<Cached> {
    let root = std::fs::canonicalize(root).ok()?;
    let mut found: Vec<(Peek, PathBuf)> = root
        .ancestors()
        .filter_map(|a| {
            let f = path_for(a);
            let p = snapshot::peek(&f).ok()?;
            (p.root == a && suits(&p, want)).then_some((p, f))
        })
        .collect();
    found.sort_by_key(|(p, _)| std::cmp::Reverse(p.started_ms));
    for (p, f) in found {
        let Ok(l) = snapshot::load(&f) else { continue };
        touch(&f);
        let mut index = l.index;
        if p.root != root {
            let Some(id) = index.lookup(&root) else { continue };
            index = index.extract(id);
        }
        if want.top_k > index.stats.top_k {
            continue;
        }
        index.stats.backend = "cache";
        return Some(Cached { index, from: p.root });
    }
    None
}

/// Mark a cached index as just used (pruning drops the least recent).
fn touch(f: &Path) {
    if let Ok(file) = std::fs::File::options().write(true).open(f) {
        let _ = file.set_modified(SystemTime::now());
    }
}

fn entries() -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(snapshot::cache_dir().join("snapshots")) else { return vec![] };
    rd.flatten().map(|e| e.path().join(FILE)).filter(|f| f.is_file()).collect()
}

/// Drop the least recently used indexes beyond the limits (never `keep`),
/// and temporary files a crashed save left behind.
fn prune(keep: &Path) {
    let mut all: Vec<(SystemTime, u64, PathBuf)> = entries()
        .into_iter()
        .filter_map(|f| {
            let m = f.metadata().ok()?;
            Some((m.modified().ok()?, m.len(), f))
        })
        .collect();
    all.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut total = 0u64;
    for (i, (_, len, f)) in all.iter().enumerate() {
        total += len;
        if f != keep && (i >= MAX_ENTRIES || total > MAX_BYTES) {
            let _ = std::fs::remove_file(f);
        }
    }
    let hour_ago = SystemTime::now() - Duration::from_secs(3600);
    if let Some(dir) = keep.parent()
        && let Ok(rd) = std::fs::read_dir(dir)
    {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "tmp") && e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < hour_ago) {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

/// The folders the cache holds indexes of.
pub fn roots() -> Vec<PathBuf> {
    entries().iter().filter_map(|f| snapshot::peek(f).ok()).map(|p| p.root).collect()
}

/// What [`sync`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Synced {
    pub patched: usize,
    pub dropped: usize,
    /// The change's window (ms), as in [`Changes`].
    pub started_ms: i64,
    pub ended_ms: i64,
}

/// Holds one root's cached index while it is written, or read, patched and
/// written back: other Tidy processes may be doing the same.
struct Lock(#[allow(dead_code)] Option<std::fs::File>);

fn lock(index_file: &Path) -> Lock {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let p = index_file.with_extension("lock");
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(f) = std::fs::File::options().create(true).truncate(false).write(true).open(&p) {
            // SAFETY: a valid descriptor, owned by `f` for as long as the lock.
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
            return Lock(Some(f));
        }
    }
    #[cfg(not(unix))]
    let _ = index_file;
    Lock(None)
}

/// Patch every cached index `changes` touch.
///
/// - Scanned after the change: it already shows it, so it is left alone.
/// - Scanned while the change ran, or covering a folder rewritten in place
///   or a change the patch can't follow: dropped, so the next look rescans.
/// - Its folder removed by the change: dropped. (An unplugged drive keeps
///   its index.)
pub fn sync(changes: &Changes) -> Synced {
    let mut out = Synced { started_ms: changes.started_ms, ended_ms: changes.ended_ms, ..Default::default() };
    for f in entries() {
        let Ok(p) = snapshot::peek(&f) else { continue };
        let inside = changes.dirs.iter().any(|c| c.dir.starts_with(&p.root)) || changes.rescan.iter().any(|r| r.starts_with(&p.root));
        let above = changes.dirs.iter().any(|c| p.root.starts_with(&c.dir)) || changes.rescan.iter().any(|r| p.root.starts_with(r));
        if !inside && !above {
            continue;
        }
        let _lock = lock(&f);
        // again, under the lock: another process may have replaced it
        let Ok(p) = snapshot::peek(&f) else { continue };
        if p.started_ms > changes.ended_ms {
            continue;
        }
        let overlapped = p.started_ms.saturating_add(p.elapsed_ms as i64) >= changes.started_ms;
        let rewritten = changes.rescan.iter().any(|r| p.root.starts_with(r));
        let drop = if p.root.is_dir() { rewritten || (inside && overlapped) } else { above };
        if !drop && !inside {
            continue;
        }
        let patched = !drop
            && snapshot::load(&f).ok().is_some_and(|l| {
                let mut idx = l.index;
                idx.patch(changes).is_some() && snapshot::save(&idx, &f).is_ok()
            });
        if patched {
            out.patched += 1;
        } else if std::fs::remove_file(&f).is_ok() {
            // dropped, or one the patch couldn't follow
            out.dropped += 1;
        }
    }
    out
}
