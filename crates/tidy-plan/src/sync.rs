//! Keeps Tidy's cached folder indexes in step with an apply or undo, so the
//! next look at those folders needs no rescan (see `tidy_scan::cache`).

use crate::exec::{ApplyOptions, Event, Preflight, Report};
use crate::model::{Action, Plan, Status};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tidy_scan::cache::{self, Synced};
use tidy_scan::patch::{Recording, Watch};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Apply,
    Undo,
}

/// The folders an apply or undo is about to change, as they are now.
pub struct Tracker {
    rec: Recording,
    op: Op,
    started: i64,
}

/// [`crate::apply`], keeping the cached indexes in step (dry runs change
/// nothing, so they aren't tracked).
pub fn apply(plan: &mut Plan, opts: &ApplyOptions, preflight: Preflight<'_>, on: &mut dyn FnMut(Event<'_>)) -> anyhow::Result<Report> {
    let tracker = (!opts.dry_run).then(|| watch(plan, Op::Apply, opts.only.as_ref()));
    let r = crate::apply(plan, opts, preflight, on);
    // a failed run may still have changed some entries
    if let Some(t) = tracker {
        t.finish(plan);
    }
    r
}

/// [`crate::undo`], keeping the cached indexes in step.
pub fn undo(plan: &mut Plan, only: Option<&HashSet<String>>, on: &mut dyn FnMut(Event<'_>)) -> anyhow::Result<Report> {
    let tracker = watch(plan, Op::Undo, only);
    let r = crate::undo(plan, only, on);
    tracker.finish(plan);
    r
}

/// Before applying (or undoing) `plan`: look at the folders its approved
/// (or applied) entries can change, where a cached index covers them.
/// `only` limits it to those entries, like the apply or undo does.
pub fn watch(plan: &Plan, op: Op, only: Option<&HashSet<String>>) -> Tracker {
    let started = tidy_scan::fmt::now_secs();
    let roots = cache::roots();
    // Unshared bytes come free with APFS listings; on Linux they cost a
    // FIEMAP per file, and the change's allocated bytes stand in for them.
    let mut w = Watch::new(cfg!(target_os = "macos")).within(roots.clone());
    if !roots.is_empty() {
        for e in &plan.entries {
            let take = match op {
                Op::Apply => e.status == Status::Approved,
                Op::Undo => e.status == Status::Applied,
            };
            if !take || e.action == Action::Tag || only.is_some_and(|o| !o.contains(&e.id)) {
                continue;
            }
            w.add(&e.path);
            // compressing a folder rewrites it under the same name
            if op == Op::Apply && e.action == Action::Compress && e.path.is_dir() {
                w.rescan(&e.path);
            }
            if let Some(d) = &e.dest {
                w.add(d);
            }
            if let Some(o) = &e.outcome {
                for p in o.now_at.iter().chain(&o.created) {
                    w.add(p);
                }
                if let Some(info) = o.now_at.as_deref().and_then(trashinfo) {
                    w.add(&info);
                }
            }
        }
        // the plan file, its lock and its journal change too
        w.add(&crate::store::plan_path(&plan.header.id));
        w.add(&crate::journal::path(&plan.header.id));
    }
    Tracker { rec: w.start(), op, started }
}

/// The XDG `.trashinfo` beside an item trashed to `<trash>/files/<name>`.
fn trashinfo(now_at: &Path) -> Option<PathBuf> {
    let files = now_at.parent()?;
    if files.file_name()? != "files" {
        return None;
    }
    let mut name = now_at.file_name()?.to_os_string();
    name.push(".trashinfo");
    Some(files.parent()?.join("info").join(name))
}

impl Tracker {
    /// After the apply or undo: look again and patch the cached indexes.
    pub fn finish(self, plan: &Plan) -> Synced {
        // where this apply put things (trash, quarantine, destinations)
        let landed: Vec<PathBuf> = match self.op {
            Op::Apply => plan
                .entries
                .iter()
                .filter(|e| e.status == Status::Applied)
                .filter_map(|e| e.outcome.as_ref().filter(|o| o.at >= self.started)?.now_at.clone())
                .flat_map(|p| {
                    let info = trashinfo(&p);
                    std::iter::once(p).chain(info)
                })
                .collect(),
            Op::Undo => vec![],
        };
        cache::sync(&self.rec.finish(landed))
    }
}
