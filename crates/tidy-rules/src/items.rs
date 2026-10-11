//! Candidate items for policy rules, their filters, and the evidence text a
//! judge sees. By default the judge sees metadata only (name, location,
//! size, age, provenance) — never file content unless a rule opts in.

use crate::policy::{Filter, Rule};
use std::path::{Path, PathBuf};
use tidy_scan::entry::{Kind, ReadOptions, flags as eflags};
use tidy_scan::fmt::{self, parse_bytes, parse_duration};
use tidy_scan::path::{display_path, expand_tilde, join_cbytes, path_to_cbytes};
use tidy_scan::platform::Reader;

#[derive(Clone, Debug)]
pub struct Item {
    pub path: PathBuf,
    pub is_dir: bool,
    pub logical: u64,
    pub alloc: u64,
    pub mtime: i64,
    pub flags: u32,
    /// Depth below the scope root (1 = direct child).
    pub depth: usize,
    /// Directory summary: (files, newest mtime, top extensions).
    pub dir: Option<DirSummary>,
}

#[derive(Clone, Debug, Default)]
pub struct DirSummary {
    pub files: u64,
    pub dirs: u64,
    pub newest: i64,
    pub exts: Vec<(String, u64)>,
}

impl Item {
    pub fn ext(&self) -> String {
        self.path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
    }
    pub fn name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
    pub fn kind(&self) -> &'static str {
        if self.is_dir {
            return "dir";
        }
        kind_of_ext(&self.ext())
    }
}

pub fn kind_of_ext(ext: &str) -> &'static str {
    match ext {
        "jpg" | "jpeg" | "png" | "heic" | "heif" | "gif" | "webp" | "tif" | "tiff" | "bmp" | "avif" | "raw" | "cr2" | "cr3" | "nef"
        | "arw" | "dng" | "raf" | "orf" | "rw2" | "svg" => "image",
        "mp4" | "mov" | "m4v" | "avi" | "mkv" | "webm" | "wmv" | "mpg" | "mpeg" | "3gp" => "video",
        "mp3" | "m4a" | "wav" | "flac" | "aac" | "ogg" | "aiff" | "aif" | "opus" | "wma" => "audio",
        "pdf" | "doc" | "docx" | "pages" | "txt" | "md" | "rtf" | "odt" | "xls" | "xlsx" | "numbers" | "csv" | "ppt" | "pptx" | "key"
        | "epub" | "odp" | "ods" | "tex" => "document",
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "zst" | "lz4" => "archive",
        "dmg" | "pkg" | "mpkg" | "iso" | "msi" | "exe" | "deb" | "rpm" | "appimage" | "apk" | "ipa" | "xip" => "installer",
        "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "java" | "c" | "cc" | "cpp" | "h" | "hpp" | "swift" | "kt" | "rb" | "php"
        | "sh" | "cs" | "scala" | "lua" => "code",
        _ => "file",
    }
}

fn kind_name(ext: &str) -> &'static str {
    match ext {
        "dmg" => "disk image",
        "pkg" | "mpkg" => "installer package",
        "iso" => "optical disc image",
        "zip" | "tar" | "gz" | "tgz" | "7z" | "rar" | "xz" | "bz2" | "zst" => "archive",
        "pdf" => "PDF document",
        "heic" | "jpg" | "jpeg" | "png" => "photo/image",
        "mov" | "mp4" => "video",
        _ => "",
    }
}

#[derive(Clone, Debug, Default)]
pub struct CompiledFilter {
    pub kind: Option<String>,
    pub ext: Vec<String>,
    /// Lower-cased globs; any may match.
    pub name: Vec<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub older_than: Option<i64>,
    pub newer_than: Option<i64>,
    pub depth: Option<usize>,
}

impl CompiledFilter {
    pub fn from(f: &Filter) -> Self {
        CompiledFilter {
            kind: f.kind.clone(),
            ext: f.ext.iter().map(|e| e.trim_start_matches('.').to_lowercase()).collect(),
            name: f.name.iter().map(|g| g.to_lowercase()).collect(),
            min_size: f.min_size.as_deref().and_then(parse_bytes),
            max_size: f.max_size.as_deref().and_then(parse_bytes),
            older_than: f.older_than.as_deref().and_then(parse_duration),
            newer_than: f.newer_than.as_deref().and_then(parse_duration),
            depth: f.depth,
        }
    }

    pub fn wants_dirs(&self) -> bool {
        matches!(self.kind.as_deref(), Some("dir" | "any"))
    }

    /// Name/extension/kind checks only (no size or age) — for items known
    /// only by path, such as labels.
    pub fn matches_name(&self, it: &Item) -> bool {
        match self.kind.as_deref() {
            Some("dir") if !it.is_dir => return false,
            Some("file") if it.is_dir => return false,
            Some("any") | Some("dir") | Some("file") => {}
            Some(k) if it.kind() != k => return false,
            None if it.is_dir => return false,
            _ => {}
        }
        if !self.ext.is_empty() && !self.ext.contains(&it.ext()) {
            return false;
        }
        if !self.name.is_empty() {
            let n = it.name().to_lowercase();
            if !self.name.iter().any(|g| glob_match(g, &n)) {
                return false;
            }
        }
        true
    }

    pub fn matches(&self, it: &Item, now: i64) -> bool {
        if !self.matches_name(it) {
            return false;
        }
        if self.min_size.is_some_and(|m| it.logical < m) || self.max_size.is_some_and(|m| it.logical > m) {
            return false;
        }
        let newest = it.dir.as_ref().map(|d| d.newest.max(it.mtime)).unwrap_or(it.mtime);
        if self.older_than.is_some_and(|d| now - newest < d) || self.newer_than.is_some_and(|d| now - newest > d) {
            return false;
        }
        if self.depth.is_some_and(|d| it.depth > d) {
            return false;
        }
        true
    }
}

/// `*` and `?` glob.
pub fn glob_match(pat: &str, s: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Enumerate files (and, if wanted, directories) under `root` down to
/// `max_depth`. Hidden entries (dotfiles) and packages' insides are skipped.
pub fn list(root: &Path, max_depth: usize, want_dirs: bool, out: &mut Vec<Item>) {
    let mut reader = Reader::new(ReadOptions::default());
    let mut stack: Vec<(Box<[u8]>, usize)> = vec![(path_to_cbytes(root).into_boxed_slice(), 1)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(c) = std::ffi::CStr::from_bytes_with_nul(&dir) else { continue };
        let mut subdirs = Vec::new();
        let _ = reader.read(c, &mut |e| {
            if e.name.first() == Some(&b'.') {
                return;
            }
            let path_c = join_cbytes(&dir, e.name);
            let path = tidy_scan::path::bytes_to_path(&path_c[..path_c.len() - 1]);
            match e.kind {
                Kind::Dir => {
                    let is_pkg = path.extension().is_some_and(|x| {
                        let x = x.to_string_lossy().to_lowercase();
                        matches!(
                            x.as_str(),
                            "app"
                                | "photoslibrary"
                                | "bundle"
                                | "framework"
                                | "pages"
                                | "numbers"
                                | "key"
                                | "rtfd"
                                | "xcodeproj"
                                | "sparsebundle"
                                | "musiclibrary"
                                | "fcpbundle"
                                | "logicx"
                        )
                    });
                    if want_dirs || is_pkg {
                        out.push(Item {
                            path: path.clone(),
                            is_dir: true,
                            logical: 0,
                            alloc: 0,
                            mtime: e.mtime,
                            flags: e.flags,
                            depth,
                            dir: None,
                        });
                    }
                    let artifact = matches!(
                        e.name,
                        b"node_modules" | b"__pycache__" | b"venv" | b"site-packages" | b"DerivedData" | b"Pods" | b"bower_components"
                    );
                    if !is_pkg && !artifact && depth < max_depth {
                        subdirs.push((path_c, depth + 1));
                    }
                }
                Kind::File => out.push(Item {
                    path,
                    is_dir: false,
                    logical: e.logical,
                    alloc: e.alloc,
                    mtime: e.mtime,
                    flags: e.flags,
                    depth,
                    dir: None,
                }),
                _ => {}
            }
        });
        stack.extend(subdirs);
    }
}

/// Fill size/summary for directory items (one parallel scan each).
pub fn summarize_dir(it: &mut Item) {
    let opts = tidy_scan::ScanOptions { threads: 4, top_k: 200, ..Default::default() };
    if let Ok(idx) = tidy_scan::scan(&it.path, &opts) {
        let r = idx.root_node();
        it.logical = r.logical;
        it.alloc = r.alloc;
        let mut exts: std::collections::HashMap<String, u64> = Default::default();
        for f in &idx.top_files {
            let name = String::from_utf8_lossy(&f.name);
            let e = Path::new(name.as_ref()).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_else(|| "(none)".into());
            *exts.entry(e).or_default() += 1;
        }
        let mut v: Vec<(String, u64)> = exts.into_iter().collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.1));
        v.truncate(5);
        it.dir = Some(DirSummary { files: r.files, dirs: r.dirs as u64, newest: r.mtime_max, exts: v });
    }
}

/// The ITEM block the judge reads.
pub fn evidence(it: &Item, now: i64, redact_home: bool) -> String {
    evidence_with(it, now, redact_home, None)
}

/// Like [`evidence`], with provenance supplied (synthetic test items).
pub fn evidence_with(it: &Item, now: i64, redact_home: bool, prov: Option<tidy_ops::meta::Provenance>) -> String {
    let shown = if redact_home { display_path(&it.path) } else { it.path.display().to_string() };
    let mut s = String::from("ITEM\n");
    s.push_str(&format!("name: {}\n", it.name()));
    s.push_str(&format!("path: {shown}\n"));
    if let Some(parent) = it.path.parent() {
        s.push_str(&format!("folder: {}\n", if redact_home { display_path(parent) } else { parent.display().to_string() }));
    }
    if it.is_dir {
        s.push_str("kind: folder\n");
    } else {
        let ext = it.ext();
        let kn = kind_name(&ext);
        let k = it.kind();
        s.push_str(&format!(
            "kind: file ({}{}{})\n",
            if ext.is_empty() { "no extension".to_string() } else { format!(".{ext}") },
            if kn.is_empty() { "" } else { ", " },
            if kn.is_empty() { k } else { kn }
        ));
    }
    s.push_str(&format!("size: {}\n", fmt::bytes(it.logical)));
    let newest = it.dir.as_ref().map(|d| d.newest.max(it.mtime)).unwrap_or(it.mtime);
    s.push_str(&format!("last modified: {} ago ({})\n", fmt::age(now - newest), ymd(newest)));
    if let Some(d) = &it.dir {
        s.push_str(&format!("contains: {} files in {} subfolders\n", d.files, d.dirs));
        if !d.exts.is_empty() {
            let ex: Vec<String> = d.exts.iter().map(|(e, n)| format!(".{e}×{n}")).collect();
            s.push_str(&format!("largest files are: {}\n", ex.join(", ")));
        }
    }
    let prov = prov.unwrap_or_else(|| tidy_ops::meta::provenance(&it.path));
    if !prov.where_from.is_empty() {
        s.push_str(&format!("downloaded from: {}\n", prov.where_from.join(" ← ")));
    } else if let Some(a) = &prov.downloaded_by {
        s.push_str(&format!("downloaded from the internet (via {a})\n"));
    }
    match prov.last_used {
        Some(t) => s.push_str(&format!("last opened: {} ago\n", fmt::age(now - t))),
        None if cfg!(target_os = "macos") && !it.is_dir => s.push_str("last opened: no record\n"),
        None => {}
    }
    if it.flags & eflags::DATALESS != 0 {
        s.push_str("stored in the cloud (not downloaded locally)\n");
    }
    s
}

/// Optional content excerpt for text-like files (rules with `content = true`).
pub fn text_excerpt(p: &Path, max: usize) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(p).ok()?;
    let mut buf = vec![0u8; max];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    if buf.iter().filter(|&&b| b == 0).count() > 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

pub fn ymd(t: i64) -> String {
    // civil-from-days (Howard Hinnant)
    let z = t.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

/// Scope roots of a rule (default: the plan roots).
pub fn scopes(rule: &Rule, roots: &[PathBuf]) -> Vec<PathBuf> {
    if rule.scope.is_empty() {
        roots.to_vec()
    } else {
        rule.scope.iter().map(|s| expand_tilde(s)).filter_map(|p| std::fs::canonicalize(p).ok()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn globs_and_dates() {
        assert!(glob_match("*.dmg", "figma-1.2.dmg"));
        assert!(glob_match("img_????.jpg", "img_1234.jpg"));
        assert!(!glob_match("*.dmg", "notes.txt"));
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(1_791_748_084), "2026-10-11");
    }
}
