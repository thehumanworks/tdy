//! Directory-only index. Files are aggregated into their directory; only the
//! top-K largest files are kept individually. Node ids are assigned at
//! discovery time, so every child id is greater than its parent's id and a
//! single reverse pass rolls subtree totals up to the root.

use crate::path::{bytes_to_path, path_bytes};
use bytemuck::{Pod, Zeroable};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const NO_PARENT: u32 = u32::MAX;

pub mod node_flags {
    /// The directory could not be opened or read.
    pub const UNREADABLE: u16 = 1 << 0;
    /// A child directory on another device was skipped.
    pub const MOUNT_SKIPPED: u16 = 1 << 1;
    // Marker entries seen directly inside the directory (set during the scan
    // by a cheap name match, so detectors never need extra syscalls).
    pub const M_CACHEDIR_TAG: u16 = 1 << 2;
    pub const M_CARGO_TOML: u16 = 1 << 3;
    pub const M_PACKAGE_JSON: u16 = 1 << 4;
    pub const M_PYVENV_CFG: u16 = 1 << 5;
    pub const M_PACKAGE_SWIFT: u16 = 1 << 6;
    pub const M_GRADLE: u16 = 1 << 7;
    pub const M_GIT: u16 = 1 << 8;
    pub const M_PYPROJECT: u16 = 1 << 9;
    pub const M_POM_XML: u16 = 1 << 10;
    pub const M_GO_MOD: u16 = 1 << 11;
    pub const M_PUBSPEC: u16 = 1 << 12;
    pub const M_CMAKE: u16 = 1 << 13;
    pub const M_XCODEPROJ: u16 = 1 << 14;
    pub const M_MAKEFILE: u16 = 1 << 15;

    /// Marker bit for a directory entry name, if any.
    #[inline]
    pub fn marker(name: &[u8]) -> u16 {
        // Cheap reject: markers start with one of these bytes.
        match name.first() {
            Some(b'C' | b'p' | b'P' | b'b' | b's' | b'.' | b'g' | b'r' | b'M' | b'G' | b'm') => {}
            _ => {
                return if name.ends_with(b".xcodeproj") { M_XCODEPROJ } else { 0 };
            }
        }
        match name {
            b"CACHEDIR.TAG" => M_CACHEDIR_TAG,
            b"Cargo.toml" => M_CARGO_TOML,
            b"package.json" => M_PACKAGE_JSON,
            b"pyvenv.cfg" => M_PYVENV_CFG,
            b"Package.swift" => M_PACKAGE_SWIFT,
            b"build.gradle" | b"build.gradle.kts" | b"settings.gradle" | b"settings.gradle.kts" => M_GRADLE,
            b".git" => M_GIT,
            b"pyproject.toml" | b"setup.py" | b"requirements.txt" => M_PYPROJECT,
            b"pom.xml" => M_POM_XML,
            b"go.mod" => M_GO_MOD,
            b"pubspec.yaml" => M_PUBSPEC,
            b"CMakeLists.txt" => M_CMAKE,
            b"Makefile" | b"GNUmakefile" | b"makefile" => M_MAKEFILE,
            _ if name.ends_with(b".xcodeproj") => M_XCODEPROJ,
            _ => 0,
        }
    }
}

/// One directory. 64 bytes; plain-old-data so snapshots are a memcpy.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct DirNode {
    pub parent: u32,
    pub name_off: u32,
    pub name_len: u16,
    pub flags: u16,
    /// Subdirectories in this subtree (excluding self).
    pub dirs: u32,
    /// Files (non-directories) in this subtree.
    pub files: u64,
    pub logical: u64,
    pub alloc: u64,
    pub private: u64,
    /// Allocated bytes of files directly inside this directory.
    pub own_alloc: u64,
    /// Newest file mtime in this subtree (seconds).
    pub mtime_max: i64,
}

const _: () = assert!(std::mem::size_of::<DirNode>() == 64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SizeKind {
    Logical,
    #[default]
    Alloc,
    Private,
}

impl std::str::FromStr for SizeKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "logical" | "apparent" => Ok(SizeKind::Logical),
            "alloc" | "allocated" | "disk" => Ok(SizeKind::Alloc),
            "private" | "unshared" => Ok(SizeKind::Private),
            _ => Err(format!("unknown size kind `{s}` (logical|alloc|private)")),
        }
    }
}

impl DirNode {
    #[inline]
    pub fn size(&self, k: SizeKind) -> u64 {
        match k {
            SizeKind::Logical => self.logical,
            SizeKind::Alloc => self.alloc,
            SizeKind::Private => self.private,
        }
    }
}

/// A large file retained individually.
#[derive(Clone, Debug)]
pub struct FileHit {
    pub dir: u32,
    pub name: Box<[u8]>,
    pub logical: u64,
    pub alloc: u64,
    pub private: u64,
    pub mtime: i64,
    pub flags: u32,
}

impl FileHit {
    #[inline]
    pub fn size(&self, k: SizeKind) -> u64 {
        match k {
            SizeKind::Logical => self.logical,
            SizeKind::Alloc => self.alloc,
            SizeKind::Private => self.private,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ScanStats {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub others: u64,
    pub errors: u64,
    pub hardlinks_deduped: u64,
    pub mounts_skipped: u64,
    pub elapsed_ms: u64,
    pub threads: usize,
    pub backend: &'static str,
    pub private_measured: bool,
    pub error_samples: Vec<String>,
}

pub struct Index {
    pub root: PathBuf,
    pub nodes: Vec<DirNode>,
    pub names: Vec<u8>,
    /// Largest files, sorted descending by `top_by`.
    pub top_files: Vec<FileHit>,
    pub top_by: SizeKind,
    pub stats: ScanStats,
    children: std::sync::OnceLock<(Vec<u32>, Vec<u32>)>,
}

/// One row of a rendered size tree.
#[derive(Clone, Debug, Serialize)]
pub struct TreeRow {
    pub depth: usize,
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub files: u64,
    pub fraction: f64,
}

impl Index {
    pub fn new(root: PathBuf, nodes: Vec<DirNode>, names: Vec<u8>, top_files: Vec<FileHit>, top_by: SizeKind, stats: ScanStats) -> Self {
        Index { root, nodes, names, top_files, top_by, stats, children: std::sync::OnceLock::new() }
    }

    #[inline]
    pub fn root_node(&self) -> &DirNode {
        &self.nodes[0]
    }

    #[inline]
    pub fn name(&self, id: u32) -> &[u8] {
        let n = &self.nodes[id as usize];
        &self.names[n.name_off as usize..n.name_off as usize + n.name_len as usize]
    }

    /// Absolute path of directory `id`.
    pub fn path(&self, id: u32) -> PathBuf {
        if id == 0 {
            return self.root.clone();
        }
        let mut chain = Vec::new();
        let mut cur = id;
        while cur != 0 && cur != NO_PARENT {
            chain.push(cur);
            cur = self.nodes[cur as usize].parent;
        }
        let mut bytes = path_bytes(&self.root).to_vec();
        for &c in chain.iter().rev() {
            if !bytes.ends_with(b"/") {
                bytes.push(b'/');
            }
            bytes.extend_from_slice(self.name(c));
        }
        bytes_to_path(&bytes)
    }

    pub fn file_path(&self, hit: &FileHit) -> PathBuf {
        self.path(hit.dir).join(bytes_to_path(&hit.name))
    }

    /// Depth of `id` below the root (root = 0).
    pub fn depth(&self, mut id: u32) -> usize {
        let mut d = 0;
        while id != 0 && id != NO_PARENT {
            id = self.nodes[id as usize].parent;
            d += 1;
        }
        d
    }

    fn csr(&self) -> &(Vec<u32>, Vec<u32>) {
        self.children.get_or_init(|| {
            let n = self.nodes.len();
            let mut start = vec![0u32; n + 1];
            for node in &self.nodes[1..] {
                start[node.parent as usize + 1] += 1;
            }
            for i in 0..n {
                start[i + 1] += start[i];
            }
            let mut fill = start.clone();
            let mut ids = vec![0u32; n.saturating_sub(1)];
            for (i, node) in self.nodes.iter().enumerate().skip(1) {
                let p = node.parent as usize;
                ids[fill[p] as usize] = i as u32;
                fill[p] += 1;
            }
            (start, ids)
        })
    }

    /// Immediate child directories of `id`.
    pub fn children(&self, id: u32) -> &[u32] {
        let (start, ids) = self.csr();
        &ids[start[id as usize] as usize..start[id as usize + 1] as usize]
    }

    /// All directories in the subtree rooted at `id` (including `id`), depth-first.
    pub fn subtree(&self, id: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            out.push(n);
            stack.extend_from_slice(self.children(n));
        }
        out
    }

    /// Find the node for an absolute path inside the index.
    pub fn lookup(&self, p: &Path) -> Option<u32> {
        let rel = p.strip_prefix(&self.root).ok()?;
        let mut cur = 0u32;
        for comp in rel.components() {
            let want = path_bytes(Path::new(comp.as_os_str()));
            cur = *self.children(cur).iter().find(|&&c| self.name(c) == want)?;
        }
        Some(cur)
    }

    /// dust-style tree: the `n` largest entries (directories and retained
    /// files) below `from`, plus every ancestor needed to place them,
    /// children sorted by size.
    pub fn tree(&self, from: u32, n: usize, k: SizeKind, max_depth: Option<usize>) -> Vec<TreeRow> {
        let base_depth = self.depth(from);
        let total = self.nodes[from as usize].size(k).max(1);
        let in_subtree = |mut id: u32| -> bool {
            loop {
                if id == from {
                    return true;
                }
                if id == 0 || id == NO_PARENT {
                    return false;
                }
                id = self.nodes[id as usize].parent;
            }
        };
        // Candidates: (size, Either<dir id, file index>)
        let mut cands: Vec<(u64, bool, u32)> = Vec::new();
        for (i, node) in self.nodes.iter().enumerate() {
            let i = i as u32;
            if i == from {
                continue;
            }
            let sz = node.size(k);
            if sz == 0 {
                continue;
            }
            cands.push((sz, true, i));
        }
        for (fi, f) in self.top_files.iter().enumerate() {
            cands.push((f.size(k), false, fi as u32));
        }
        let take = n.min(cands.len());
        if take > 0 && take < cands.len() {
            // Over-select, then filter to the subtree (cheap: subtree check is O(depth)).
            let over = (take * 4).min(cands.len());
            cands.select_nth_unstable_by(over - 1, |a, b| b.0.cmp(&a.0));
            cands.truncate(over);
        }
        cands.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        let mut chosen_dirs = rustc_hash::FxHashSet::default();
        let mut chosen_files: rustc_hash::FxHashMap<u32, Vec<u32>> = Default::default();
        let mut count = 0;
        for &(_, is_dir, i) in &cands {
            if count >= n {
                break;
            }
            let dir = if is_dir { i } else { self.top_files[i as usize].dir };
            if !in_subtree(dir) {
                continue;
            }
            if let Some(md) = max_depth {
                let d = self.depth(dir) - base_depth + usize::from(!is_dir);
                if d > md {
                    continue;
                }
            }
            if is_dir {
                if !chosen_dirs.insert(i) {
                    continue;
                }
            } else {
                chosen_files.entry(dir).or_default().push(i);
            }
            count += 1;
            // ancestors
            let mut a = dir;
            while a != from && a != NO_PARENT {
                chosen_dirs.insert(a);
                a = self.nodes[a as usize].parent;
            }
        }
        let mut rows = Vec::new();
        self.render(from, 0, k, total, &chosen_dirs, &chosen_files, &mut rows);
        rows
    }

    #[allow(clippy::too_many_arguments)]
    fn render(
        &self,
        id: u32,
        depth: usize,
        k: SizeKind,
        total: u64,
        dirs: &rustc_hash::FxHashSet<u32>,
        files: &rustc_hash::FxHashMap<u32, Vec<u32>>,
        out: &mut Vec<TreeRow>,
    ) {
        let node = &self.nodes[id as usize];
        let name = if id == 0 || depth == 0 {
            crate::path::display_path(&self.path(id))
        } else {
            String::from_utf8_lossy(self.name(id)).into_owned()
        };
        out.push(TreeRow {
            depth,
            path: self.path(id),
            name,
            is_dir: true,
            size: node.size(k),
            files: node.files,
            fraction: node.size(k) as f64 / total as f64,
        });
        // Merge child dirs and files, sorted by size desc.
        let mut kids: Vec<(u64, bool, u32)> =
            self.children(id).iter().filter(|c| dirs.contains(c)).map(|&c| (self.nodes[c as usize].size(k), true, c)).collect();
        if let Some(fs) = files.get(&id) {
            kids.extend(fs.iter().map(|&fi| (self.top_files[fi as usize].size(k), false, fi)));
        }
        kids.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        for (sz, is_dir, i) in kids {
            if is_dir {
                self.render(i, depth + 1, k, total, dirs, files, out);
            } else {
                let f = &self.top_files[i as usize];
                out.push(TreeRow {
                    depth: depth + 1,
                    path: self.file_path(f),
                    name: String::from_utf8_lossy(&f.name).into_owned(),
                    is_dir: false,
                    size: sz,
                    files: 1,
                    fraction: sz as f64 / total as f64,
                });
            }
        }
    }

    /// Directories ranked by bytes of files *directly* inside them.
    pub fn top_dirs_own(&self, n: usize) -> Vec<(u32, u64)> {
        let mut v: Vec<(u32, u64)> = self.nodes.iter().enumerate().map(|(i, d)| (i as u32, d.own_alloc)).filter(|x| x.1 > 0).collect();
        if n < v.len() {
            v.select_nth_unstable_by(n, |a, b| b.1.cmp(&a.1));
            v.truncate(n);
        }
        v.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        v
    }

    /// Directories with no file modified since `cutoff` (subtree), largest first.
    pub fn stale_dirs(&self, cutoff: i64, min_size: u64, k: SizeKind) -> Vec<u32> {
        let mut v: Vec<u32> = (1..self.nodes.len() as u32)
            .filter(|&i| {
                let n = &self.nodes[i as usize];
                n.mtime_max != 0 && n.mtime_max < cutoff && n.size(k) >= min_size && {
                    let p = &self.nodes[n.parent as usize];
                    // only the outermost stale directory
                    !(n.parent != 0 && p.mtime_max < cutoff && p.mtime_max != 0)
                }
            })
            .collect();
        v.sort_unstable_by_key(|&i| std::cmp::Reverse(self.nodes[i as usize].size(k)));
        v
    }

    /// Approximate heap bytes held by the index.
    pub fn heap_bytes(&self) -> usize {
        self.nodes.capacity() * std::mem::size_of::<DirNode>()
            + self.names.capacity()
            + self.top_files.iter().map(|f| f.name.len() + std::mem::size_of::<FileHit>()).sum::<usize>()
    }
}
