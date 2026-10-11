//! The plan file model. A plan is JSONL: one header line, then one entry per
//! line. It is the single source of truth that every view (CLI, editor, TUI,
//! MCP) reads and writes.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tidy_ops::Fingerprint;

pub const PLAN_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Manual,
    Autonomous,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Header {
    pub tidy_plan: u32,
    pub id: String,
    pub created: i64,
    pub roots: Vec<PathBuf>,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Approved,
    Rejected,
    Applied,
    Failed,
    /// Not applied because the path changed since planning (or vanished).
    Skipped,
    Undone,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Approved => "approve",
            Status::Rejected => "reject",
            Status::Applied => "applied",
            Status::Failed => "failed",
            Status::Skipped => "skipped",
            Status::Undone => "undone",
        }
    }
    /// Whether review may still change this entry.
    pub fn reviewable(self) -> bool {
        matches!(self, Status::Pending | Status::Approved | Status::Rejected)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Permanent removal.
    Delete,
    /// OS trash (restorable by the OS and by `tidy undo`).
    Trash,
    /// Tidy's own holding area on the same volume, purged after a TTL.
    Quarantine,
    Move,
    Copy,
    Rename,
    Tag,
    /// Replace a duplicate with a copy-on-write clone of the original.
    Dedupe,
    /// Transparent filesystem compression in place.
    Compress,
    /// Pack a directory into a `.tar.zst` next to it, then remove it.
    Archive,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Delete => "delete",
            Action::Trash => "trash",
            Action::Quarantine => "quarantine",
            Action::Move => "move",
            Action::Copy => "copy",
            Action::Rename => "rename",
            Action::Tag => "tag",
            Action::Dedupe => "dedupe",
            Action::Compress => "compress",
            Action::Archive => "archive",
        }
    }
    /// Only one exclusive action can apply to a path (first match wins).
    pub fn exclusive(self) -> bool {
        !matches!(self, Action::Tag | Action::Copy)
    }
    pub fn destructive(self) -> bool {
        matches!(self, Action::Delete | Action::Trash | Action::Quarantine | Action::Move | Action::Rename | Action::Archive | Action::Dedupe)
    }
}

impl std::str::FromStr for Action {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Ok(match s {
            "delete" => Action::Delete,
            "trash" => Action::Trash,
            "quarantine" => Action::Quarantine,
            "move" => Action::Move,
            "copy" => Action::Copy,
            "rename" => Action::Rename,
            "tag" => Action::Tag,
            "dedupe" => Action::Dedupe,
            "compress" => Action::Compress,
            "archive" => Action::Archive,
            _ => return Err(format!("unknown action `{s}`")),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Undo {
    /// Can be reversed by `tidy undo`.
    Restore,
    /// Cannot be reversed (permanent delete).
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Layer {
    /// Deterministic detector.
    L1,
    /// Decision model under a natural-language policy.
    L2,
    /// Added by a person or an agent directly.
    User,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Bytes {
    pub logical: u64,
    pub alloc: u64,
    pub private: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    pub layer: Layer,
    pub rule: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p: Option<f64>,
}

/// A runner-up decision, selectable during review.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Alt {
    pub rule: String,
    pub action: Action,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum OnConflict {
    /// Leave the source where it is.
    #[default]
    Skip,
    /// Append " 2", " 3"… to the destination name.
    Suffix,
    /// If the destination is identical, drop the source (to quarantine); else suffix.
    DedupeElseSuffix,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Outcome {
    pub at: i64,
    /// Where the item now lives (trash/quarantine/destination), for undo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now_at: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Previous value for reversible metadata actions (tags).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub freed: u64,
    /// Directories created to hold the destination (removed on undo if empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created: Vec<PathBuf>,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub status: Status,
    pub action: Action,
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub create_dest: bool,
    #[serde(default)]
    pub on_conflict: OnConflict,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub bytes: Bytes,
    pub fingerprint: Fingerprint,
    pub source: Source,
    pub reason: String,
    pub undo: Undo,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternatives: Vec<Alt>,
    /// Raw judge answers, for `tidy explain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
}

impl Entry {
    /// Short label for where this came from: rule id plus probability.
    pub fn provenance(&self) -> String {
        match (self.source.layer, self.source.p) {
            (Layer::L1, _) => format!("{} (L1)", self.source.rule),
            (_, Some(p)) => format!("{} {:.2}", self.source.rule, p),
            _ => self.source.rule.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub header: Header,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Counts {
    pub total: usize,
    pub pending: usize,
    pub approved: usize,
    pub rejected: usize,
    pub applied: usize,
    pub failed: usize,
    pub skipped: usize,
    pub undone: usize,
    /// Bytes (private) of entries that would free space when applied.
    pub reclaimable: u64,
}

impl Plan {
    pub fn counts(&self) -> Counts {
        let mut c = Counts { total: self.entries.len(), ..Default::default() };
        for e in &self.entries {
            match e.status {
                Status::Pending => c.pending += 1,
                Status::Approved => c.approved += 1,
                Status::Rejected => c.rejected += 1,
                Status::Applied => c.applied += 1,
                Status::Failed => c.failed += 1,
                Status::Skipped => c.skipped += 1,
                Status::Undone => c.undone += 1,
            }
            if matches!(e.action, Action::Delete | Action::Trash | Action::Quarantine | Action::Archive | Action::Dedupe)
                && e.status.reviewable()
                && e.status != Status::Rejected
            {
                c.reclaimable += e.bytes.private;
            }
        }
        c
    }

    pub fn entry_mut(&mut self, id: &str) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }
}

/// Stable short id for an entry: 4+ hex chars of BLAKE3(path ‖ action),
/// lengthened on collision.
pub fn entry_id(path: &std::path::Path, action: Action, taken: &std::collections::HashSet<String>) -> String {
    let mut h = blake3::Hasher::new();
    h.update(tidy_scan::path::path_bytes(path));
    h.update(action.as_str().as_bytes());
    let hex = h.finalize().to_hex();
    for len in 4..=16 {
        let id = hex[..len].to_string();
        if !taken.contains(&id) {
            return id;
        }
    }
    hex[..32].to_string()
}
