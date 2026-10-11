//! Apply and undo plans.
//!
//! For every approved entry: verify the fingerprint → run the preflight
//! guard → journal `begin` (with the exact destination) → act → journal
//! `done`/`fail`. Same-volume moves are single renames; cross-volume moves
//! copy, verify, journal `copied`, then remove the source. `recover` uses
//! the journal plus the filesystem to settle any entry left mid-flight.

use crate::journal::{self, Journal, Phase};
use crate::model::{Action, Entry, OnConflict, Outcome, Plan, Status, Undo};
use crate::store;
use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tidy_ops::fsops::{copy_tree, remove_tree, rename_noreplace, same_content, unique_in};
use tidy_ops::{trash, verify, Drift};

#[derive(Default, Clone)]
pub struct ApplyOptions {
    pub dry_run: bool,
    /// Restrict to these entry ids.
    pub only: Option<HashSet<String>>,
    /// Re-fingerprint before acting (on by default in the CLI).
    pub verify: bool,
}

#[derive(Debug)]
pub enum Event<'a> {
    Ok(&'a Entry, String),
    Skip(&'a Entry, String),
    Fail(&'a Entry, String),
    WouldDo(&'a Entry, String),
    Note(String),
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Report {
    pub applied: usize,
    pub skipped: usize,
    pub failed: usize,
    pub would: usize,
    /// Bytes released immediately (deletes, archives, dedupes).
    pub freed: u64,
    /// Bytes moved to the trash or quarantine (freed once emptied/purged).
    pub parked: u64,
    pub undone: usize,
}

/// Apply-time guard: return `Err(reason)` to refuse an entry.
pub type Preflight<'a> = &'a dyn Fn(&Entry) -> std::result::Result<(), String>;

pub fn apply(plan: &mut Plan, opts: &ApplyOptions, preflight: Preflight<'_>, on: &mut dyn FnMut(Event<'_>)) -> Result<Report> {
    let _lock = store::lock(&plan.header.id)?;
    for m in recover(plan)? {
        on(Event::Note(m));
    }
    let mut journal = if opts.dry_run { None } else { Some(Journal::open(&plan.header.id)?) };
    let mut report = Report::default();
    let plan_id = plan.header.id.clone();
    let mut since_save = 0usize;
    for i in 0..plan.entries.len() {
        let e = &plan.entries[i];
        if e.status != Status::Approved {
            continue;
        }
        if let Some(only) = &opts.only {
            if !only.contains(&e.id) {
                continue;
            }
        }
        // 1. fingerprint
        if opts.verify {
            if let Err(d) = verify(&e.path, &e.fingerprint) {
                let msg = d.to_string();
                if !opts.dry_run {
                    let e = &mut plan.entries[i];
                    e.status = Status::Skipped;
                    e.outcome = Some(Outcome { at: now(), error: Some(msg.clone()), ..Default::default() });
                }
                report.skipped += 1;
                on(Event::Skip(&plan.entries[i], msg));
                if matches!(d, Drift::Gone) {
                    continue;
                }
                continue;
            }
        }
        // 2. guards
        if let Err(why) = preflight(e) {
            if !opts.dry_run {
                let e = &mut plan.entries[i];
                e.status = Status::Skipped;
                e.outcome = Some(Outcome { at: now(), error: Some(format!("guard: {why}")), ..Default::default() });
            }
            report.skipped += 1;
            on(Event::Skip(&plan.entries[i], format!("guard: {why}")));
            continue;
        }
        // 3. destination planning (also validates dry runs)
        let target = match resolve_target(e, &plan_id, !opts.dry_run) {
            Ok(t) => t,
            Err(TargetErr::Skip(why)) => {
                if !opts.dry_run {
                    let e = &mut plan.entries[i];
                    e.status = Status::Skipped;
                    e.outcome = Some(Outcome { at: now(), error: Some(why.clone()), ..Default::default() });
                }
                report.skipped += 1;
                on(Event::Skip(&plan.entries[i], why));
                continue;
            }
            Err(TargetErr::Fail(why)) => {
                if !opts.dry_run {
                    let e = &mut plan.entries[i];
                    e.status = Status::Failed;
                    e.outcome = Some(Outcome { at: now(), error: Some(why.clone()), ..Default::default() });
                }
                report.failed += 1;
                on(Event::Fail(&plan.entries[i], why));
                continue;
            }
        };
        if opts.dry_run {
            report.would += 1;
            on(Event::WouldDo(&plan.entries[i], describe(e, &target)));
            continue;
        }
        // 4. journal + act
        let j = journal.as_mut().expect("journal");
        let action = e.action.as_str();
        j.log(&e.id, Phase::Begin, action, &e.path, target.dst(), None)?;
        let res = execute(e, &target, j);
        let e = &mut plan.entries[i];
        match res {
            Ok(outcome) => {
                j.log(&e.id, Phase::Done, action, &e.path, outcome.now_at.as_deref(), None)?;
                match e.action {
                    Action::Trash | Action::Quarantine => report.parked += e.bytes.private,
                    _ => report.freed += outcome.freed,
                }
                e.status = Status::Applied;
                e.outcome = Some(outcome);
                report.applied += 1;
                let msg = describe_done(e);
                on(Event::Ok(&plan.entries[i], msg));
            }
            Err(err) => {
                let msg = format!("{err:#}");
                j.log(&e.id, Phase::Fail, action, &e.path, None, Some(msg.clone()))?;
                e.status = Status::Failed;
                e.outcome = Some(Outcome { at: now(), error: Some(msg.clone()), ..Default::default() });
                report.failed += 1;
                on(Event::Fail(&plan.entries[i], msg));
            }
        }
        since_save += 1;
        if since_save >= 64 {
            store::save(plan)?;
            since_save = 0;
        }
    }
    if !opts.dry_run {
        store::save(plan)?;
    }
    Ok(report)
}

/// Where an entry's item will end up.
enum Target {
    None,
    Trash(trash::Slot),
    Path(PathBuf),
    /// Destination already holds identical content: park the source.
    DuplicateOf(PathBuf, PathBuf),
}

impl Target {
    fn dst(&self) -> Option<&Path> {
        match self {
            Target::None => None,
            Target::Trash(s) => Some(&s.dst),
            Target::Path(p) | Target::DuplicateOf(_, p) => Some(p),
        }
    }
}

enum TargetErr {
    Skip(String),
    Fail(String),
}

fn resolve_target(e: &Entry, plan_id: &str, reserve: bool) -> std::result::Result<Target, TargetErr> {
    match e.action {
        Action::Delete => Ok(Target::None),
        Action::Trash => trash::trash_slot(&e.path, reserve).map(Target::Trash).map_err(|x| TargetErr::Fail(x.to_string())),
        Action::Quarantine => trash::quarantine_slot(&e.path, plan_id, &e.id, reserve).map(Target::Path).map_err(|x| TargetErr::Fail(x.to_string())),
        Action::Move | Action::Rename | Action::Copy => {
            let dest = e.dest.clone().ok_or_else(|| TargetErr::Fail("no destination".into()))?;
            let parent = dest.parent().ok_or_else(|| TargetErr::Fail("destination has no parent".into()))?;
            if !parent.exists() && !e.create_dest {
                return Err(TargetErr::Skip(format!("destination folder {} does not exist", parent.display())));
            }
            if dest.starts_with(&e.path) {
                return Err(TargetErr::Fail("destination is inside the source".into()));
            }
            if std::fs::symlink_metadata(&dest).is_ok() {
                return match e.on_conflict {
                    OnConflict::Skip => Err(TargetErr::Skip(format!("{} already exists", dest.display()))),
                    OnConflict::Suffix => Ok(Target::Path(unique_in(parent, dest.file_name().unwrap_or_default()))),
                    OnConflict::DedupeElseSuffix => {
                        if same_content(&e.path, &dest).unwrap_or(false) {
                            let q = trash::quarantine_slot(&e.path, plan_id, &e.id, reserve).map_err(|x| TargetErr::Fail(x.to_string()))?;
                            Ok(Target::DuplicateOf(dest, q))
                        } else {
                            Ok(Target::Path(unique_in(parent, dest.file_name().unwrap_or_default())))
                        }
                    }
                };
            }
            Ok(Target::Path(dest))
        }
        a => Err(TargetErr::Fail(format!("action `{}` is not supported by this version", a.as_str()))),
    }
}

fn execute(e: &Entry, t: &Target, j: &mut Journal) -> Result<Outcome> {
    let mut out = Outcome { at: now(), ..Default::default() };
    match (e.action, t) {
        (Action::Delete, _) => {
            remove_tree(&e.path)?;
            out.freed = e.bytes.private;
        }
        (Action::Trash, Target::Trash(slot)) => {
            trash::commit(&e.path, slot)?;
            out.now_at = Some(slot.dst.clone());
        }
        (Action::Quarantine, Target::Path(dst)) => {
            rename_noreplace(&e.path, dst)?;
            out.now_at = Some(dst.clone());
        }
        (Action::Move | Action::Rename, Target::DuplicateOf(_, q)) => {
            rename_noreplace(&e.path, q)?;
            out.now_at = Some(q.clone());
        }
        (Action::Move | Action::Rename, Target::Path(dst)) => {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            move_any(&e.path, dst, e, j)?;
            out.now_at = Some(dst.clone());
        }
        (Action::Copy, Target::Path(dst)) => {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            copy_verified(&e.path, dst)?;
            out.now_at = Some(dst.clone());
        }
        (a, _) => return Err(anyhow!("action `{}` is not supported by this version", a.as_str())),
    }
    Ok(out)
}

/// Rename when on the same volume; otherwise copy → verify → journal
/// `copied` → remove source.
fn move_any(src: &Path, dst: &Path, e: &Entry, j: &mut Journal) -> Result<()> {
    match rename_noreplace(src, dst) {
        Ok(()) => Ok(()),
        Err(err) if err.raw_os_error() == Some(libc::EXDEV) => {
            copy_verified(src, dst)?;
            j.log(&e.id, Phase::Copied, e.action.as_str(), src, Some(dst), None)?;
            remove_tree(src)?;
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

fn copy_verified(src: &Path, dst: &Path) -> Result<()> {
    let partial = partial_path(dst);
    let _ = remove_tree(&partial);
    copy_tree(src, &partial)?;
    let a = tidy_ops::fingerprint(src, None, true)?;
    let b = tidy_ops::fingerprint(&partial, None, true)?;
    if a.size != b.size || a.files != b.files || a.hash != b.hash {
        let _ = remove_tree(&partial);
        return Err(anyhow!("copy verification failed for {}", src.display()));
    }
    rename_noreplace(&partial, dst)?;
    Ok(())
}

fn partial_path(dst: &Path) -> PathBuf {
    let mut n = dst.file_name().unwrap_or_default().to_os_string();
    n.push(".tidy-partial");
    dst.with_file_name(n)
}

fn describe(e: &Entry, t: &Target) -> String {
    match t {
        Target::None => format!("{} {}", e.action.as_str(), e.path.display()),
        Target::DuplicateOf(d, _) => format!("{} is identical to {} → quarantine", e.path.display(), d.display()),
        t => format!("{} {} → {}", e.action.as_str(), e.path.display(), t.dst().map(|p| p.display().to_string()).unwrap_or_default()),
    }
}

fn describe_done(e: &Entry) -> String {
    match e.outcome.as_ref().and_then(|o| o.now_at.as_ref()) {
        Some(p) => format!("{} → {}", e.action.as_str(), p.display()),
        None => e.action.as_str().to_string(),
    }
}

// ---- undo ------------------------------------------------------------------

pub fn undo(plan: &mut Plan, only: Option<&HashSet<String>>, on: &mut dyn FnMut(Event<'_>)) -> Result<Report> {
    let _lock = store::lock(&plan.header.id)?;
    for m in recover(plan)? {
        on(Event::Note(m));
    }
    let mut j = Journal::open(&plan.header.id)?;
    let mut report = Report::default();
    for i in (0..plan.entries.len()).rev() {
        let e = &plan.entries[i];
        if e.status != Status::Applied || only.is_some_and(|o| !o.contains(&e.id)) {
            continue;
        }
        if e.undo == Undo::None {
            report.skipped += 1;
            on(Event::Skip(e, format!("{} is permanent", e.action.as_str())));
            continue;
        }
        let Some(now_at) = e.outcome.as_ref().and_then(|o| o.now_at.clone()) else {
            report.skipped += 1;
            on(Event::Skip(e, "no record of where it went".into()));
            continue;
        };
        let action = e.action.as_str();
        j.log(&e.id, Phase::UndoBegin, action, &now_at, Some(&e.path), None)?;
        let res: Result<()> = (|| {
            match e.action {
                Action::Copy => remove_tree(&now_at)?,
                _ => {
                    if let Some(parent) = e.path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    match rename_noreplace(&now_at, &e.path) {
                        Ok(()) => {}
                        Err(err) if err.raw_os_error() == Some(libc::EXDEV) => {
                            copy_verified(&now_at, &e.path)?;
                            remove_tree(&now_at)?;
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                            return Err(anyhow!("{} exists again; not overwriting", e.path.display()));
                        }
                        Err(err) => return Err(err.into()),
                    }
                    if e.action == Action::Trash {
                        trash::forget_trashinfo(&now_at);
                    }
                }
            }
            Ok(())
        })();
        let e = &mut plan.entries[i];
        match res {
            Ok(()) => {
                j.log(&e.id, Phase::UndoDone, action, &now_at, Some(&e.path), None)?;
                e.status = Status::Undone;
                report.undone += 1;
                on(Event::Ok(&plan.entries[i], format!("restored {}", plan.entries[i].path.display())));
            }
            Err(err) => {
                let msg = format!("{err:#}");
                j.log(&e.id, Phase::UndoFail, action, &now_at, Some(&e.path), Some(msg.clone()))?;
                report.failed += 1;
                on(Event::Fail(&plan.entries[i], msg));
            }
        }
    }
    store::save(plan)?;
    Ok(report)
}

// ---- recovery ----------------------------------------------------------------

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// Reconcile entries whose last journal record is an unfinished phase.
/// Returns human-readable notes for anything it changed.
pub fn recover(plan: &mut Plan) -> Result<Vec<String>> {
    let recs = journal::read(&plan.header.id)?;
    if recs.is_empty() {
        return Ok(Vec::new());
    }
    let mut last: HashMap<&str, &journal::Record> = HashMap::new();
    for r in &recs {
        last.insert(r.entry.as_str(), r);
    }
    let mut notes = Vec::new();
    let mut j: Option<Journal> = None;
    let mut changed = false;
    for e in plan.entries.iter_mut() {
        let Some(r) = last.get(e.id.as_str()) else { continue };
        let note = match r.phase {
            Phase::Begin => {
                let to = r.to.as_deref();
                match (e.action, to) {
                    (Action::Delete, _) => {
                        if exists(&e.path) {
                            e.status = Status::Failed;
                            e.outcome = Some(Outcome { at: now(), error: Some("interrupted while deleting; partially removed".into()), ..Default::default() });
                            "interrupted delete left a partial tree".to_string()
                        } else {
                            e.status = Status::Applied;
                            e.outcome = Some(Outcome { at: now(), freed: e.bytes.private, ..Default::default() });
                            "delete had completed".to_string()
                        }
                    }
                    (_, Some(to)) => {
                        let partial = partial_path(to);
                        if exists(&partial) {
                            let _ = remove_tree(&partial);
                        }
                        match (exists(&e.path), exists(to)) {
                            (false, true) => {
                                e.status = Status::Applied;
                                e.outcome = Some(Outcome { at: now(), now_at: Some(to.to_path_buf()), ..Default::default() });
                                "had completed".to_string()
                            }
                            (true, false) => {
                                e.status = Status::Approved;
                                "had not started; will retry".to_string()
                            }
                            _ => {
                                e.status = Status::Failed;
                                e.outcome = Some(Outcome { at: now(), error: Some("interrupted; both source and destination exist".into()), ..Default::default() });
                                "interrupted with both copies present".to_string()
                            }
                        }
                    }
                    _ => {
                        e.status = Status::Failed;
                        "interrupted".to_string()
                    }
                }
            }
            Phase::Copied => {
                // Copy is complete and verified; finish removing the source.
                if exists(&e.path) {
                    remove_tree(&e.path)?;
                }
                e.status = Status::Applied;
                e.outcome = Some(Outcome { at: now(), now_at: r.to.clone(), ..Default::default() });
                "finished a cross-volume move".to_string()
            }
            Phase::UndoBegin => {
                let now_at = &r.from;
                if exists(&e.path) && !exists(now_at) {
                    e.status = Status::Undone;
                    "undo had completed".to_string()
                } else {
                    e.status = Status::Applied;
                    "undo had not completed".to_string()
                }
            }
            // Terminal phases the plan file missed (crash between journal and save).
            Phase::Done if e.status != Status::Applied => {
                e.status = Status::Applied;
                let freed = if e.action == Action::Delete { e.bytes.private } else { 0 };
                e.outcome = Some(Outcome { at: r.ts, now_at: r.to.clone(), freed, ..Default::default() });
                "completed before the plan was saved".to_string()
            }
            Phase::Fail if e.status != Status::Failed => {
                e.status = Status::Failed;
                e.outcome = Some(Outcome { at: r.ts, error: r.note.clone(), ..Default::default() });
                "failed before the plan was saved".to_string()
            }
            Phase::UndoDone if e.status != Status::Undone => {
                e.status = Status::Undone;
                "undo completed before the plan was saved".to_string()
            }
            _ => continue,
        };
        let jj = match j.as_mut() {
            Some(x) => x,
            None => j.insert(Journal::open(&plan.header.id)?),
        };
        jj.log(&e.id, Phase::Recovered, e.action.as_str(), &e.path, r.to.as_deref(), Some(note.clone()))?;
        notes.push(format!("recovered {} ({}): {}", e.id, e.path.display(), note));
        changed = true;
    }
    if changed {
        store::save(plan)?;
    }
    Ok(notes)
}

fn now() -> i64 {
    tidy_scan::fmt::now_secs()
}
