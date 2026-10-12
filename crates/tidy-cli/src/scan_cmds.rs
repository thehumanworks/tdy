//! `tidy scan`, `tidy top`, `tidy diff`.

use crate::ui;
use anyhow::{Context, Result};
use clap::Args;
use std::path::{Path, PathBuf};
use tidy_scan::{Index, ScanOptions, SizeKind, cache, fmt, path, snapshot};

#[derive(Args, Clone)]
pub struct ScanArgs {
    /// Directory to scan.
    #[arg(default_value = ".")]
    pub path: String,
    /// Worker threads (0 = auto).
    #[arg(short = 'j', long, default_value_t = 0)]
    pub threads: usize,
    /// Descend into other filesystems / mount points.
    #[arg(long)]
    pub cross_mounts: bool,
    /// Measure unshared bytes (APFS clones, btrfs/xfs reflinks). Free on APFS; slower on Linux.
    #[arg(long)]
    pub private: bool,
    /// Size to rank by: alloc (on disk), logical (apparent) or private (unshared).
    #[arg(long, default_value = "alloc")]
    pub by: SizeKind,
    /// Emit JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

impl ScanArgs {
    pub fn options(&self, top_k: usize) -> ScanOptions {
        // Unshared bytes come in the same call on APFS, so always take them there.
        let private = self.private || self.by == SizeKind::Private || cfg!(target_os = "macos");
        ScanOptions { threads: self.threads, cross_mounts: self.cross_mounts, private, top_k, top_by: self.by, progress: None }
    }
    pub fn root(&self) -> PathBuf {
        path::expand_tilde(&self.path)
    }
}

#[derive(Args)]
pub struct ScanCmd {
    #[command(flatten)]
    pub(crate) scan: ScanArgs,
    /// Number of entries to show in the tree.
    #[arg(short = 'n', long, default_value_t = 24)]
    lines: usize,
    /// Limit tree depth.
    #[arg(short, long)]
    depth: Option<usize>,
    /// Save a snapshot for `tidy diff` and cached queries.
    #[arg(long)]
    save: bool,
    /// Only print the summary line.
    #[arg(short, long)]
    quiet: bool,
    /// Answer from the last scan of this folder (or of a folder above it)
    /// if there is one, instead of scanning. Says how old it is.
    #[arg(long)]
    cached: bool,
}

#[derive(Args)]
pub struct TopCmd {
    #[command(flatten)]
    scan: ScanArgs,
    #[arg(short = 'n', long, default_value_t = 20)]
    lines: usize,
    /// Rank directories by bytes of files directly inside them.
    #[arg(long)]
    dirs: bool,
    /// Answer from the last scan of this folder (or of a folder above it)
    /// if there is one, instead of scanning.
    #[arg(long)]
    cached: bool,
}

#[derive(Args)]
pub struct DiffCmd {
    #[command(flatten)]
    scan: ScanArgs,
    #[arg(short = 'n', long, default_value_t = 20)]
    lines: usize,
    /// Ignore changes smaller than this (e.g. 10MB).
    #[arg(long, default_value = "1MB")]
    min: String,
}

/// Scan `root` and keep the index in the cache (best effort). At least the
/// 64 largest files are kept, so the cached index can answer `tidy scan`
/// and `tidy top` later.
pub fn scan_and_keep(root: &Path, opts: &ScanOptions) -> Result<Index> {
    let opts = ScanOptions { top_k: opts.top_k.max(CACHED_TOP), ..opts.clone() };
    let idx = tidy_scan::scan(root, &opts).with_context(|| format!("scanning {}", root.display()))?;
    keep(&idx);
    Ok(idx)
}

/// Largest files every cached scan keeps (cheap: a bounded heap per thread).
pub const CACHED_TOP: usize = 64;

/// Keep a full scan's index in the cache; a failure only costs speed later.
pub fn keep(idx: &Index) {
    if let Err(e) = cache::store(idx) {
        eprintln!("{}", ui::dim(&format!("could not cache the index: {e}")));
    }
}

/// The cached index when `cached` and there is one that fits; otherwise a
/// fresh scan (which is then cached).
pub fn index_for(root: &Path, opts: &ScanOptions, cached: bool) -> Result<(Index, Option<PathBuf>)> {
    if cached && let Some(c) = cache::load(root, opts) {
        return Ok((c.index, Some(c.from)));
    }
    Ok((scan_and_keep(root, opts)?, None))
}

/// `from the index of ~/x, 3 min ago — without --cached to rescan`
fn cached_note(idx: &Index, from: &Path) -> String {
    let src = if from == idx.root { String::new() } else { format!(" of {}", path::display_path(from)) };
    format!("from the cached index{src}, scanned {} ago — run without --cached to rescan", fmt::age(cache::age_secs(idx)))
}

pub fn summary(idx: &tidy_scan::Index, by: SizeKind) -> String {
    let s = &idx.stats;
    let entries = s.files + s.dirs;
    let rate = if s.elapsed_ms > 0 { entries as f64 / (s.elapsed_ms as f64 / 1000.0) } else { 0.0 };
    let root = idx.root_node();
    let mut parts = vec![
        format!("{} {}", fmt::bytes(root.size(by)), by_label(by)),
        format!("{} files", ui::thousands(s.files)),
        format!("{} dirs", ui::thousands(s.dirs)),
        if s.backend == "cache" {
            format!("cached {} ago", fmt::age(cache::age_secs(idx)))
        } else {
            format!("{:.2}s ({}/s, {} threads, {})", s.elapsed_ms as f64 / 1000.0, ui::thousands(rate as u64), s.threads, s.backend)
        },
    ];
    if by != SizeKind::Logical {
        parts.insert(1, format!("{} apparent", fmt::bytes(root.logical)));
    }
    if s.private_measured && by != SizeKind::Private {
        parts.insert(1, format!("{} unshared", fmt::bytes(root.private)));
    }
    if s.errors > 0 {
        parts.push(format!("{} unreadable", s.errors));
    }
    if s.mounts_skipped > 0 {
        parts.push(format!("{} mounts skipped", s.mounts_skipped));
    }
    parts.join(" · ")
}

fn by_label(by: SizeKind) -> &'static str {
    match by {
        SizeKind::Alloc => "on disk",
        SizeKind::Logical => "apparent",
        SizeKind::Private => "unshared",
    }
}

pub fn cmd_scan(c: ScanCmd) -> Result<()> {
    let root = c.scan.root();
    let (idx, from) = index_for(&root, &c.scan.options(c.lines.max(64)), c.cached)?;
    if c.save {
        let ts = fmt::now_secs();
        snapshot::save(&idx, &snapshot::snapshot_path(&idx.root, None))?;
        snapshot::save(&idx, &snapshot::snapshot_path(&idx.root, Some(ts)))?;
    }
    let by = c.scan.by;
    if c.scan.json {
        let rows = if c.quiet { vec![] } else { idx.tree(0, c.lines, by, c.depth) };
        let out = serde_json::json!({
            "root": idx.root,
            "by": by,
            "totals": { "logical": idx.root_node().logical, "alloc": idx.root_node().alloc, "private": idx.root_node().private,
                        "files": idx.stats.files, "dirs": idx.stats.dirs },
            "stats": idx.stats,
            "cached": from.as_ref().map(|f| serde_json::json!({"from": f, "age_secs": cache::age_secs(&idx)})),
            "tree": rows,
        });
        println!("{}", serde_json::to_string(&out)?);
        return Ok(());
    }
    println!("{}  {}", ui::bold(&path::display_path(&idx.root)), ui::dim(&summary(&idx, by)));
    if !c.quiet {
        ui::print_tree(&idx.tree(0, c.lines, by, c.depth));
    }
    if let Some(f) = &from {
        eprintln!("{}", ui::dim(&cached_note(&idx, f)));
    }
    for e in idx.stats.error_samples.iter().take(3) {
        eprintln!("{}", ui::dim(&format!("unreadable: {e}")));
    }
    Ok(())
}

pub fn cmd_top(c: TopCmd) -> Result<()> {
    let root = c.scan.root();
    // `--dirs` reads folders only, so any cached largest-files list will do
    let k = if c.dirs { 0 } else { c.lines.max(1) };
    let (idx, from) = index_for(&root, &c.scan.options(k), c.cached)?;
    if let Some(f) = &from
        && !c.scan.json
    {
        eprintln!("{}", ui::dim(&cached_note(&idx, f)));
    }
    let by = c.scan.by;
    let now = fmt::now_secs();
    if c.dirs {
        let rows = idx.top_dirs_own(c.lines);
        if c.scan.json {
            let v: Vec<_> = rows
                .iter()
                .map(|&(id, b)| serde_json::json!({"path": idx.path(id), "own_alloc": b, "files": idx.nodes[id as usize].files}))
                .collect();
            println!("{}", serde_json::to_string(&v)?);
            return Ok(());
        }
        for (id, b) in rows {
            println!("{:>9}  {}", fmt::bytes(b), path::display_path(&idx.path(id)));
        }
        return Ok(());
    }
    if c.scan.json {
        for f in idx.top_files.iter().take(c.lines) {
            println!(
                "{}",
                serde_json::json!({"path": idx.file_path(f), "logical": f.logical, "alloc": f.alloc, "private": f.private, "mtime": f.mtime})
            );
        }
        return Ok(());
    }
    for f in idx.top_files.iter().take(c.lines) {
        println!("{:>9}  {:>4}  {}", fmt::bytes(f.size(by)), ui::dim(&fmt::age(now - f.mtime)), path::display_path(&idx.file_path(f)));
    }
    Ok(())
}

pub fn cmd_diff(c: DiffCmd) -> Result<()> {
    let root = std::fs::canonicalize(c.scan.root())?;
    let snap = snapshot::snapshot_path(&root, None);
    let old = snapshot::load(&snap).with_context(|| format!("no snapshot for {} — run `tidy scan --save` first", root.display()))?;
    let idx = scan_and_keep(&root, &c.scan.options(64))?;
    let min = fmt::parse_bytes(&c.min).context("--min")?;
    let changes = tidy_scan::snapshot::diff(&old.index, &idx, c.scan.by, min);
    // Collapse an ancestor into its descendant when the descendant explains
    // ≥90% of the ancestor's change (changes arrive largest first).
    let mut shown = 0;
    let mut kept: Vec<(PathBuf, i64)> = Vec::new();
    for (p, d) in &changes {
        kept.retain(|(k, kd)| !(p.starts_with(k) && kd.signum() == d.signum() && d.unsigned_abs() * 10 >= kd.unsigned_abs() * 9));
        kept.push((p.clone(), *d));
    }
    kept.sort_unstable_by_key(|(_, d)| std::cmp::Reverse(d.unsigned_abs()));
    println!(
        "{}  {}",
        ui::bold(&path::display_path(&root)),
        ui::dim(&format!(
            "since {} ago · {} → {}",
            fmt::age(fmt::now_secs() - old.created),
            fmt::bytes(old.index.root_node().size(c.scan.by)),
            fmt::bytes(idx.root_node().size(c.scan.by))
        ))
    );
    for (p, d) in kept {
        if shown >= c.lines {
            break;
        }
        let sign = if d >= 0 { "+" } else { "-" };
        println!("{sign}{:>9}  {}", fmt::bytes(d.unsigned_abs()), path::display_path(&p));
        shown += 1;
    }
    snapshot::save(&idx, &snap)?;
    Ok(())
}
