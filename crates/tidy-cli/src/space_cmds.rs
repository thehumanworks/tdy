//! `tidy dupes`, `tidy archive`, `tidy compress`: plans that reclaim space
//! without deleting anything unique.

use crate::ui;
use anyhow::{Context, Result};
use clap::Args;
use std::collections::HashSet;
use std::path::PathBuf;
use tidy_ops::space;
use tidy_plan::model::*;
use tidy_plan::store;
use tidy_rules::guards::{Facts, Guards};
use tidy_rules::planner;
use tidy_scan::{SizeKind, fmt, path::display_path, path::expand_tilde};

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
        println!(
            "{}",
            serde_json::json!({"groups": groups.iter().take(c.lines).collect::<Vec<_>>(), "reclaimable": total, "count": groups.len()})
        );
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
            println!("{}", ui::dim("make a plan with: tidy dupes --plan"));
        }
        return Ok(());
    }
    let (policy, hash, ppath) = crate::plan_cmds::load_policy(None)?;
    let guards = Guards::new(policy.guard_config());
    let mut plan = Plan { header: planner::new_header(roots.clone(), Mode::Manual, ppath, hash, None), entries: vec![] };
    let mut taken = HashSet::new();
    let mut blocked = 0;
    for g in &groups {
        let keeper = &g.files[0];
        let can_clone = space::clone_supported(&keeper.path);
        for f in &g.files[1..] {
            if f.private.is_some_and(|p| p < f.alloc / 2) {
                continue; // already deduplicated
            }
            let action = if can_clone { Action::Dedupe } else { Action::Trash };
            let facts = Facts { mtime_max: Some(f.mtime), flags: 0, regenerable: false, min_age: None };
            if guards.check(&f.path, action, &facts, Some(&[])).is_err() {
                blocked += 1;
                continue;
            }
            let Ok(mut fp) = tidy_ops::fingerprint(&f.path, None, false) else { continue };
            fp.hash = Some(g.hash.clone());
            let id = entry_id(&f.path, action, &taken);
            taken.insert(id.clone());
            plan.entries.push(Entry {
                id,
                status: Status::Pending,
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
    store::save(&plan)?;
    let cnt = plan.counts();
    println!(
        "{} {}  {}",
        ui::bold("plan"),
        ui::bold(&plan.header.id),
        ui::dim(&format!("{} actions · {} reclaimable · {} blocked by guards", cnt.total, fmt::bytes(cnt.reclaimable), blocked))
    );
    println!("{}", ui::dim(&format!("next: tidy review {}", plan.header.id)));
    Ok(())
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

/// Plan archives (or transparent compression) for large, cold, compressible folders.
pub fn cmd_archive(c: ArchiveCmd) -> Result<()> {
    let roots = roots(&c.paths)?;
    let older = fmt::parse_duration(&c.older).context("--older")?;
    let min = fmt::parse_bytes(&c.min_size).context("--min-size")?;
    let (policy, hash, ppath) = crate::plan_cmds::load_policy(None)?;
    let guards = Guards::new(policy.guard_config());
    let mut plan = Plan { header: planner::new_header(roots.clone(), Mode::Manual, ppath, hash, None), entries: vec![] };
    let mut taken = HashSet::new();
    let cutoff = fmt::now_secs() - older;
    let dcfg = policy.detect_config();
    for root in &roots {
        let idx = tidy_scan::scan(root, &tidy_scan::ScanOptions { top_k: 0, ..Default::default() })?;
        // Regenerable artifacts are for deletion, not archiving.
        let artifacts: Vec<PathBuf> = tidy_rules::detect::detect(&idx, &tidy_rules::detect::DetectConfig { min_size: 0, ..dcfg.clone() })
            .into_iter()
            .map(|f| f.path)
            .collect();
        for id in idx.stale_dirs(cutoff, min, SizeKind::Alloc) {
            let p = idx.path(id);
            if artifacts.iter().any(|a| p.starts_with(a) || a.starts_with(&p)) {
                continue;
            }
            let node = idx.nodes[id as usize];
            let action = if c.transparent && space::transparent_compression_supported(&p) { Action::Compress } else { Action::Archive };
            let git = tidy_rules::guards::git_roots_in_index(&idx, id);
            let facts = Facts { mtime_max: Some(node.mtime_max), flags: 0, regenerable: false, min_age: Some(older) };
            if guards.check(&p, action, &facts, Some(&git)).is_err() {
                continue;
            }
            let ratio = space::compressibility(&p, 64);
            if ratio > c.max_ratio {
                continue;
            }
            let Ok(fp) = tidy_ops::fingerprint(&p, None, false) else { continue };
            let eid = entry_id(&p, action, &taken);
            taken.insert(eid.clone());
            let est = (node.alloc as f64 * (1.0 - ratio)) as u64;
            plan.entries.push(Entry {
                id: eid,
                status: Status::Pending,
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
                    "untouched for {} · sample compresses to {:.0}% · ≈{} saved{}",
                    fmt::age(fmt::now_secs() - node.mtime_max),
                    ratio * 100.0,
                    fmt::bytes(est),
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
    store::save(&plan)?;
    let cnt = plan.counts();
    println!(
        "{} {}  {}",
        ui::bold("plan"),
        ui::bold(&plan.header.id),
        ui::dim(&format!("{} cold folders · ≈{} saved", cnt.total, fmt::bytes(cnt.reclaimable)))
    );
    for e in plan.entries.iter().take(c.lines) {
        println!("  {}", tidy_plan::review::line(e));
    }
    if !plan.entries.is_empty() {
        println!("{}", ui::dim(&format!("next: tidy review {}", plan.header.id)));
    }
    Ok(())
}
