//! Parallel work-stealing walker.
//!
//! Each directory is one rayon job. The worker that reads a directory owns
//! its `DirNode` exclusively while reading, so per-file accounting needs no
//! synchronisation; the only shared state is the id counter, the hardlink
//! set (only touched for `nlink > 1`) and atomics for errors.

use crate::entry::{EntryInfo, Kind, ReadOptions, flags};
use crate::index::{DirNode, FileHit, Index, NO_PARENT, ScanStats, SizeKind, node_flags};
use crate::path::{join_cbytes, path_to_cbytes};
use crate::platform::{self, Reader};
use parking_lot::Mutex;
use rustc_hash::FxHashSet;
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Worker threads; 0 = auto.
    pub threads: usize,
    /// Descend into directories on other devices.
    pub cross_mounts: bool,
    /// Measure unshared bytes (APFS private size / Linux FIEMAP).
    pub private: bool,
    /// How many largest files to retain individually.
    pub top_k: usize,
    /// Size used to rank retained files.
    pub top_by: SizeKind,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions { threads: 0, cross_mounts: false, private: false, top_k: 256, top_by: SizeKind::Alloc }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("{path}: {source}")]
    Root { path: PathBuf, source: std::io::Error },
    #[error("{0}: not a directory")]
    NotDir(PathBuf),
    #[error("thread pool: {0}")]
    Pool(String),
}

/// Default thread count: walking is latency-bound (metadata I/O), so more
/// threads than cores helps until the filesystem's metadata locks saturate.
pub fn default_threads() -> usize {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    (cores * 2).clamp(4, 32)
}

struct Job {
    id: u32,
    parent: u32,
    path: Box<[u8]>, // NUL-terminated
}

#[derive(Default)]
struct LocalStats {
    symlinks: u64,
    others: u64,
    errors: u64,
    hardlinks: u64,
    mounts_skipped: u64,
}

struct TopK {
    k: usize,
    by: SizeKind,
    items: Vec<FileHit>,
    floor: u64,
}

impl TopK {
    fn new(k: usize, by: SizeKind) -> Self {
        TopK { k, by, items: Vec::with_capacity(k * 2), floor: 0 }
    }
    #[inline]
    fn would_take(&self, key: u64) -> bool {
        self.k > 0 && (key > self.floor || self.items.len() < self.k)
    }
    fn push(&mut self, hit: FileHit) {
        self.items.push(hit);
        if self.items.len() >= self.k * 2 {
            self.compact();
        }
    }
    fn compact(&mut self) {
        let by = self.by;
        if self.items.len() > self.k {
            self.items.select_nth_unstable_by(self.k - 1, |a, b| b.size(by).cmp(&a.size(by)));
            self.items.truncate(self.k);
        }
        if self.items.len() >= self.k {
            self.floor = self.items.iter().map(|h| h.size(by)).min().unwrap_or(0);
        }
    }
}

struct Worker {
    reader: Reader,
    nodes: Vec<(u32, DirNode)>,
    names: Vec<u8>,
    top: TopK,
    pend: Vec<(u32, u32)>,
    pend_names: Vec<u8>,
    stats: LocalStats,
    samples: Vec<String>,
}

#[repr(align(128))]
struct Padded<T>(T);

const LINK_SHARDS: usize = 64;

struct Ctx {
    workers: Vec<Padded<Mutex<Worker>>>,
    next_id: AtomicU32,
    root_dev: u64,
    cross_mounts: bool,
    links: Vec<Mutex<FxHashSet<(u64, u64)>>>,
}

impl Ctx {
    #[inline]
    fn first_link(&self, dev: u64, ino: u64) -> bool {
        let shard = ((ino ^ dev.rotate_left(17)).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) as usize % LINK_SHARDS;
        self.links[shard].lock().insert((dev, ino))
    }
}

/// Scan `root` and build an [`Index`].
pub fn scan(root: &Path, opts: &ScanOptions) -> Result<Index, ScanError> {
    let started = Instant::now();
    let root = std::fs::canonicalize(root).map_err(|e| ScanError::Root { path: root.to_path_buf(), source: e })?;
    let md = std::fs::metadata(&root).map_err(|e| ScanError::Root { path: root.clone(), source: e })?;
    if !md.is_dir() {
        return Err(ScanError::NotDir(root));
    }
    let root_c = path_to_cbytes(&root);
    let root_cstr = CStr::from_bytes_with_nul(&root_c).expect("nul");
    let root_dev = platform::dev_of(root_cstr).map_err(|e| ScanError::Root { path: root.clone(), source: e })?;
    let threads = if opts.threads == 0 { default_threads() } else { opts.threads };
    let read_opts = ReadOptions { private: opts.private };

    let ctx = Ctx {
        workers: (0..threads)
            .map(|_| {
                Padded(Mutex::new(Worker {
                    reader: Reader::new(read_opts),
                    nodes: Vec::new(),
                    names: Vec::new(),
                    top: TopK::new(opts.top_k, opts.top_by),
                    pend: Vec::new(),
                    pend_names: Vec::new(),
                    stats: LocalStats::default(),
                    samples: Vec::new(),
                }))
            })
            .collect(),
        next_id: AtomicU32::new(1),
        root_dev,
        cross_mounts: opts.cross_mounts,
        links: (0..LINK_SHARDS).map(|_| Mutex::new(FxHashSet::default())).collect(),
    };

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("tidy-walk-{i}"))
        .stack_size(256 * 1024)
        .build()
        .map_err(|e| ScanError::Pool(e.to_string()))?;

    let root_job = Job { id: 0, parent: NO_PARENT, path: root_c.clone().into_boxed_slice() };
    pool.scope(|s| process(s, root_job, &ctx));
    drop(pool);

    // ---- merge ---------------------------------------------------------
    let n = ctx.next_id.load(Ordering::Relaxed) as usize;
    let mut nodes = vec![DirNode::default(); n];
    let mut names = Vec::new();
    let mut top_all: Vec<FileHit> = Vec::new();
    let mut stats = ScanStats { threads, backend: platform::BACKEND, private_measured: opts.private, ..Default::default() };
    for w in ctx.workers {
        let w = w.0.into_inner();
        let base = names.len() as u32;
        names.extend_from_slice(&w.names);
        for (id, mut node) in w.nodes {
            node.name_off += base;
            nodes[id as usize] = node;
        }
        top_all.extend(w.top.items);
        stats.symlinks += w.stats.symlinks;
        stats.others += w.stats.others;
        stats.errors += w.stats.errors;
        stats.hardlinks_deduped += w.stats.hardlinks;
        stats.mounts_skipped += w.stats.mounts_skipped;
        for s in w.samples {
            if stats.error_samples.len() < 32 {
                stats.error_samples.push(s);
            }
        }
    }
    if nodes[0].flags & node_flags::UNREADABLE != 0 {
        let err = std::fs::read_dir(&root).err().unwrap_or_else(|| std::io::Error::other("unreadable"));
        return Err(ScanError::Root { path: root, source: err });
    }

    // ---- roll subtree totals up (child id > parent id) ---------------------
    for i in (1..n).rev() {
        let c = nodes[i];
        let p = &mut nodes[c.parent as usize];
        p.files += c.files;
        p.dirs += c.dirs;
        p.logical += c.logical;
        p.alloc += c.alloc;
        p.private += c.private;
        if c.mtime_max > p.mtime_max {
            p.mtime_max = c.mtime_max;
        }
    }

    let by = opts.top_by;
    top_all.sort_unstable_by_key(|b| std::cmp::Reverse(b.size(by)));
    top_all.truncate(opts.top_k);
    stats.files = nodes[0].files;
    stats.dirs = nodes[0].dirs as u64 + 1;
    stats.elapsed_ms = started.elapsed().as_millis() as u64;
    names.shrink_to_fit();
    Ok(Index::new(root, nodes, names, top_all, by, stats))
}

fn process<'s>(s: &rayon::Scope<'s>, job: Job, ctx: &'s Ctx) {
    let wi = rayon::current_thread_index().unwrap_or(0);
    let mut children: Vec<Job> = Vec::new();
    {
        let mut guard = ctx.workers[wi].0.lock();
        let Worker { reader, nodes, names, top, pend, pend_names, stats, samples } = &mut *guard;

        let mut node = DirNode { parent: job.parent, ..Default::default() };
        let name: &[u8] = if job.id == 0 {
            b""
        } else {
            let p = &job.path[..job.path.len() - 1];
            let cut = p.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
            &p[cut..]
        };
        node.name_off = names.len() as u32;
        node.name_len = name.len().min(u16::MAX as usize) as u16;
        names.extend_from_slice(&name[..node.name_len as usize]);
        pend.clear();
        pend_names.clear();

        let cpath = CStr::from_bytes_with_nul(&job.path).expect("nul-terminated job path");
        let res = reader.read(cpath, &mut |e: &EntryInfo<'_>| {
            node.flags |= node_flags::marker(e.name);
            match e.kind {
                Kind::Dir => {
                    if !ctx.cross_mounts && e.dev != ctx.root_dev {
                        node.flags |= node_flags::MOUNT_SKIPPED;
                        stats.mounts_skipped += 1;
                        return;
                    }
                    // A directory's own inode blocks count toward its parent, like du.
                    node.logical += e.logical;
                    node.alloc += e.alloc;
                    node.private += e.alloc;
                    node.dirs += 1;
                    pend.push((pend_names.len() as u32, e.name.len() as u32));
                    pend_names.extend_from_slice(e.name);
                }
                kind => {
                    if e.nlink > 1 && kind == Kind::File && !ctx.first_link(e.dev, e.ino) {
                        stats.hardlinks += 1;
                        return;
                    }
                    match kind {
                        Kind::Symlink => stats.symlinks += 1,
                        Kind::Other => stats.others += 1,
                        _ => {}
                    }
                    // Dataless (cloud placeholder) files occupy no local blocks.
                    let alloc = if e.flags & flags::DATALESS != 0 { 0 } else { e.alloc };
                    let private = e.private.unwrap_or(alloc).min(alloc);
                    node.files += 1;
                    node.logical += e.logical;
                    node.alloc += alloc;
                    node.private += private;
                    node.own_alloc += alloc;
                    if e.mtime > node.mtime_max {
                        node.mtime_max = e.mtime;
                    }
                    let key = match top.by {
                        SizeKind::Logical => e.logical,
                        SizeKind::Alloc => alloc,
                        SizeKind::Private => private,
                    };
                    if kind == Kind::File && top.would_take(key) {
                        top.push(FileHit {
                            dir: job.id,
                            name: e.name.into(),
                            logical: e.logical,
                            alloc,
                            private,
                            mtime: e.mtime,
                            flags: e.flags,
                        });
                    }
                }
            }
        });
        if let Err(err) = res {
            node.flags |= node_flags::UNREADABLE;
            stats.errors += 1;
            if samples.len() < 32 {
                samples.push(format!("{}: {}", String::from_utf8_lossy(&job.path[..job.path.len() - 1]), err));
            }
        }
        nodes.push((job.id, node));
        children.reserve(pend.len());
        for &(st, ln) in pend.iter() {
            let id = ctx.next_id.fetch_add(1, Ordering::Relaxed);
            let nm = &pend_names[st as usize..(st + ln) as usize];
            children.push(Job { id, parent: job.id, path: join_cbytes(&job.path, nm) });
        }
    }
    for c in children {
        s.spawn(move |s| process(s, c, ctx));
    }
}
