//! Keeping an index current without rescanning it. Look at the folders a
//! change can touch, before and after the change; then patch only those
//! folders' totals, drop the subfolders that went away and scan only the
//! subfolders that appeared.
//!
//! A folder looked at before and after, whose index entry still matched it
//! before, is patched by exactly what the change did to it. Any other
//! folder (one the index had fallen behind on, or one only looked at
//! afterwards, such as where a moved item landed) is set to what is there
//! now. When hard links make it impossible to tell where a file counts, the
//! patch refuses, and the index has to be rescanned.

use crate::entry::{Kind, ReadOptions, flags};
use crate::index::{FileHit, Index, NO_PARENT, node_flags};
use crate::list::list_dir;
use crate::path::{bytes_to_path, path_bytes, path_to_cbytes};
use crate::walk::{ScanError, ScanOptions, scan};
use rustc_hash::FxHashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Above this many folders, nothing is looked at before the change, and
/// every folder is set to what is there afterwards.
const MAX_BEFORE: usize = 20_000;

/// The flag bits that come from entry names (the rest is scan state).
const MARKERS: u16 = !(node_flags::UNREADABLE | node_flags::MOUNT_SKIPPED);

/// A subfolder, as its parent folder lists it.
#[derive(Clone, Debug)]
pub struct SubDir {
    pub name: Box<[u8]>,
    pub dev: u64,
    pub ino: u64,
    /// The subfolder's own entry (its inode), which counts toward the parent.
    pub logical: u64,
    pub alloc: u64,
}

/// What one folder holds directly, at one moment.
#[derive(Clone, Debug, Default)]
pub struct DirState {
    pub dev: u64,
    /// Everything but subfolders, counted the way a scan counts them (hard
    /// links included: whether a scan counts them here depends on where it
    /// met them first).
    pub files: u64,
    pub logical: u64,
    pub alloc: u64,
    pub private: u64,
    /// Newest modification time among those entries.
    pub mtime_max: i64,
    /// Marker bits for the names inside (see `node_flags::marker`).
    pub markers: u16,
    /// Order-independent digest of the non-folder entries' names, sizes
    /// and times: a rename shows up even when the totals don't change.
    pub digest: u64,
    /// Files with more than one link, among the above.
    pub linked_files: u64,
    pub linked_logical: u64,
    pub linked_alloc: u64,
    pub linked_digest: u64,
    /// Sorted by name.
    pub subdirs: Vec<SubDir>,
}

impl DirState {
    fn counts(&self, d: &SubDir, cross_mounts: bool) -> bool {
        cross_mounts || d.dev == self.dev
    }

    /// (logical, alloc) of the subfolder entries a scan would count here.
    fn subdir_bytes(&self, cross_mounts: bool) -> (u64, u64) {
        self.subdirs.iter().filter(|d| self.counts(d, cross_mounts)).fold((0, 0), |(l, a), d| (l + d.logical, a + d.alloc))
    }
}

/// Look at `dir` now. `None` when it is gone or unreadable.
pub fn observe(dir: &Path, private: bool) -> Option<DirState> {
    let c = path_to_cbytes(dir);
    let dev = crate::platform::dev_of(std::ffi::CStr::from_bytes_with_nul(&c).ok()?).ok()?;
    if !std::fs::symlink_metadata(dir).ok()?.is_dir() {
        return None;
    }
    let entries = list_dir(dir, ReadOptions { private }).ok()?;
    let mut s = DirState { dev, ..Default::default() };
    for e in entries {
        s.markers |= node_flags::marker(&e.name);
        if e.kind == Kind::Dir {
            s.subdirs.push(SubDir { name: e.name, dev: e.dev, ino: e.ino, logical: e.logical, alloc: e.alloc });
            continue;
        }
        // Dataless (cloud placeholder) files occupy no local blocks.
        let alloc = if e.flags & flags::DATALESS != 0 { 0 } else { e.alloc };
        s.files += 1;
        s.logical += e.logical;
        s.alloc += alloc;
        s.private += e.private.unwrap_or(alloc).min(alloc);
        s.mtime_max = s.mtime_max.max(e.mtime);
        let mut h = rustc_hash::FxHasher::default();
        (&*e.name, e.logical, alloc, e.mtime).hash(&mut h);
        let h = mix(h.finish());
        s.digest = s.digest.wrapping_add(h);
        if e.kind == Kind::File && e.nlink > 1 {
            s.linked_files += 1;
            s.linked_logical += e.logical;
            s.linked_alloc += alloc;
            s.linked_digest = s.linked_digest.wrapping_add(h);
        }
    }
    s.subdirs.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Some(s)
}

/// splitmix64 finaliser, so summed hashes don't cancel out.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// One folder around a change.
#[derive(Clone, Debug)]
pub struct DirChange {
    pub dir: PathBuf,
    /// `None`: not looked at before; set to what is there now.
    pub before: Option<DirState>,
    /// `None`: gone (its parent's change removes it).
    pub after: Option<DirState>,
}

/// Everything a change did to the folders that were watched.
#[derive(Clone, Debug, Default)]
pub struct Changes {
    /// When watching began and when the change was over (ms). A scan that
    /// started after `ended_ms` already shows the change; one that ran
    /// across the window may show part of it.
    pub started_ms: i64,
    pub ended_ms: i64,
    pub dirs: Vec<DirChange>,
    /// Folders rewritten in place (compressed, say): rescanned whole.
    pub rescan: Vec<PathBuf>,
}

/// Folders to look at around a change.
#[derive(Default)]
pub struct Watch {
    private: bool,
    dirs: Vec<PathBuf>,
    rescan: Vec<PathBuf>,
    within: Option<Vec<PathBuf>>,
}

/// The folder that holds `path`, or its nearest ancestor that exists.
fn holder(path: &Path) -> Option<PathBuf> {
    let mut d = path.parent()?;
    loop {
        if d.is_dir() {
            return std::fs::canonicalize(d).ok();
        }
        d = d.parent()?;
    }
}

impl Watch {
    /// `private`: measure unshared bytes (free on APFS, slow on Linux).
    pub fn new(private: bool) -> Self {
        Watch { private, ..Default::default() }
    }

    /// Only look at folders inside these (the folders some index covers).
    pub fn within(mut self, roots: Vec<PathBuf>) -> Self {
        self.within = Some(roots);
        self
    }

    /// Watch the folder holding `path`, or its nearest existing ancestor
    /// when that folder doesn't exist yet. Its parent is watched too: a
    /// folder's own entry can change size as entries come and go, and that
    /// entry counts toward the parent.
    pub fn add(&mut self, path: &Path) {
        if let Some(d) = holder(path) {
            if let Some(p) = d.parent() {
                self.dirs.push(p.to_path_buf());
            }
            self.dirs.push(d);
        }
    }

    /// `path` is a folder the change rewrites in place: it keeps its name,
    /// so only a scan can tell what's in it afterwards.
    pub fn rescan(&mut self, path: &Path) {
        if let Ok(p) = std::fs::canonicalize(path) {
            self.add(&p);
            self.rescan.push(p);
        }
    }

    /// Look at every watched folder now, before the change.
    pub fn start(mut self) -> Recording {
        use rayon::prelude::*;
        let started_ms = crate::fmt::now_millis();
        self.dirs.sort_unstable();
        self.dirs.dedup();
        if let Some(roots) = &self.within {
            self.dirs.retain(|d| inside(d, roots));
        }
        let private = self.private;
        let before = if self.dirs.len() > MAX_BEFORE {
            self.dirs.into_iter().map(|d| (d, None)).collect()
        } else {
            self.dirs
                .into_par_iter()
                .map(|d| {
                    let s = observe(&d, private);
                    (d, s)
                })
                .collect()
        };
        Recording { private, before, rescan: self.rescan, started_ms, within: self.within }
    }
}

fn inside(d: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| d.starts_with(r))
}

/// Folders as they were before a change.
pub struct Recording {
    private: bool,
    before: Vec<(PathBuf, Option<DirState>)>,
    rescan: Vec<PathBuf>,
    pub started_ms: i64,
    within: Option<Vec<PathBuf>>,
}

impl Recording {
    /// Look at the same folders again, now that the change is done.
    /// `landed`: paths items moved to (trash, quarantine, destinations).
    /// The folders holding them are looked at too, with every folder above
    /// them up to the outermost watched root, since the change may have
    /// created some of them (`quarantine/<plan>/<entry>/`).
    pub fn finish(self, landed: impl IntoIterator<Item = PathBuf>) -> Changes {
        use rayon::prelude::*;
        let ended_ms = crate::fmt::now_millis();
        let mut seen: FxHashSet<PathBuf> = self.before.iter().map(|(d, _)| d.clone()).collect();
        let mut todo: Vec<(PathBuf, Option<DirState>)> = self.before;
        for p in landed {
            let Some(d) = holder(&p) else { continue };
            let up: Vec<PathBuf> = match &self.within {
                Some(roots) => {
                    let Some(top) = roots.iter().filter(|r| d.starts_with(r)).min_by_key(|r| r.components().count()) else { continue };
                    d.ancestors().take_while(|a| a.starts_with(top)).map(Path::to_path_buf).collect()
                }
                None => d.ancestors().take(2).map(Path::to_path_buf).collect(),
            };
            for a in up {
                if seen.insert(a.clone()) {
                    todo.push((a, None));
                }
            }
        }
        let private = self.private;
        let dirs = todo.into_par_iter().map(|(dir, before)| DirChange { after: observe(&dir, private), dir, before }).collect();
        Changes { started_ms: self.started_ms, ended_ms, dirs, rescan: self.rescan }
    }
}

/// What a patch did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Patched {
    /// Folders whose own entries changed.
    pub dirs: usize,
    /// Subfolders that went away (or were rescanned).
    pub removed: usize,
    /// Subfolders that appeared (or were rescanned) and were scanned.
    pub grafted: usize,
}

impl Patched {
    pub fn is_empty(&self) -> bool {
        *self == Patched::default()
    }
}

#[inline]
fn adj(v: u64, d: i128) -> u64 {
    (v as i128 + d).clamp(0, u64::MAX as i128) as u64
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Delta {
    files: i128,
    logical: i128,
    alloc: i128,
    private: i128,
    dirs: i128,
}

/// One folder's own entries, brought up to date.
struct Fix {
    id: u32,
    delta: Delta,
    own_alloc: u64,
    mtime: i64,
    markers: u16,
}

/// A subfolder that appeared, scanned (or found unreadable).
enum Graft {
    Scanned(Box<Index>),
    Unreadable,
}

impl Index {
    /// Add `d` to folder `id` and every folder above it, and raise their
    /// newest-change time to `mtime`.
    fn add_up(&mut self, mut id: u32, d: Delta, mtime: i64) {
        loop {
            let n = &mut self.nodes[id as usize];
            n.files = adj(n.files, d.files);
            n.logical = adj(n.logical, d.logical);
            n.alloc = adj(n.alloc, d.alloc);
            n.private = adj(n.private, d.private);
            n.dirs = adj(n.dirs as u64, d.dirs).min(u32::MAX as u64) as u32;
            n.mtime_max = n.mtime_max.max(mtime);
            if id == 0 || n.parent == NO_PARENT {
                break;
            }
            id = n.parent;
        }
    }

    /// Bring the index up to date with `changes` (folders outside it are
    /// ignored). `None` when it can't be: its own folder is gone or was
    /// rewritten, or hard links make the counts ambiguous. The index is
    /// unchanged then, and only a scan can bring it up to date.
    ///
    /// Newest-change times only move forward: a folder that lost its newest
    /// file keeps that file's time until the next scan.
    pub fn patch(&mut self, changes: &Changes) -> Option<Patched> {
        if !self.root.is_dir() || changes.rescan.iter().any(|r| self.root.starts_with(r)) {
            return None;
        }
        let cross = self.stats.cross_mounts;

        // ---- plan: nothing changes until every folder checks out ----------
        let mut fixes: Vec<Fix> = vec![];
        let mut gone: Vec<u32> = vec![];
        let mut new_dirs: Vec<(u32, PathBuf)> = vec![];
        let mut files_changed = false;
        for ch in &changes.dirs {
            let Some(after) = &ch.after else { continue };
            let Some(id) = self.lookup(&ch.dir) else { continue };
            let node = self.nodes[id as usize];
            let (f, l, a, p) = self.own_part(id);
            let (al, aa) = after.subdir_bytes(cross);
            let exact = match &ch.before {
                Some(b) => {
                    if b.linked_digest != after.linked_digest {
                        // a hard-linked file came, went or changed: where it
                        // counts depends on where a scan meets it first
                        return None;
                    }
                    let (bl, ba) = b.subdir_bytes(cross);
                    let (bf, bl, ba) = (b.files, b.logical + bl, b.alloc + ba);
                    // Did the index still match the folder before the change?
                    // Hard-linked files may be counted elsewhere, so with them
                    // it only has to fall within the possible range.
                    let in_step = (f, l, a) == (bf, bl, ba)
                        || (b.linked_files > 0
                            && f <= bf
                            && bf <= f + b.linked_files
                            && l <= bl
                            && bl <= l + b.linked_logical
                            && a <= ba
                            && ba <= a + b.linked_alloc);
                    match in_step {
                        true => Some(b),
                        false if b.linked_files == 0 => None,
                        false => return None,
                    }
                }
                None if after.linked_files > 0 => return None,
                None => None,
            };
            let (delta, own_alloc) = match exact {
                // what the change did
                Some(b) => {
                    let (bl, ba) = b.subdir_bytes(cross);
                    files_changed |= b.digest != after.digest;
                    let d = Delta {
                        files: after.files as i128 - b.files as i128,
                        logical: (after.logical + al) as i128 - (b.logical + bl) as i128,
                        alloc: (after.alloc + aa) as i128 - (b.alloc + ba) as i128,
                        private: (after.private + aa) as i128 - (b.private + ba) as i128,
                        dirs: 0,
                    };
                    (d, adj(node.own_alloc, after.alloc as i128 - b.alloc as i128))
                }
                // what is there now
                None => {
                    let d = Delta {
                        files: after.files as i128 - f as i128,
                        logical: (after.logical + al) as i128 - l as i128,
                        alloc: (after.alloc + aa) as i128 - a as i128,
                        private: (after.private + aa) as i128 - p as i128,
                        dirs: 0,
                    };
                    // (the index keeps no apparent size for files alone)
                    files_changed |= d.files != 0 || after.alloc != node.own_alloc;
                    (d, after.alloc)
                }
            };
            fixes.push(Fix { id, delta, own_alloc, mtime: after.mtime_max, markers: after.markers });

            // subfolders: the index's against what is there now
            let here = |name: &[u8]| {
                after.subdirs.binary_search_by(|d| (*d.name).cmp(name)).ok().is_some_and(|i| after.counts(&after.subdirs[i], cross))
            };
            // the same name on another folder: replaced (or swapped in, as
            // APFS compression does), so it is rescanned
            let replaced = |name: &[u8]| {
                let Some(b) = &ch.before else { return false };
                let find = |s: &DirState| s.subdirs.binary_search_by(|d| (*d.name).cmp(name)).ok().map(|i| s.subdirs[i].ino);
                matches!((find(b), find(after)), (Some(x), Some(y)) if x != y)
            };
            let mut known: FxHashSet<&[u8]> = FxHashSet::default();
            for &c in self.children(id) {
                let name = self.name(c);
                if !here(name) {
                    gone.push(c);
                } else if replaced(name) {
                    gone.push(c);
                    files_changed = true;
                } else {
                    known.insert(name);
                }
            }
            for d in after.subdirs.iter().filter(|d| after.counts(d, cross)) {
                if !known.contains(&*d.name) {
                    new_dirs.push((id, ch.dir.join(bytes_to_path(&d.name))));
                }
            }
        }
        for r in &changes.rescan {
            if let Some(id) = self.lookup(r)
                && id != 0
            {
                gone.push(id);
                new_dirs.push((self.nodes[id as usize].parent, r.clone()));
                files_changed = true;
            }
        }

        // the outermost folders that go (their descendants go with them)
        let gone_set: FxHashSet<u32> = gone.iter().copied().collect();
        let outer: Vec<u32> = gone_set
            .iter()
            .copied()
            .filter(|&g| {
                let mut p = self.nodes[g as usize].parent;
                while p != NO_PARENT {
                    if gone_set.contains(&p) {
                        return false;
                    }
                    if p == 0 {
                        break;
                    }
                    p = self.nodes[p as usize].parent;
                }
                true
            })
            .collect();
        let mut dead = vec![false; if outer.is_empty() { 0 } else { self.nodes.len() }];
        if !outer.is_empty() {
            for &g in &outer {
                dead[g as usize] = true;
            }
            // parents come first, so one pass marks every descendant
            for i in 1..self.nodes.len() {
                if !dead[i] && dead[self.nodes[i].parent as usize] {
                    dead[i] = true;
                }
            }
        }

        // What the largest-files list can still vouch for. Folders moving
        // keep it true as far as it goes; files changing anywhere don't.
        let kept_top = if dead.is_empty() { self.top_files.len() } else { self.top_files.iter().filter(|f| !dead[f.dir as usize]).count() };
        let full_top = self.stats.top_k > 0 && self.top_files.len() >= self.stats.top_k;
        let top_k = if files_changed {
            0
        } else if full_top && kept_top < self.top_files.len() {
            kept_top
        } else {
            self.stats.top_k
        };

        // the new folders are scanned now, so a failure leaves the index as it was
        new_dirs.sort_unstable_by(|a, b| a.1.cmp(&b.1));
        new_dirs.dedup_by(|a, b| a.1 == b.1);
        let opts = ScanOptions {
            threads: 0,
            cross_mounts: cross,
            private: self.stats.private_measured,
            top_k,
            top_by: self.top_by,
            progress: None,
        };
        let mut grafts: Vec<(u32, PathBuf, Graft)> = vec![];
        for (parent, path) in new_dirs {
            if !dead.is_empty() && dead[parent as usize] {
                continue;
            }
            match scan(&path, &opts) {
                // a hard-linked file inside may also be counted elsewhere
                Ok(sub) if sub.stats.linked_files > 0 => return None,
                Ok(sub) => grafts.push((parent, path, Graft::Scanned(Box::new(sub)))),
                Err(ScanError::Root { source, .. }) if source.kind() == std::io::ErrorKind::PermissionDenied => {
                    grafts.push((parent, path, Graft::Unreadable))
                }
                // gone again already
                Err(_) => {}
            }
        }

        // ---- apply ------------------------------------------------------------
        let mut out = Patched::default();
        for fx in fixes {
            if fx.delta != Delta::default() {
                out.dirs += 1;
            }
            self.add_up(fx.id, fx.delta, fx.mtime);
            let n = &mut self.nodes[fx.id as usize];
            n.own_alloc = fx.own_alloc;
            n.flags = (n.flags & !MARKERS) | (fx.markers & MARKERS);
        }
        let mut map: Option<Vec<u32>> = None;
        if !outer.is_empty() {
            for &g in &outer {
                let n = self.nodes[g as usize];
                let d = Delta {
                    files: -(n.files as i128),
                    logical: -(n.logical as i128),
                    alloc: -(n.alloc as i128),
                    private: -(n.private as i128),
                    dirs: -(n.dirs as i128 + 1),
                };
                self.add_up(n.parent, d, i64::MIN);
            }
            out.removed = outer.len();
            let total = self.nodes.len();
            let mut m = vec![NO_PARENT; total];
            let mut nodes = Vec::with_capacity(total);
            let mut names = Vec::with_capacity(self.names.len());
            for i in 0..total {
                if dead[i] {
                    continue;
                }
                m[i] = nodes.len() as u32;
                let mut n = self.nodes[i];
                let name = &self.names[n.name_off as usize..n.name_off as usize + n.name_len as usize];
                n.name_off = names.len() as u32;
                names.extend_from_slice(name);
                if i != 0 {
                    n.parent = m[n.parent as usize];
                }
                nodes.push(n);
            }
            self.nodes = nodes;
            self.names = names;
            self.top_files.retain(|f| !dead[f.dir as usize]);
            for f in &mut self.top_files {
                f.dir = m[f.dir as usize];
            }
            map = Some(m);
        }
        self.stats.top_k = top_k;
        if top_k == 0 {
            self.top_files.clear();
        }
        for (parent, path, g) in grafts {
            let parent = map.as_ref().map_or(parent, |m| m[parent as usize]);
            let base = self.nodes.len() as u32;
            let name = path_bytes(&path).rsplit(|&b| b == b'/').next().unwrap_or_default();
            let name = &name[..name.len().min(u16::MAX as usize)];
            match g {
                Graft::Unreadable => {
                    // as a scan records it: an empty folder it couldn't read
                    let n = crate::index::DirNode {
                        parent,
                        name_off: self.names.len() as u32,
                        name_len: name.len() as u16,
                        flags: node_flags::UNREADABLE,
                        ..Default::default()
                    };
                    self.names.extend_from_slice(name);
                    self.nodes.push(n);
                    self.add_up(parent, Delta { dirs: 1, ..Default::default() }, i64::MIN);
                    self.stats.errors += 1;
                }
                Graft::Scanned(sub) => {
                    let sub = *sub;
                    for (i, n) in sub.nodes.iter().enumerate() {
                        let mut n = *n;
                        let nm = if i == 0 { name } else { sub.name(i as u32) };
                        n.name_off = self.names.len() as u32;
                        n.name_len = nm.len() as u16;
                        self.names.extend_from_slice(nm);
                        n.parent = if i == 0 { parent } else { n.parent + base };
                        self.nodes.push(n);
                    }
                    let r = sub.nodes[0];
                    let d = Delta {
                        files: r.files as i128,
                        logical: r.logical as i128,
                        alloc: r.alloc as i128,
                        private: r.private as i128,
                        dirs: r.dirs as i128 + 1,
                    };
                    self.add_up(parent, d, r.mtime_max);
                    self.stats.errors += sub.stats.errors;
                    self.stats.mounts_skipped += sub.stats.mounts_skipped;
                    self.stats.symlinks += sub.stats.symlinks;
                    self.stats.others += sub.stats.others;
                    if top_k > 0 {
                        self.top_files.extend(sub.top_files.into_iter().map(|f| FileHit { dir: f.dir + base, ..f }));
                    }
                }
            }
            out.grafted += 1;
        }
        if top_k > 0 && out.grafted > 0 {
            let by = self.top_by;
            self.top_files.sort_unstable_by_key(|f| std::cmp::Reverse(f.size(by)));
            self.top_files.truncate(top_k);
        }
        if map.is_some() || out.grafted > 0 {
            self.reset_children();
        }
        self.stats.files = self.nodes[0].files;
        self.stats.dirs = self.nodes[0].dirs as u64 + 1;
        Some(out)
    }
}
