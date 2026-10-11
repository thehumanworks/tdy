//! Review helpers shared by every view: filters, bulk status changes, and the
//! rebase-style editor text format.

use crate::model::{Action, Entry, Plan, Status, Undo};
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use tidy_scan::{fmt, path::display_path};

#[derive(Default, Clone, Debug)]
pub struct Filter {
    /// Rule id or prefix (`photos.` matches `photos.dogs`).
    pub rule: Option<String>,
    pub action: Option<Action>,
    pub min_p: Option<f64>,
    pub min_size: Option<u64>,
    pub under: Option<PathBuf>,
    pub ids: Vec<String>,
    pub status: Option<Status>,
}

impl Filter {
    pub fn is_empty(&self) -> bool {
        self.rule.is_none() && self.action.is_none() && self.min_p.is_none() && self.min_size.is_none() && self.under.is_none() && self.ids.is_empty() && self.status.is_none()
    }

    pub fn matches(&self, e: &Entry) -> bool {
        if let Some(r) = &self.rule {
            if !(e.source.rule == *r || e.source.rule.starts_with(r.as_str())) {
                return false;
            }
        }
        if self.action.is_some_and(|a| a != e.action) {
            return false;
        }
        if let Some(p) = self.min_p {
            if e.source.p.is_some_and(|x| x < p) {
                return false;
            }
        }
        if self.min_size.is_some_and(|m| e.bytes.private < m && e.bytes.logical < m) {
            return false;
        }
        if let Some(u) = &self.under {
            if !e.path.starts_with(u) {
                return false;
            }
        }
        if !self.ids.is_empty() && !self.ids.iter().any(|i| e.id.starts_with(i.as_str())) {
            return false;
        }
        if self.status.is_some_and(|s| s != e.status) {
            return false;
        }
        true
    }
}

/// Set `status` on every reviewable entry matching `f`. Returns how many changed.
pub fn set_status(plan: &mut Plan, f: &Filter, status: Status) -> usize {
    let mut n = 0;
    for e in plan.entries.iter_mut() {
        if e.status.reviewable() && f.matches(e) && e.status != status {
            e.status = status;
            n += 1;
        }
    }
    n
}

/// One-line, column-aligned description of an entry.
pub fn line(e: &Entry) -> String {
    let what = match (&e.dest, e.action) {
        (Some(d), Action::Move | Action::Copy | Action::Rename) => format!("{} → {}", display_path(&e.path), display_path(d)),
        _ if e.action == Action::Tag => format!("{} +{}", display_path(&e.path), e.tags.join(",")),
        _ => display_path(&e.path),
    };
    let size = if e.bytes.private > 0 {
        fmt::bytes(e.bytes.private)
    } else if e.bytes.logical > 0 {
        fmt::bytes(e.bytes.logical)
    } else {
        String::new()
    };
    let undo = match e.undo {
        Undo::Restore => "undo ✓",
        Undo::None => "undo ✗",
    };
    let alt = e
        .alternatives
        .first()
        .map(|a| format!(" · alt {}{}", a.rule, a.p.map(|p| format!(" {p:.2}")).unwrap_or_default()))
        .unwrap_or_default();
    format!("{:<10} {:<5} {:<10} {}  {:>8}  {}{}  {}", e.status.as_str(), e.id, e.action.as_str(), what, size, e.provenance(), alt, undo)
}

pub fn render_editor(plan: &Plan) -> String {
    let c = plan.counts();
    let h = &plan.header;
    let mut s = String::new();
    s.push_str(&format!(
        "# tidy plan {} · created {} ago · mode {:?}{}{}\n",
        h.id,
        fmt::age(fmt::now_secs() - h.created),
        h.mode,
        h.policy.as_ref().map(|p| format!(" · policy {}", display_path(p))).unwrap_or_default(),
        h.judge.as_ref().map(|j| format!(" · judge {j}")).unwrap_or_default()
    ));
    s.push_str(&format!(
        "# {} actions · {} reclaimable · {} pending · {} approved · {} rejected\n#\n",
        c.total,
        fmt::bytes(c.reclaimable),
        c.pending,
        c.approved,
        c.rejected
    ));
    s.push_str("# Set the first word of a line: approve (a) · reject (r) · pending (p).\n");
    s.push_str("# Deleting a line rejects it. Only approved lines run on `tidy apply`.\n#\n");
    for e in &plan.entries {
        if e.status.reviewable() {
            s.push_str(&line(e));
        } else {
            s.push_str("# ");
            s.push_str(&line(e));
        }
        s.push('\n');
    }
    s
}

/// Apply an edited buffer. Returns (approved, rejected, pending) counts.
pub fn parse_editor(plan: &mut Plan, text: &str) -> Result<(usize, usize, usize)> {
    let mut seen: HashMap<&str, Status> = HashMap::new();
    for (ln, raw) in text.lines().enumerate() {
        let l = raw.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let mut it = l.split_whitespace();
        let verb = it.next().unwrap_or("");
        let id = it.next().unwrap_or("");
        let st = match verb {
            "a" | "approve" | "approved" => Status::Approved,
            "r" | "reject" | "rejected" | "d" | "drop" => Status::Rejected,
            "p" | "pending" => Status::Pending,
            _ => bail!("line {}: unknown command `{verb}` (use approve/reject/pending)", ln + 1),
        };
        if !plan.entries.iter().any(|e| e.id == id) {
            bail!("line {}: unknown entry `{id}`", ln + 1);
        }
        seen.insert(plan.entries.iter().find(|e| e.id == id).map(|e| e.id.as_str()).unwrap(), st);
    }
    let seen: HashMap<String, Status> = seen.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    let (mut a, mut r, mut p) = (0, 0, 0);
    for e in plan.entries.iter_mut() {
        if !e.status.reviewable() {
            continue;
        }
        let st = seen.get(&e.id).copied().unwrap_or(Status::Rejected);
        e.status = st;
        match st {
            Status::Approved => a += 1,
            Status::Rejected => r += 1,
            _ => p += 1,
        }
    }
    Ok((a, r, p))
}
