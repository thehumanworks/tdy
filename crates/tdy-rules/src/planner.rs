//! Build a plan: scan → detectors (L1) → guards (L0) → entries.
//! Policy rules (L2) are added by the judge layer on top of this.

use crate::detect::{Finding, detect};
use crate::guards::{Block, Facts, Guards, git_roots_in_index};
use anyhow::Result;
use std::collections::HashSet;
use std::path::PathBuf;
use tdy_plan::model::*;
use tdy_plan::store;
use tdy_scan::{Index, ScanOptions};

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
        tdy_plan: PLAN_VERSION,
        id: store::new_id(),
        created: tdy_scan::fmt::now_secs(),
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
        let refusal = guards.check(&f.path, f.action, &facts, Some(&git)).err().map(|b| {
            let why = b.to_string();
            blocked.push(Blocked { path: f.path.clone(), by: f.detector.to_string(), block: b, bytes: node.private });
            why
        });
        // Fresh, independent fingerprint (same procedure `apply` uses to verify).
        let Ok(fp) = tdy_ops::fingerprint(&f.path, None, false) else { continue };
        let id = entry_id(&f.path, f.action, taken);
        taken.insert(id.clone());
        out.push(Entry {
            id,
            status: if refusal.is_some() {
                Status::Rejected
            } else if mode == Mode::Autonomous {
                Status::Approved
            } else {
                Status::Pending
            },
            approved_by: (refusal.is_none() && mode == Mode::Autonomous).then(|| "policy".to_string()),
            refusal,
            override_refusal: false,
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
        let idx = tdy_scan::scan(root, &req.scan)?;
        if req.detectors && !policy.detectors.off {
            let findings = detect(&idx, &dcfg);
            let mode = policy.mode;
            plan.entries.extend(entries_from_findings(&idx, &findings, guards, mode, &mut taken, &mut blocked));
        }
        indexes.push(idx);
    }
    Ok(Planned { plan, blocked, indexes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guards::GuardConfig;

    #[test]
    fn guard_refusals_are_retained_for_all_blocked_and_partial_plans() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["blocked", "allowed"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
            std::fs::write(dir.path().join(name).join("data"), b"candidate").unwrap();
        }
        let idx = tdy_scan::scan(dir.path(), &ScanOptions::default()).unwrap();
        let findings: Vec<_> = idx
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(n, _)| {
                let path = idx.path(n as u32);
                matches!(path.file_name().and_then(|n| n.to_str()), Some("blocked" | "allowed")).then(|| Finding {
                    node: n as u32,
                    path,
                    detector: "test",
                    action: Action::Delete,
                    reason: "test artifact".into(),
                    hint: "regenerable",
                    min_age: None,
                })
            })
            .collect();
        assert_eq!(findings.len(), 2);
        for (protect, expected) in [(dir.path().to_path_buf(), 2), (dir.path().join("blocked"), 1)] {
            let guards = Guards::new(GuardConfig { min_age: 0, protect: vec![protect], ..Default::default() });
            let mut blocked = Vec::new();
            let entries = entries_from_findings(&idx, &findings, &guards, Mode::Autonomous, &mut HashSet::new(), &mut blocked);
            assert_eq!(entries.len(), 2);
            assert_eq!(blocked.len(), expected);
            assert_eq!(entries.iter().filter(|e| e.status == Status::Rejected).count(), expected);
            for e in entries {
                assert!(!e.override_refusal);
                if e.refusal.is_some() {
                    assert!(e.approved_by.is_none());
                } else {
                    assert_eq!(e.status, Status::Approved);
                }
            }
        }
    }
}
