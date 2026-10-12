//! `tdy dupes`, `tdy archive`, `tdy compress`: plans that reclaim space
//! without deleting anything unique.

use crate::ui;
use anyhow::{Context, Result};
use clap::Args;
use std::collections::HashSet;
use std::path::PathBuf;
use tdy_ops::space;
use tdy_plan::model::*;
use tdy_plan::store;
use tdy_rules::guards::{Facts, Guards};
use tdy_rules::planner;
use tdy_scan::{SizeKind, fmt, path::display_path, path::expand_tilde};

fn roots(paths: &[String]) -> Result<Vec<PathBuf>> {
    paths.iter().map(|p| std::fs::canonicalize(expand_tilde(p)).with_context(|| p.clone())).collect()
}

#[derive(Args)]
pub struct DupesCmd {
    /// Folders to search (default: the current folder).
    #[arg(default_value = ".")]
    pub paths: Vec<String>,
    #[arg(long, default_value = "1MB")]
    pub min_size: String,
    #[arg(short = 'n', long, default_value_t = 20)]
    pub lines: usize,
    /// Write a plan: clone-dedupe where the filesystem supports it, else trash the copies.
    #[arg(long)]
    pub plan: bool,
    #[arg(short = 'j', long, default_value_t = 0)]
    pub threads: usize,
    #[arg(long)]
    pub json: bool,
}

pub fn cmd_dupes(c: DupesCmd) -> Result<()> {
    let roots = roots(&c.paths)?;
    let min = fmt::parse_bytes(&c.min_size).context("--min-size")?;
    let t = std::time::Instant::now();
    let groups = space::find_duplicates(&roots, min, c.threads)?;
    let total: u64 = groups.iter().map(|g| g.reclaimable()).sum();
    if c.json {
        let mut result =
            serde_json::json!({"groups": groups.iter().take(c.lines).collect::<Vec<_>>(), "reclaimable": total, "count": groups.len()});
        if c.plan {
            let SpacePlan { plan, blocked } = dupes_plan(roots, &groups)?;
            result["path"] = serde_json::to_value(store::save(&plan)?)?;
            result["plan"] = serde_json::to_value(&plan.header.id)?;
            result["counts"] = serde_json::to_value(plan.counts())?;
            result["blocked"] = serde_json::to_value(blocked)?;
        }
        println!("{result}");
        return Ok(());
    }
    println!(
        "{}  {}",
        ui::bold(&format!("{} duplicate groups", groups.len())),
        ui::dim(&format!("{} reclaimable · {:.1}s", fmt::bytes(total), t.elapsed().as_secs_f64()))
    );
    for g in groups.iter().take(c.lines) {
        println!("  {:>9} × {}  {}", fmt::bytes(g.size), g.files.len(), ui::dim(&format!("{} reclaimable", fmt::bytes(g.reclaimable()))));
        for (i, f) in g.files.iter().enumerate() {
            let shared = f.private.is_some_and(|p| p < f.alloc / 2);
            println!(
                "      {} {}{}",
                if i == 0 { ui::green("keep") } else { ui::dim("dup ") },
                display_path(&f.path),
                if shared { ui::dim("  (already shares storage)") } else { String::new() }
            );
        }
    }
    if !c.plan {
        if !groups.is_empty() {
            println!("{}", ui::dim("make a plan with: tdy dupes --plan"));
        }
        return Ok(());
    }
    let SpacePlan { plan, blocked, .. } = dupes_plan(roots, &groups)?;
    store::save(&plan)?;
    let cnt = plan.counts();
    println!(
        "{} {}  {}",
        ui::bold("plan"),
        ui::bold(&plan.header.id),
        ui::dim(&format!(
            "{} actions · {} reclaimable · {} blocked by guards",
            cnt.pending + cnt.approved,
            fmt::bytes(cnt.reclaimable),
            blocked
        ))
    );
    println!("{}", ui::dim(&format!("next: tdy review {}", plan.header.id)));
    if blocked > 0 {
        println!(
            "{}",
            ui::yellow(&format!("Override refused items: tdy review {} --override-refusals (optionally --entry ID)", plan.header.id))
        );
    }
    Ok(())
}

/// A plan from one of the space finders, not saved yet.
pub struct SpacePlan {
    pub plan: Plan,
    /// Items the guards refused.
    pub blocked: usize,
}

/// Clone-dedupe the copies in `groups` where the filesystem can share
/// storage, else trash them; the first file of each group is kept.
pub fn dupes_plan(roots: Vec<PathBuf>, groups: &[space::DupGroup]) -> Result<SpacePlan> {
    let (policy, hash, ppath) = crate::plan_cmds::load_policy(None)?;
    let guards = Guards::new(policy.guard_config());
    let mut plan = Plan { header: planner::new_header(roots, Mode::Manual, ppath, hash, None), entries: vec![] };
    let mut taken = HashSet::new();
    let mut blocked = 0;
    for g in groups {
        let keeper = &g.files[0];
        let can_clone = space::clone_supported(&keeper.path);
        for f in &g.files[1..] {
            if f.private.is_some_and(|p| p < f.alloc / 2) {
                continue; // already deduplicated
            }
            let action = if can_clone { Action::Dedupe } else { Action::Trash };
            let facts = Facts { mtime_max: Some(f.mtime), flags: 0, regenerable: false, min_age: None };
            let refusal = guards.check(&f.path, action, &facts, Some(&[])).err().map(|b| b.to_string());
            blocked += usize::from(refusal.is_some());
            let Ok(mut fp) = tdy_ops::fingerprint(&f.path, None, false) else { continue };
            fp.hash = Some(g.hash.clone());
            let id = entry_id(&f.path, action, &taken);
            taken.insert(id.clone());
            plan.entries.push(Entry {
                status: if refusal.is_some() { Status::Rejected } else { Status::Pending },
                refusal,
                override_refusal: false,
                id,
                approved_by: None,
                action,
                path: f.path.clone(),
                dest: Some(keeper.path.clone()),
                create_dest: false,
                on_conflict: OnConflict::Skip,
                tags: vec![],
                bytes: Bytes { logical: f.size, alloc: f.alloc, private: f.private.unwrap_or(f.alloc).min(f.alloc) },
                fingerprint: fp,
                source: Source { layer: Layer::L1, rule: "duplicate".into(), p: None },
                reason: if can_clone {
                    format!("identical to {} — replace with a copy-on-write clone (path stays, space freed)", display_path(&keeper.path))
                } else {
                    format!(
                        "identical to {} — this filesystem cannot share storage, so the copy goes to the trash",
                        display_path(&keeper.path)
                    )
                },
                undo: if can_clone { Undo::None } else { Undo::Restore },
                alternatives: vec![],
                evidence: None,
                outcome: None,
            });
        }
    }
    Ok(SpacePlan { plan, blocked })
}

#[derive(Args)]
pub struct ArchiveCmd {
    /// Folders to look in (default: the current folder).
    #[arg(default_value = ".")]
    pub paths: Vec<String>,
    /// Only folders untouched for this long.
    #[arg(long, default_value = "1y")]
    pub older: String,
    #[arg(long, default_value = "100MB")]
    pub min_size: String,
    /// Skip folders whose sample compresses worse than this ratio.
    #[arg(long, default_value_t = 0.8)]
    pub max_ratio: f64,
    /// Prefer transparent filesystem compression (APFS/btrfs) over archives.
    #[arg(long)]
    pub transparent: bool,
    #[arg(short = 'n', long, default_value_t = 20)]
    pub lines: usize,
}

/// What `archive_plan` looks for.
#[derive(Clone, Debug)]
pub struct ArchiveOpts {
    /// Untouched for at least this many seconds.
    pub older: i64,
    pub min_size: u64,
    /// Skip folders whose sample compresses worse than this ratio.
    pub max_ratio: f64,
    /// Transparent filesystem compression (APFS/btrfs) instead of archives.
    pub transparent: bool,
}

/// Archive (or compress in place) large, cold, compressible folders.
pub fn archive_plan(roots: Vec<PathBuf>, o: &ArchiveOpts, progress: Option<std::sync::Arc<tdy_scan::ScanProgress>>) -> Result<SpacePlan> {
    let (policy, hash, ppath) = crate::plan_cmds::load_policy(None)?;
    let guards = Guards::new(policy.guard_config());
    let mut plan = Plan { header: planner::new_header(roots.clone(), Mode::Manual, ppath, hash, None), entries: vec![] };
    let mut taken = HashSet::new();
    let mut blocked = 0;
    let older = o.older;
    let cutoff = fmt::now_secs() - older;
    let dcfg = policy.detect_config();
    for root in &roots {
        let opts = tdy_scan::ScanOptions { top_k: crate::scan_cmds::CACHED_TOP, progress: progress.clone(), ..Default::default() };
        let idx = tdy_scan::scan(root, &opts)?;
        // a full scan: the next look at this folder can start from it
        let _ = tdy_scan::cache::store(&idx);
        // Regenerable artifacts are for deletion, not archiving.
        let artifacts: Vec<PathBuf> = tdy_rules::detect::detect(&idx, &tdy_rules::detect::DetectConfig { min_size: 0, ..dcfg.clone() })
            .into_iter()
            .map(|f| f.path)
            .collect();
        for id in idx.stale_dirs(cutoff, o.min_size, SizeKind::Alloc) {
            let p = idx.path(id);
            if artifacts.iter().any(|a| p.starts_with(a) || a.starts_with(&p)) {
                continue;
            }
            let node = idx.nodes[id as usize];
            let action = if o.transparent && space::transparent_compression_supported(&p) { Action::Compress } else { Action::Archive };
            let git = tdy_rules::guards::git_roots_in_index(&idx, id);
            let facts = Facts { mtime_max: Some(node.mtime_max), flags: 0, regenerable: false, min_age: Some(older) };
            let refusal = guards.check(&p, action, &facts, Some(&git)).err().map(|b| b.to_string());
            // Do not open refused content to estimate savings: reading a cloud
            // placeholder could download it before the user authorizes an override.
            let ratio = refusal.is_none().then(|| space::compressibility(&p, 64));
            if ratio.is_some_and(|r| r > o.max_ratio) {
                continue;
            }
            let Ok(fp) = tdy_ops::fingerprint(&p, None, false) else { continue };
            blocked += usize::from(refusal.is_some());
            let eid = entry_id(&p, action, &taken);
            taken.insert(eid.clone());
            let est = ratio.map(|r| (node.alloc as f64 * (1.0 - r)) as u64).unwrap_or(0);
            plan.entries.push(Entry {
                status: if refusal.is_some() { Status::Rejected } else { Status::Pending },
                refusal,
                override_refusal: false,
                id: eid,
                approved_by: None,
                action,
                path: p.clone(),
                dest: None,
                create_dest: false,
                on_conflict: OnConflict::Skip,
                tags: vec![],
                bytes: Bytes { logical: node.logical, alloc: node.alloc, private: est },
                fingerprint: fp,
                source: Source {
                    layer: Layer::L1,
                    rule: if action == Action::Compress { "cold-compress".into() } else { "cold-archive".into() },
                    p: None,
                },
                reason: format!(
                    "untouched for {} · {}{}",
                    fmt::age(fmt::now_secs() - node.mtime_max),
                    ratio
                        .map(|r| format!("sample compresses to {:.0}% · ≈{} saved", r * 100.0, fmt::bytes(est)))
                        .unwrap_or_else(|| "compression not sampled: guard refused content access".into()),
                    if action == Action::Archive {
                        " (original kept in quarantine until purged)"
                    } else {
                        " (transparent: files stay where they are)"
                    }
                ),
                undo: if action == Action::Archive { Undo::Restore } else { Undo::None },
                alternatives: vec![],
                evidence: None,
                outcome: None,
            });
        }
    }
    plan.entries.sort_by_key(|e| std::cmp::Reverse(e.bytes.private));
    Ok(SpacePlan { plan, blocked })
}

/// Plan archives (or transparent compression) for large, cold, compressible folders.
pub fn cmd_archive(c: ArchiveCmd) -> Result<()> {
    let roots = roots(&c.paths)?;
    let o = ArchiveOpts {
        older: fmt::parse_duration(&c.older).context("--older")?,
        min_size: fmt::parse_bytes(&c.min_size).context("--min-size")?,
        max_ratio: c.max_ratio,
        transparent: c.transparent,
    };
    let SpacePlan { plan, blocked } = archive_plan(roots, &o, None)?;
    store::save(&plan)?;
    let cnt = plan.counts();
    println!(
        "{} {}  {}",
        ui::bold("plan"),
        ui::bold(&plan.header.id),
        ui::dim(&format!("{} cold folders · ≈{} saved", cnt.total, fmt::bytes(cnt.reclaimable)))
    );
    for e in plan.entries.iter().take(c.lines) {
        println!("  {}", tdy_plan::review::line(e));
    }
    if !plan.entries.is_empty() {
        println!("{}", ui::dim(&format!("next: tdy review {}", plan.header.id)));
        if blocked > 0 {
            println!(
                "{}",
                ui::yellow(&format!("Override refused items: tdy review {} --override-refusals (optionally --entry ID)", plan.header.id))
            );
        }
    }
    Ok(())
}
