//! Build a plan: scan → detectors (L1) → guards (L0) → entries.
//! Policy rules (L2) are added by the judge layer on top of this.

use crate::detect::{Finding, detect};
use crate::guards::{Block, Facts, Guards, git_roots_in_index};
use anyhow::Result;
use std::collections::HashSet;
use std::path::PathBuf;
use tidy_plan::model::*;
use tidy_plan::store;
use tidy_scan::{Index, ScanOptions};

pub struct Blocked {
    pub path: PathBuf,
    pub by: String,
    pub block: Block,
    pub bytes: u64,
}

pub struct Planned {
    pub plan: Plan,
    pub blocked: Vec<Blocked>,
    pub indexes: Vec<Index>,
}

pub struct PlanRequest {
    pub roots: Vec<PathBuf>,
    pub scan: ScanOptions,
    pub detectors: bool,
}

/// Header for a new plan.
pub fn new_header(roots: Vec<PathBuf>, mode: Mode, policy: Option<PathBuf>, policy_hash: Option<String>, judge: Option<String>) -> Header {
    Header {
        tidy_plan: PLAN_VERSION,
        id: store::new_id(),
        created: tidy_scan::fmt::now_secs(),
        roots,
        mode,
        policy,
        policy_hash,
        judge,
        note: None,
    }
}

/// Turn L1 findings into guarded plan entries.
pub fn entries_from_findings(
    idx: &Index,
    findings: &[Finding],
    guards: &Guards,
    mode: Mode,
    taken: &mut HashSet<String>,
    blocked: &mut Vec<Blocked>,
) -> Vec<Entry> {
    let mut out = Vec::new();
    for f in findings {
        let node = &idx.nodes[f.node as usize];
        let facts = Facts { mtime_max: (node.mtime_max != 0).then_some(node.mtime_max), flags: 0, regenerable: true, min_age: f.min_age };
        let git = git_roots_in_index(idx, f.node);
        if let Err(b) = guards.check(&f.path, f.action, &facts, Some(&git)) {
            blocked.push(Blocked { path: f.path.clone(), by: f.detector.to_string(), block: b, bytes: node.private });
            continue;
        }
        // Fresh, independent fingerprint (same procedure `apply` uses to verify).
        let Ok(fp) = tidy_ops::fingerprint(&f.path, None, false) else { continue };
        let id = entry_id(&f.path, f.action, taken);
        taken.insert(id.clone());
        out.push(Entry {
            id,
            status: if mode == Mode::Autonomous { Status::Approved } else { Status::Pending },
            approved_by: (mode == Mode::Autonomous).then(|| "policy".to_string()),
            action: f.action,
            path: f.path.clone(),
            dest: None,
            create_dest: false,
            on_conflict: OnConflict::Skip,
            tags: vec![],
            bytes: Bytes { logical: node.logical, alloc: node.alloc, private: node.private },
            fingerprint: fp,
            source: Source { layer: Layer::L1, rule: f.detector.to_string(), p: None },
            reason: format!("{} — {}", f.reason, f.hint),
            undo: if f.action == Action::Delete { Undo::None } else { Undo::Restore },
            alternatives: vec![],
            evidence: None,
            outcome: None,
        });
    }
    out
}

/// Scan every root and run the detectors.
pub fn plan_detectors(req: &PlanRequest, policy: &crate::policy::Policy, guards: &Guards, header: Header) -> Result<Planned> {
    let mut plan = Plan { header, entries: Vec::new() };
    let mut blocked = Vec::new();
    let mut indexes = Vec::new();
    let mut taken = HashSet::new();
    let dcfg = policy.detect_config();
    for root in &req.roots {
        let idx = tidy_scan::scan(root, &req.scan)?;
        if req.detectors && !policy.detectors.off {
            let findings = detect(&idx, &dcfg);
            let mode = policy.mode;
            plan.entries.extend(entries_from_findings(&idx, &findings, guards, mode, &mut taken, &mut blocked));
        }
        indexes.push(idx);
    }
    Ok(Planned { plan, blocked, indexes })
}
