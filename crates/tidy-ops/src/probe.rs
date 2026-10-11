//! System probes used by guards: open files, git state, cloud storage.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Paths currently open (or used as a working directory) by processes the
/// user can inspect. Sorted, for prefix queries.
#[derive(Default, Debug)]
pub struct OpenFiles {
    paths: Vec<PathBuf>,
    pub available: bool,
}

impl OpenFiles {
    pub fn collect() -> Self {
        let mut paths = Vec::new();
        let available;
        #[cfg(target_os = "linux")]
        {
            available = collect_proc(&mut paths);
        }
        #[cfg(not(target_os = "linux"))]
        {
            available = collect_lsof(&mut paths);
        }
        paths.sort();
        paths.dedup();
        OpenFiles { paths, available }
    }

    /// First open path at or below `p`.
    pub fn any_under(&self, p: &Path) -> Option<&Path> {
        let i = self.paths.partition_point(|x| x.as_path() < p);
        self.paths[i..].iter().take_while(|x| x.starts_with(p)).map(|x| x.as_path()).next()
    }
}

#[cfg(target_os = "linux")]
fn collect_proc(out: &mut Vec<PathBuf>) -> bool {
    let me = std::process::id();
    let Ok(rd) = std::fs::read_dir("/proc") else { return false };
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        if pid == me {
            continue;
        }
        let base = e.path();
        if let Ok(cwd) = std::fs::read_link(base.join("cwd")) {
            out.push(cwd);
        }
        if let Ok(fds) = std::fs::read_dir(base.join("fd")) {
            for fd in fds.flatten() {
                if let Ok(t) = std::fs::read_link(fd.path())
                    && t.is_absolute()
                {
                    out.push(t);
                }
            }
        }
    }
    true
}

#[cfg(not(target_os = "linux"))]
fn collect_lsof(out: &mut Vec<PathBuf>) -> bool {
    let uid = unsafe { libc::getuid() };
    let Ok(o) = Command::new("lsof").args(["-w", "-n", "-P", "-F", "n", "-u"]).arg(uid.to_string()).output() else {
        return false;
    };
    for line in o.stdout.split(|&b| b == b'\n') {
        if let Some(rest) = line.strip_prefix(b"n/") {
            let mut v = b"/".to_vec();
            v.extend_from_slice(rest);
            out.push(tidy_scan::path::bytes_to_path(&v));
        }
    }
    true
}

/// Git facts about a repository work tree.
#[derive(Debug, Clone, Default)]
pub struct GitState {
    pub root: PathBuf,
    pub dirty: bool,
    pub ahead: u64,
    pub has_upstream: bool,
    pub stashes: bool,
}

impl GitState {
    /// Work that exists only in this clone.
    pub fn has_local_only_work(&self) -> bool {
        self.dirty || self.ahead > 0 || !self.has_upstream || self.stashes
    }
    pub fn describe(&self) -> String {
        let mut v = Vec::new();
        if self.dirty {
            v.push("uncommitted changes".to_string());
        }
        if self.ahead > 0 {
            v.push(format!("{} unpushed commit(s)", self.ahead));
        }
        if !self.has_upstream {
            v.push("no upstream branch".to_string());
        }
        if self.stashes {
            v.push("stashed changes".to_string());
        }
        v.join(", ")
    }
}

/// Nearest ancestor (or self) that is a git work tree root.
pub fn git_root(p: &Path) -> Option<PathBuf> {
    let mut cur = Some(p);
    while let Some(c) = cur {
        if c.join(".git").exists() {
            return Some(c.to_path_buf());
        }
        cur = c.parent();
    }
    None
}

pub fn git_state(root: &Path) -> Option<GitState> {
    let o =
        Command::new("git").arg("-C").arg(root).args(["status", "--porcelain=v2", "--branch", "--untracked-files=normal"]).output().ok()?;
    if !o.status.success() {
        return None;
    }
    let mut st = GitState { root: root.to_path_buf(), ..Default::default() };
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        if let Some(ab) = line.strip_prefix("# branch.ab ") {
            st.has_upstream = true;
            if let Some(a) = ab.split_whitespace().next().and_then(|s| s.strip_prefix('+')) {
                st.ahead = a.parse().unwrap_or(0);
            }
        } else if line.starts_with("# branch.upstream") {
            st.has_upstream = true;
        } else if !line.starts_with('#') && !line.is_empty() {
            st.dirty = true;
        }
    }
    if let Ok(s) = Command::new("git").arg("-C").arg(root).args(["stash", "list"]).output() {
        st.stashes = !s.stdout.is_empty();
    }
    Some(st)
}

/// Whether `p` is ignored by the repository at `root` (`git check-ignore`).
pub fn git_ignored(root: &Path, p: &Path) -> bool {
    Command::new("git").arg("-C").arg(root).args(["check-ignore", "-q", "--"]).arg(p).status().map(|s| s.success()).unwrap_or(false)
}

/// Directories synced by cloud providers, where deleting locally deletes remotely.
pub fn cloud_roots() -> Vec<PathBuf> {
    let home = tidy_scan::path::home_dir();
    let mut v = vec![
        home.join("Library/Mobile Documents"),
        home.join("Library/CloudStorage"),
        home.join("Dropbox"),
        home.join("OneDrive"),
        home.join("Google Drive"),
        home.join("pCloud Drive"),
    ];
    // iCloud "Desktop & Documents" sync makes these cloud-backed too, but only
    // when enabled; we cannot detect that cheaply, so they are not listed here.
    v.retain(|p| p.exists());
    v
}

/// Mount points strictly below `p` (from the kernel's mount table).
pub fn mounts_under(p: &Path) -> Vec<PathBuf> {
    let Ok(base) = std::fs::canonicalize(p) else { return vec![] };
    mount_points().into_iter().filter(|m| m != &base && m.starts_with(&base)).collect()
}

/// All current mount points.
pub fn mount_points() -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        // /proc/self/mounts: "dev mountpoint fstype opts 0 0", octal-escaped.
        let Ok(text) = std::fs::read_to_string("/proc/self/mounts") else { return vec![] };
        return text
            .lines()
            .filter_map(|l| l.split(' ').nth(1))
            .map(|m| {
                let mut out = Vec::new();
                let b = m.as_bytes();
                let mut i = 0;
                while i < b.len() {
                    if b[i] == b'\\' && i + 3 < b.len() {
                        if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 4]).unwrap_or("x"), 8) {
                            out.push(v);
                            i += 4;
                            continue;
                        }
                    }
                    out.push(b[i]);
                    i += 1;
                }
                tidy_scan::path::bytes_to_path(&out)
            })
            .collect();
    }
    #[cfg(target_os = "macos")]
    {
        let mut buf: *mut libc::statfs = std::ptr::null_mut();
        let n = unsafe { libc::getmntinfo(&mut buf, libc::MNT_NOWAIT) };
        if n <= 0 || buf.is_null() {
            return vec![];
        }
        let s = unsafe { std::slice::from_raw_parts(buf, n as usize) };
        return s
            .iter()
            .map(|f| {
                let c = unsafe { std::ffi::CStr::from_ptr(f.f_mntonname.as_ptr()) };
                tidy_scan::path::bytes_to_path(c.to_bytes())
            })
            .collect();
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        vec![]
    }
}
