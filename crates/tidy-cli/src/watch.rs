//! `tidy watch`: watch inbox folders, wait for new files to settle, judge
//! them against the policy and append entries to a watch plan. In
//! autonomous mode, entries the policy approves are applied immediately.

use crate::ui;
use anyhow::{Context, Result, bail};
use clap::Args;
use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tidy_plan::model::*;
use tidy_plan::{ApplyOptions, store};
use tidy_rules::calib::Calibration;
use tidy_rules::guards::Guards;
use tidy_rules::l2;
use tidy_scan::{fmt, path::display_path, path::expand_tilde};

#[derive(Args)]
pub struct WatchCmd {
    /// Folders to watch (default: every rule scope in the policy).
    pub paths: Vec<String>,
    #[arg(long)]
    pub policy: Option<PathBuf>,
    /// Seconds a file must stay unchanged before it is judged.
    #[arg(long, default_value_t = 2.0)]
    pub settle: f64,
    /// Desktop notification for each proposal.
    #[arg(long)]
    pub notify: bool,
    /// Exit after this many seconds (for tests and cron-style use).
    #[arg(long)]
    pub duration: Option<f64>,
}

fn ignored(p: &Path) -> bool {
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    name.starts_with('.')
        || name.starts_with("~$")
        || [".part", ".crdownload", ".download", ".tmp", ".partial", ".opdownload", ".tidy-partial", ".swp"]
            .iter()
            .any(|s| name.ends_with(s))
        || p.components().any(|c| c.as_os_str() == ".tidy-state" || c.as_os_str() == "node_modules")
}

fn notify_desktop(title: &str, body: &str) {
    if cfg!(target_os = "macos") {
        let script = format!("display notification {:?} with title {:?}", body, title);
        let _ = std::process::Command::new("osascript").arg("-e").arg(script).status();
    } else {
        let _ = std::process::Command::new("notify-send").arg(title).arg(body).status();
    }
}

pub fn cmd_watch(c: WatchCmd) -> Result<()> {
    let (policy, hash, ppath) = crate::plan_cmds::load_policy(c.policy.as_deref())?;
    if policy.rules.is_empty() {
        bail!("the policy has no rules to watch for (see `tidy init`)");
    }
    let built = crate::judge_cmds::build_judge(&policy, false)?;
    // The watcher's own settle check replaces the guards' organising delay;
    // removal actions keep the policy's full minimum age.
    let mut gc = policy.guard_config();
    gc.settle = c.settle.ceil() as i64;
    let guards = Guards::new(gc.clone());
    let calib = Calibration::load();
    let mut roots: Vec<PathBuf> = c.paths.iter().filter_map(|p| std::fs::canonicalize(expand_tilde(p)).ok()).collect();
    let rules = l2::compile(&policy, &roots, built.as_ref().map(|b| b.id.as_str()), &calib);
    if roots.is_empty() {
        let mut set = HashSet::new();
        for r in &rules {
            for s in &r.scopes {
                if set.insert(s.clone()) {
                    roots.push(s.clone());
                }
            }
        }
    }
    roots.retain(|r| r.is_dir());
    if roots.is_empty() {
        bail!("nothing to watch: no rule scope exists on this machine");
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx).context("starting file watcher")?;
    for r in &roots {
        watcher.watch(r, RecursiveMode::Recursive).with_context(|| format!("watching {}", r.display()))?;
    }
    let mut plan = Plan {
        header: tidy_rules::planner::new_header(roots.clone(), policy.mode, ppath, hash, built.as_ref().map(|b| b.id.clone())),
        entries: vec![],
    };
    plan.header.note = Some("watch".into());
    let mut taken: HashSet<String> = HashSet::new();
    println!(
        "{} {}  {}",
        ui::bold("watching"),
        roots.iter().map(|r| display_path(r)).collect::<Vec<_>>().join(", "),
        ui::dim(&format!("plan {} · mode {:?} · settle {}s · Ctrl-C to stop", plan.header.id, policy.mode, c.settle))
    );
    // path → (first seen, last (size, mtime), last change)
    let mut pending: HashMap<PathBuf, (Instant, (u64, i64), Instant)> = HashMap::new();
    let settle = Duration::from_secs_f64(c.settle);
    let started = Instant::now();
    let conc = policy.judge.concurrency.unwrap_or(32);
    loop {
        if let Some(d) = c.duration
            && started.elapsed().as_secs_f64() >= d
        {
            break;
        }
        // drain events
        while let Ok(ev) = rx.recv_timeout(Duration::from_millis(250)) {
            let Ok(ev) = ev else { continue };
            if !matches!(ev.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                continue;
            }
            for p in ev.paths {
                if ignored(&p) || plan.entries.iter().any(|e| e.path == p && e.status.reviewable()) {
                    continue;
                }
                let now = Instant::now();
                pending.entry(p).or_insert((now, (u64::MAX, 0), now));
            }
            if pending.len() > 10_000 {
                break;
            }
        }
        // settle check
        let mut ready = Vec::new();
        let now = Instant::now();
        pending.retain(|p, (first, last, changed)| {
            use std::os::unix::fs::MetadataExt;
            let Ok(md) = std::fs::symlink_metadata(p) else { return false };
            let cur = (md.len(), md.mtime());
            if cur != *last {
                *last = cur;
                *changed = now;
                return true;
            }
            if now.duration_since(*changed) >= settle {
                ready.push((p.clone(), *first));
                return false;
            }
            true
        });
        if ready.is_empty() {
            continue;
        }
        let paths: Vec<PathBuf> = ready.iter().map(|r| r.0.clone()).collect();
        let first_seen: HashMap<PathBuf, Instant> = ready.into_iter().collect();
        let items = l2::items_for_paths(&rules, &paths, fmt::now_secs());
        if items.is_empty() {
            continue;
        }
        let run = match l2::run_on(&policy, &rules, items, built.as_ref().map(|b| b.judge.as_ref()), &guards, conc, &|_, _| {}) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{}", ui::yellow(&format!("judge error: {e:#}")));
                continue;
            }
        };
        let new = l2::entries(&run, policy.mode, built.as_ref().map(|b| b.id.as_str()), &mut taken);
        for o in &run.outcomes {
            if let l2::OutcomeKind::Kept { rule, p } = &o.kind {
                println!(
                    "  {} {}  {}",
                    ui::dim("keep"),
                    display_path(&o.item.path),
                    ui::dim(&format!("{rule} {}", p.map(|p| format!("{p:.2}")).unwrap_or("unanswered".into())))
                );
            }
        }
        if new.is_empty() {
            continue;
        }
        let ids: HashSet<String> = new.iter().map(|e| e.id.clone()).collect();
        for e in &new {
            let lat = first_seen.get(&e.path).map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
            println!("  {}  {}", tidy_plan::review::line(e), ui::dim(&format!("+{lat:.1}s")));
            if c.notify {
                notify_desktop("Tidy", &format!("{} {}", e.action.as_str(), e.path.file_name().unwrap_or_default().to_string_lossy()));
            }
        }
        plan.entries.extend(new);
        store::save(&plan)?;
        // Autonomous: apply what the policy approved, right away.
        if plan.entries.iter().any(|e| ids.contains(&e.id) && e.status == Status::Approved) {
            let guard = crate::plan_cmds::apply_guard_with(gc.clone());
            let opts = ApplyOptions { dry_run: false, only: Some(ids), verify: true };
            let r = tidy_plan::apply(&mut plan, &opts, &guard, &mut |ev| {
                if let tidy_plan::Event::Ok(e, m) = ev {
                    println!("    {} {} {}", ui::green("✓"), e.id, m.replace(&tidy_scan::path::home_dir().display().to_string(), "~"));
                }
            })?;
            if r.failed > 0 {
                eprintln!("{}", ui::yellow(&format!("{} entries failed — see `tidy show {}`", r.failed, plan.header.id)));
            }
        }
    }
    drop(watcher);
    if !plan.entries.is_empty() {
        println!("{}", ui::dim(&format!("review with: tidy review {}", plan.header.id)));
    }
    Ok(())
}
