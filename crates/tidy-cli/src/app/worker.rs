//! Background jobs for the TUI. Each job runs on its own thread and reports
//! back over a channel, so the screen never blocks on disk or network I/O.

use crate::judge_cmds::RulesExplanation;
use crate::plan_cmds::{EventKind, Explanation, Known};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use tidy_plan::{Action, ApplyOptions, store, sync};
use tidy_rules::Policy;
use tidy_rules::guards::Guards;
use tidy_scan::{Index, ScanOptions, ScanProgress};

/// What a detector says about a directory in the scan.
#[derive(Clone, Debug)]
pub struct Tag {
    pub id: &'static str,
    pub what: String,
    pub hint: &'static str,
    pub min_age: Option<i64>,
}

pub struct Scanned {
    pub index: Index,
    pub tags: HashMap<u32, Tag>,
    /// For an index from the cache: the folder whose scan it came from.
    pub cached_from: Option<PathBuf>,
}

impl Scanned {
    fn new(index: Index, policy: &Policy, cached_from: Option<PathBuf>) -> Self {
        let tags = tidy_rules::detect::detect(&index, &policy.detect_config())
            .into_iter()
            .map(|f| (f.node, Tag { id: f.detector, what: f.reason, hint: f.hint, min_age: f.min_age }))
            .collect();
        Scanned { index, tags, cached_from }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecOp {
    DryRun,
    Apply,
    Undo,
}

#[derive(Debug, Default, Clone)]
pub struct CleanupResult {
    pub plan: String,
    pub actions: usize,
    pub reclaimable: u64,
    pub blocked: usize,
    pub blocked_bytes: u64,
    pub judge: Option<String>,
    pub warnings: Vec<String>,
    pub elapsed_ms: u64,
}

pub struct ProposeResult {
    pub plan: Option<String>,
    pub entries: usize,
    pub bytes: u64,
    pub rejected: Vec<(PathBuf, String)>,
    pub accepted: Vec<PathBuf>,
}

pub enum Msg {
    Scanned {
        seq: u64,
        result: Result<Scanned, String>,
    },
    /// An index from the cache (`None`: there is none for this folder).
    Cached {
        seq: u64,
        scanned: Option<Scanned>,
    },
    Explained {
        path: PathBuf,
        result: Result<Explanation, String>,
    },
    Rules {
        path: PathBuf,
        result: Result<Option<RulesExplanation>, String>,
    },
    Proposed {
        action: Action,
        result: Result<ProposeResult, String>,
    },
    CleanupScanning(Arc<ScanProgress>),
    CleanupPhase(&'static str),
    CleanupJudging(usize, usize),
    CleanupDone(Result<CleanupResult, String>),
    ExecLine {
        plan: String,
        kind: EventKind,
        text: String,
    },
    /// `synced`: cached indexes patched to match (none for dry runs).
    ExecDone {
        plan: String,
        op: ExecOp,
        result: Result<tidy_plan::Report, String>,
        synced: tidy_scan::cache::Synced,
    },
}

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

/// What the Explore screen can show from the cache: any scan of the folder
/// (or of one above it) on this filesystem.
fn cached_index(root: &std::path::Path, policy: &Policy) -> Option<Scanned> {
    let want = ScanOptions { top_k: 0, ..Default::default() };
    tidy_scan::cache::load(root, &want).map(|c| Scanned::new(c.index, policy, Some(c.from)))
}

/// Scan `root` for the Explore screen and tag what the detectors recognise.
/// With `cached`, first send the cached index, if there is one, so the
/// screen fills at once while the scan runs.
pub fn scan(tx: Sender<Msg>, seq: u64, root: PathBuf, policy: Arc<Policy>, progress: Arc<ScanProgress>, cached: bool) {
    std::thread::spawn(move || {
        if cached && let Some(s) = cached_index(&root, &policy) {
            let _ = tx.send(Msg::Cached { seq, scanned: Some(s) });
        }
        let opts = ScanOptions {
            top_k: crate::scan_cmds::CACHED_TOP,
            // Free on APFS (same syscall); FIEMAP per file on Linux is not.
            private: cfg!(target_os = "macos"),
            progress: Some(progress),
            ..Default::default()
        };
        let result = tidy_scan::scan(&root, &opts).map_err(|e| e.to_string()).map(|index| {
            // best effort: the next start is instant (stderr belongs to the screen)
            let _ = tidy_scan::cache::store(&index);
            Scanned::new(index, &policy, None)
        });
        let _ = tx.send(Msg::Scanned { seq, result });
    });
}

/// Read `root`'s index back from the cache (after an apply patched it).
pub fn reload(tx: Sender<Msg>, seq: u64, root: PathBuf, policy: Arc<Policy>) {
    std::thread::spawn(move || {
        let _ = tx.send(Msg::Cached { seq, scanned: cached_index(&root, &policy) });
    });
}

pub fn explain(tx: Sender<Msg>, guards: Arc<Guards>, path: PathBuf, known: Option<Known>) {
    std::thread::spawn(move || {
        let result = crate::plan_cmds::explain_path_with(&path, &guards, known).map_err(err);
        let _ = tx.send(Msg::Explained { path, result });
    });
}

pub fn rules(tx: Sender<Msg>, policy: Arc<Policy>, path: PathBuf) {
    std::thread::spawn(move || {
        let result = crate::judge_cmds::rules_for_path(&path, &policy).map_err(err);
        let _ = tx.send(Msg::Rules { path, result });
    });
}

/// Marked paths → a plan approved by the human who marked them.
pub fn propose(tx: Sender<Msg>, action: Action, items: Vec<(PathBuf, tidy_plan::Bytes)>) {
    std::thread::spawn(move || {
        let proposals = items
            .into_iter()
            .map(|(path, bytes)| crate::propose::Proposal {
                action,
                path,
                dest: None,
                tags: vec![],
                reason: format!("marked in tidy ui ({})", action.as_str()),
                bytes: Some(bytes),
            })
            .collect();
        let result = crate::propose::propose(proposals, crate::propose::By::Human, "marked in tidy ui").map_err(err).map(|p| {
            let (plan, entries, bytes, accepted) = match &p.plan {
                Some(pl) => (
                    Some(pl.header.id.clone()),
                    pl.entries.len(),
                    pl.entries.iter().map(|e| e.bytes.alloc).sum(),
                    pl.entries.iter().map(|e| e.path.clone()).collect(),
                ),
                None => (None, 0, 0, vec![]),
            };
            ProposeResult { plan, entries, bytes, rejected: p.rejected, accepted }
        });
        let _ = tx.send(Msg::Proposed { action, result });
    });
}

/// Detectors + policy rules for one folder, like `tidy plan`.
pub fn cleanup(tx: Sender<Msg>, root: PathBuf, detectors: bool, rules: bool) {
    std::thread::spawn(move || {
        let tx2 = tx.clone();
        let result = (|| -> anyhow::Result<CleanupResult> {
            let started = std::time::Instant::now();
            let (policy, hash, ppath) = crate::plan_cmds::load_policy(None)?;
            let guards = Guards::new(policy.guard_config());
            let header = tidy_rules::planner::new_header(vec![root.clone()], policy.mode, ppath, hash, None);
            let progress = Arc::new(ScanProgress::default());
            let _ = tx2.send(Msg::CleanupScanning(progress.clone()));
            let req = tidy_rules::planner::PlanRequest {
                roots: vec![root.clone()],
                scan: ScanOptions { top_k: crate::scan_cmds::CACHED_TOP, progress: Some(progress), ..Default::default() },
                detectors,
            };
            let mut planned = tidy_rules::planner::plan_detectors(&req, &policy, &guards, header)?;
            for idx in &planned.indexes {
                let _ = tidy_scan::cache::store(idx);
            }
            let mut out = CleanupResult::default();
            if rules && !policy.rules.is_empty() {
                let _ = tx2.send(Msg::CleanupPhase("judging"));
                let txj = tx2.clone();
                let judged = crate::judge_cmds::policy_entries(&mut planned, &policy, &guards, &move |d, t| {
                    let _ = txj.send(Msg::CleanupJudging(d, t));
                });
                match judged {
                    Ok(sum) => {
                        out.judge = sum.judge_line();
                        out.warnings = sum.warnings;
                    }
                    // A missing key or unreachable judge must not block deterministic cleanup.
                    Err(e) => out.warnings.push(format!("policy rules skipped: {e:#}")),
                }
            }
            let _ = tx2.send(Msg::CleanupPhase("saving"));
            store::save(&planned.plan)?;
            let c = planned.plan.counts();
            out.plan = planned.plan.header.id.clone();
            out.actions = c.total;
            out.reclaimable = c.reclaimable;
            out.blocked = planned.blocked.len();
            out.blocked_bytes = planned.blocked.iter().map(|b| b.bytes).sum();
            out.elapsed_ms = started.elapsed().as_millis() as u64;
            Ok(out)
        })()
        .map_err(err);
        let _ = tx.send(Msg::CleanupDone(result));
    });
}

/// Dry run, apply or undo a plan, streaming each event.
pub fn exec(tx: Sender<Msg>, plan_id: String, op: ExecOp) {
    std::thread::spawn(move || {
        let txl = tx.clone();
        let id = plan_id.clone();
        let mut on = move |ev: tidy_plan::Event<'_>| {
            let (kind, text) = crate::plan_cmds::event_text(&ev);
            let _ = txl.send(Msg::ExecLine { plan: id.clone(), kind, text });
        };
        let mut synced = tidy_scan::cache::Synced::default();
        let result = (|| -> anyhow::Result<tidy_plan::Report> {
            let (mut plan, _) = store::load(Some(&plan_id))?;
            let tracker = match op {
                ExecOp::DryRun => None,
                ExecOp::Apply => Some(sync::watch(&plan, sync::Op::Apply, None)),
                ExecOp::Undo => Some(sync::watch(&plan, sync::Op::Undo, None)),
            };
            let r = match op {
                ExecOp::Undo => tidy_plan::undo(&mut plan, None, &mut on),
                ExecOp::DryRun | ExecOp::Apply => {
                    let policy = crate::plan_cmds::plan_policy(&plan)?;
                    let guard = crate::plan_cmds::apply_guard(&policy);
                    let opts = ApplyOptions { dry_run: op == ExecOp::DryRun, only: None, verify: true };
                    tidy_plan::apply(&mut plan, &opts, &guard, &mut on)
                }
            };
            // the cached indexes catch up, so Explore needn't rescan
            if let Some(t) = tracker {
                synced = t.finish(&plan);
            }
            r
        })()
        .map_err(err);
        let _ = tx.send(Msg::ExecDone { plan: plan_id, op, result, synced });
    });
}
