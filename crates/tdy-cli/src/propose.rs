//! Turn explicit requests ("trash these") into a plan: shared by MCP agents
//! (entries start pending) and the TUI's marks (approved by the human who
//! marked them). Guard refusals are retained for explicit human review.

use anyhow::Result;
use std::collections::HashSet;
use std::path::PathBuf;
use tdy_plan::model::{Bytes, Entry, Layer, OnConflict, Source, Undo, entry_id};
use tdy_plan::{Action, Mode, Plan, Status, store};
use tdy_rules::guards::{Facts, Guards};

pub struct Proposal {
    pub action: Action,
    /// Canonical path.
    pub path: PathBuf,
    pub dest: Option<PathBuf>,
    pub tags: Vec<String>,
    pub reason: String,
    /// Sizes when the caller knows them (e.g. from a scan); otherwise the
    /// fingerprint's apparent size is used.
    pub bytes: Option<Bytes>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum By {
    /// Proposed by an agent: pending until a human approves.
    Agent,
    /// Marked by the human at the keyboard: approved.
    Human,
}

pub struct Proposed {
    /// The saved plan, including guard refusals. None when no valid action could be built.
    pub plan: Option<Plan>,
    pub rejected: Vec<(PathBuf, String)>,
}

pub fn propose(items: Vec<Proposal>, by: By, note: &str) -> Result<Proposed> {
    let (policy, hash, ppath) = crate::plan_cmds::load_policy(None)?;
    let guards = Guards::new(policy.guard_config());
    let mut plan = Plan { header: tdy_rules::planner::new_header(vec![], Mode::Manual, ppath, hash, None), entries: vec![] };
    plan.header.note = Some(note.to_string());
    let mut taken = HashSet::new();
    let mut rejected = Vec::new();
    for it in items {
        if matches!(it.action, Action::Dedupe | Action::Compress) {
            rejected.push((it.path, "use `tdy dupes` / `tdy archive` for dedupe and compression".into()));
            continue;
        }
        if matches!(it.action, Action::Move | Action::Copy | Action::Rename) && it.dest.is_none() {
            rejected.push((it.path, "needs a destination".into()));
            continue;
        }
        let fp = match tdy_ops::fingerprint(&it.path, None, false) {
            Ok(fp) => fp,
            Err(e) => {
                rejected.push((it.path, format!("cannot read: {e}")));
                continue;
            }
        };
        let facts = Facts { mtime_max: Some(fp.mtime), flags: 0, regenerable: false, min_age: None };
        let refusal = guards.check(&it.path, it.action, &facts, None).err().map(|b| b.to_string());
        if let Some(why) = &refusal {
            rejected.push((it.path.clone(), why.clone()));
        }
        let id = entry_id(&it.path, it.action, &taken);
        taken.insert(id.clone());
        let bytes = it.bytes.unwrap_or(Bytes { logical: fp.size, alloc: fp.size, private: fp.size });
        plan.entries.push(Entry {
            id,
            status: if refusal.is_some() {
                Status::Rejected
            } else if by == By::Human {
                Status::Approved
            } else {
                Status::Pending
            },
            approved_by: (by == By::Human && refusal.is_none()).then(|| "human".to_string()),
            refusal,
            override_refusal: false,
            action: it.action,
            path: it.path,
            dest: it.dest,
            create_dest: true,
            on_conflict: OnConflict::Suffix,
            tags: it.tags,
            bytes,
            fingerprint: fp,
            source: Source { layer: Layer::User, rule: if by == By::Human { "manual" } else { "agent" }.into(), p: None },
            reason: it.reason,
            undo: if it.action == Action::Delete { Undo::None } else { Undo::Restore },
            alternatives: vec![],
            evidence: None,
            outcome: None,
        });
    }
    if plan.entries.is_empty() {
        return Ok(Proposed { plan: None, rejected });
    }
    plan.header.roots =
        plan.entries.iter().filter_map(|e| e.path.parent().map(|p| p.to_path_buf())).collect::<HashSet<_>>().into_iter().collect();
    store::save(&plan)?;
    Ok(Proposed { plan: Some(plan), rejected })
}
