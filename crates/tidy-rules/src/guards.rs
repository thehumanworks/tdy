//! L0 guards: absolute, code-level blocks. Nothing a detector, a model or a
//! policy says can override them (only explicit, narrow config knobs).

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tidy_ops::probe::{self, GitState, OpenFiles};
use tidy_plan::Action;
use tidy_scan::entry::flags as eflags;
use tidy_scan::path::home_dir;

#[derive(Clone, Debug)]
pub struct GuardConfig {
    /// Removal-type actions (delete, trash, quarantine, archive, dedupe)
    /// require the newest file under the path to be at least this old
    /// (seconds). Rules may ask for more, never less.
    pub min_age: i64,
    /// Organising actions (move, rename, copy) only wait for the item to
    /// settle (finished downloading/writing).
    pub settle: i64,
    /// Allow destructive actions inside cloud-synced folders.
    pub allow_cloud: bool,
    /// Extra user-protected paths (prefix match).
    pub protect: Vec<PathBuf>,
    /// Paths that are destinations of move rules (never sources).
    pub destinations: Vec<PathBuf>,
}

impl Default for GuardConfig {
    fn default() -> Self {
        GuardConfig { min_age: 7 * 86_400, settle: 120, allow_cloud: false, protect: Vec::new(), destinations: Vec::new() }
    }
}

/// Facts about the candidate supplied by the caller (from the index).
#[derive(Clone, Copy, Debug, Default)]
pub struct Facts {
    /// Newest mtime in the subtree (files) or the file's mtime.
    pub mtime_max: Option<i64>,
    /// Entry flags (dataless, immutable…), files only.
    pub flags: u32,
    /// Output of a deterministic detector for a regenerable artifact.
    pub regenerable: bool,
    /// Rule-requested minimum age (raises, never lowers, `min_age`).
    pub min_age: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub guard: &'static str,
    pub reason: String,
}

impl std::fmt::Display for Block {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.guard, self.reason)
    }
}

const SYSTEM_PREFIXES: &[&str] = &[
    "/System",
    "/usr",
    "/bin",
    "/sbin",
    "/etc",
    "/private/etc",
    "/private/var/db",
    "/Library",
    "/Applications",
    "/opt",
    "/dev",
    "/proc",
    "/sys",
    "/boot",
    "/lib",
    "/lib64",
    "/nix/store",
    "/snap",
    "/cores",
    "/Volumes/Recovery",
    "/var",
    "/run",
    "/srv",
];

/// Home-relative paths holding credentials or identity; never touched.
const SECRET_PATHS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".config/gh",
    ".config/gcloud",
    ".password-store",
    ".secrets",
    ".netrc",
    ".pki",
    "Library/Keychains",
    ".config/op",
    ".config/fnox",
    ".age",
    ".config/sops",
    ".vault-token",
    ".npmrc",
    ".pypirc",
    ".cargo/credentials.toml",
    ".cargo/credentials",
    ".git-credentials",
    ".config/age",
];

/// Home-relative app data: destructive actions only for regenerable caches.
const APP_DATA: &[&str] = &[
    "Library/Mail",
    "Library/Messages",
    "Library/Containers",
    "Library/Group Containers",
    "Library/Application Support",
    "Library/Safari",
    "Library/Calendars",
    "Library/Contacts",
    "Library/Notes",
    "Library/Photos",
    "Library/Preferences",
    "Library/Accounts",
    "Library/Cookies",
    "Library/HomeKit",
    ".local/share",
    ".config",
    ".mozilla",
    ".thunderbird",
];

/// Standard top-level folders that are never themselves removed or moved.
const STANDARD_DIRS: &[&str] = &[
    "Desktop",
    "Documents",
    "Downloads",
    "Pictures",
    "Movies",
    "Music",
    "Library",
    "Applications",
    "Public",
    "Videos",
    "Templates",
    "projects",
    "dev",
    "src",
    "code",
    "go",
    ".cache",
    ".local",
    ".config",
    ".cargo",
    ".rustup",
];

/// Directories that macOS and apps treat as single documents/bundles.
const PACKAGE_EXTS: &[&str] = &[
    "app",
    "bundle",
    "framework",
    "plugin",
    "kext",
    "xpc",
    "appex",
    "photoslibrary",
    "musiclibrary",
    "tvlibrary",
    "imovielibrary",
    "fcpbundle",
    "logicx",
    "band",
    "pages",
    "numbers",
    "key",
    "rtfd",
    "xcodeproj",
    "xcworkspace",
    "playground",
    "sparsebundle",
    "photolibrary",
    "aplibrary",
    "lrdata",
    "lrlibrary",
    "dtBase2",
    "scriv",
    "nib",
    "storyboardc",
    "pkg",
    "mpkg",
    "qlgenerator",
    "prefPane",
    "saver",
    "wdgt",
    "dSYM",
];

/// Whole-library packages: destructive actions on them are always blocked.
const LIBRARY_EXTS: &[&str] =
    &["photoslibrary", "musiclibrary", "tvlibrary", "imovielibrary", "photolibrary", "aplibrary", "lrlibrary", "fcpbundle", "logicx"];

pub struct Guards {
    pub cfg: GuardConfig,
    home: PathBuf,
    cloud: Vec<PathBuf>,
    open: OnceLock<OpenFiles>,
    git: Mutex<HashMap<PathBuf, Option<GitState>>>,
}

fn ext_of(p: &Path) -> Option<&str> {
    p.extension().and_then(|e| e.to_str())
}

impl Guards {
    pub fn new(cfg: GuardConfig) -> Self {
        Guards { cfg, home: home_dir(), cloud: probe::cloud_roots(), open: OnceLock::new(), git: Mutex::new(HashMap::new()) }
    }

    fn open_files(&self) -> &OpenFiles {
        self.open.get_or_init(OpenFiles::collect)
    }

    fn git_state(&self, root: &Path) -> Option<GitState> {
        let mut g = self.git.lock();
        g.entry(root.to_path_buf()).or_insert_with(|| probe::git_state(root)).clone()
    }

    /// Check one candidate. `index_subtree_git_roots` lists repositories
    /// inside the candidate (from the index) when known; otherwise a quick
    /// scan finds them.
    pub fn check(&self, path: &Path, action: Action, facts: &Facts, git_roots_inside: Option<&[PathBuf]>) -> Result<(), Block> {
        let block = |guard: &'static str, reason: String| Err(Block { guard, reason });
        let destructive = action.destructive();

        if path == Path::new("/") || path.parent().is_none() {
            return block("root", "filesystem root".into());
        }
        // system
        for pre in SYSTEM_PREFIXES {
            if path.starts_with(pre) && !path.starts_with(&self.home) {
                return block("system", format!("system location {pre}"));
            }
        }
        if path.starts_with("/private/var") && !path.starts_with("/private/var/folders") {
            return block("system", "system location /private/var".into());
        }
        // home itself and standard folders
        if path == self.home {
            return block("root", "home directory".into());
        }
        if let Ok(rel) = path.strip_prefix(&self.home) {
            if destructive && rel.components().count() == 1 && STANDARD_DIRS.iter().any(|d| rel == Path::new(d)) {
                return block("root", format!("standard folder ~/{}", rel.display()));
            }
            for s in SECRET_PATHS {
                if rel.starts_with(s) {
                    return block("secrets", format!("credentials live in ~/{s}"));
                }
            }
            if destructive && !facts.regenerable {
                for a in APP_DATA {
                    if rel.starts_with(a) {
                        return block("app-data", format!("application data in ~/{a}"));
                    }
                }
            }
        }
        // user protection
        for p in &self.cfg.protect {
            if path.starts_with(p) {
                return block("protect", format!("protected by policy ({})", p.display()));
            }
        }
        // move-rule destinations are never sources
        if destructive {
            for d in &self.cfg.destinations {
                if path.starts_with(d) {
                    return block("destination", format!("inside a move destination ({})", d.display()));
                }
            }
        }
        // packages: never act on something inside a bundle; never destroy libraries
        let mut cur = path.parent();
        while let Some(c) = cur {
            if let Some(e) = ext_of(c)
                && PACKAGE_EXTS.iter().any(|x| x.eq_ignore_ascii_case(e))
            {
                return block("package", format!("inside package {}", c.display()));
            }
            cur = c.parent();
        }
        if destructive
            && let Some(e) = ext_of(path)
            && LIBRARY_EXTS.iter().any(|x| x.eq_ignore_ascii_case(e))
        {
            return block("package", "media library".into());
        }
        // cloud
        if !self.cfg.allow_cloud && destructive {
            for c in &self.cloud {
                if path.starts_with(c) {
                    return block("cloud", format!("cloud-synced folder {} (deleting here deletes everywhere)", c.display()));
                }
            }
        }
        if facts.flags & eflags::DATALESS != 0 {
            return block("cloud", "cloud placeholder (content not downloaded)".into());
        }
        if facts.flags & eflags::IMMUTABLE != 0 {
            return block("immutable", "file is locked/immutable".into());
        }
        if !destructive {
            return Ok(());
        }
        // age
        let removal = matches!(action, Action::Delete | Action::Trash | Action::Quarantine | Action::Archive | Action::Dedupe);
        let floor = if removal { self.cfg.min_age } else { self.cfg.settle };
        let min_age = facts.min_age.unwrap_or(0).max(floor);
        if let Some(m) = facts.mtime_max {
            let age = tidy_scan::fmt::now_secs() - m;
            if age < min_age {
                return block("recent", format!("modified {} ago (minimum {})", tidy_scan::fmt::age(age), tidy_scan::fmt::age(min_age)));
            }
        }
        // open files
        if let Some(o) = self.open_files().any_under(path) {
            return block("open", format!("in use: {}", o.display()));
        }
        // git: inside a work tree → must be ignored; containing repos → no local-only work
        if let Some(root) = probe::git_root(path.parent().unwrap_or(path))
            && root != path
            && !path.starts_with(root.join(".git"))
            && !probe::git_ignored(&root, path)
        {
            return block("git", format!("tracked or untracked-but-not-ignored in repo {}", root.display()));
        }
        let found;
        let roots: &[PathBuf] = match git_roots_inside {
            Some(r) => r,
            None => {
                found = find_git_roots(path);
                &found
            }
        };
        for r in roots {
            if let Some(st) = self.git_state(r) {
                if st.has_local_only_work() {
                    return block("git", format!("repo {} has {}", r.display(), st.describe()));
                }
            } else {
                return block("git", format!("cannot read git state of {}", r.display()));
            }
        }
        Ok(())
    }
}

/// Repository roots at or below `p` (directories containing `.git`).
pub fn find_git_roots(p: &Path) -> Vec<PathBuf> {
    if !std::fs::symlink_metadata(p).map(|m| m.is_dir()).unwrap_or(false) {
        return Vec::new();
    }
    let opts = tidy_scan::ScanOptions { threads: 4, top_k: 0, ..Default::default() };
    match tidy_scan::scan(p, &opts) {
        Ok(idx) => git_roots_in_index(&idx, 0),
        Err(_) => Vec::new(),
    }
}

/// Repository roots in the subtree of `node` using scan markers.
pub fn git_roots_in_index(idx: &tidy_scan::Index, node: u32) -> Vec<PathBuf> {
    use tidy_scan::index::node_flags::M_GIT;
    idx.subtree(node).into_iter().filter(|&n| idx.nodes[n as usize].flags & M_GIT != 0).map(|n| idx.path(n)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn old() -> Facts {
        Facts { mtime_max: Some(0), ..Default::default() }
    }

    #[test]
    fn system_and_secrets_blocked() {
        let g = Guards::new(GuardConfig::default());
        assert_eq!(g.check(Path::new("/usr/lib/x"), Action::Delete, &old(), Some(&[])).unwrap_err().guard, "system");
        let home = home_dir();
        assert_eq!(g.check(&home.join(".ssh/id_ed25519"), Action::Move, &old(), Some(&[])).unwrap_err().guard, "secrets");
        assert_eq!(g.check(&home.join("Downloads"), Action::Trash, &old(), Some(&[])).unwrap_err().guard, "root");
        assert_eq!(
            g.check(&home.join("Pictures/Photos Library.photoslibrary/x.jpg"), Action::Tag, &old(), Some(&[])).unwrap_err().guard,
            "package"
        );
        assert_eq!(g.check(&home.join("Pictures/P.photoslibrary"), Action::Trash, &old(), Some(&[])).unwrap_err().guard, "package");
        // app data: blocked unless regenerable
        let p = home.join("Library/Application Support/Foo/Cache");
        assert_eq!(g.check(&p, Action::Delete, &old(), Some(&[])).unwrap_err().guard, "app-data");
    }

    #[test]
    fn recent_and_git() {
        let t = tempfile::tempdir().unwrap();
        let g = Guards::new(GuardConfig::default());
        let f = t.path().join("fresh.bin");
        std::fs::write(&f, b"x").unwrap();
        let now = Facts { mtime_max: Some(tidy_scan::fmt::now_secs()), ..Default::default() };
        assert_eq!(g.check(&f, Action::Trash, &now, Some(&[])).unwrap_err().guard, "recent");
        assert!(g.check(&f, Action::Tag, &now, Some(&[])).is_ok());

        // a repo with uncommitted work inside the candidate
        let repo = t.path().join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        let ok = std::process::Command::new("git").arg("-C").arg(&repo).args(["init", "-q"]).status().map(|s| s.success()).unwrap_or(false);
        if !ok {
            return;
        }
        std::fs::write(repo.join("main.rs"), b"fn main(){}").unwrap();
        std::fs::write(repo.join(".gitignore"), b"target/\n").unwrap();
        std::fs::create_dir_all(repo.join("target")).unwrap();
        let e = g.check(t.path(), Action::Trash, &old(), None).unwrap_err();
        assert_eq!(e.guard, "git", "{e}");
        // ignored build dir inside the repo is fine
        assert!(g.check(&repo.join("target"), Action::Delete, &old(), Some(&[])).is_ok());
        // tracked/untracked source file is not
        assert_eq!(g.check(&repo.join("main.rs"), Action::Delete, &old(), Some(&[])).unwrap_err().guard, "git");
    }
}
