//! Explore: an ncdu-style browser over the directory index. Folders come
//! from the scan; files are listed on demand for the folder on screen, so
//! memory stays proportional to the number of folders.

use super::newplan::{Kind as PlanKind, Start};
use super::worker::{self, ProposeResult, Scanned, Tag};
use super::{Act, Confirm, Ctx, Dialog, accent, bold, dim, fg, plural, spinner};
use crate::judge_cmds::RulesExplanation;
use crate::plan_cmds::{Explanation, Known};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row as TRow, Table, TableState, Wrap};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tdy_plan::{Action, Bytes};
use tdy_scan::index::node_flags;
use tdy_scan::path::display_path;
use tdy_scan::{Index, Kind, ReadOptions, ScanProgress, SizeKind, fmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    Size,
    Name,
    /// Least recently changed first: cold things to the top.
    Age,
    Items,
}

impl Sort {
    fn next(self) -> Self {
        match self {
            Sort::Size => Sort::Name,
            Sort::Name => Sort::Age,
            Sort::Age => Sort::Items,
            Sort::Items => Sort::Size,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Sort::Size => "size",
            Sort::Name => "name",
            Sort::Age => "oldest",
            Sort::Items => "items",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    Dir(u32),
    File,
    Link,
    Other,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub kind: RowKind,
    pub name: String,
    pub path: PathBuf,
    pub bytes: Bytes,
    /// Files inside (folders) or 1.
    pub items: u64,
    pub mtime: i64,
    pub tag: Option<&'static str>,
    pub flags: u16,
}

impl Row {
    fn size(&self, by: SizeKind) -> u64 {
        match by {
            SizeKind::Logical => self.bytes.logical,
            SizeKind::Alloc => self.bytes.alloc,
            SizeKind::Private => self.bytes.private,
        }
    }
    fn is_dir(&self) -> bool {
        matches!(self.kind, RowKind::Dir(_))
    }
}

pub struct Explore {
    pub root: PathBuf,
    pub index: Option<Index>,
    pub tags: HashMap<u32, Tag>,
    pub scanning: Option<(Arc<ScanProgress>, Instant)>,
    seq: u64,
    pub error: Option<String>,
    pub cwd: u32,
    pub rows: Vec<Row>,
    pub state: TableState,
    remember: HashMap<u32, usize>,
    pub sort: Sort,
    pub by: SizeKind,
    pub marks: BTreeMap<PathBuf, Bytes>,
    pub filter: String,
    pub typing: bool,
    pub info: bool,
    explained: HashMap<PathBuf, Result<Explanation, String>>,
    explaining: HashSet<PathBuf>,
    rules: HashMap<PathBuf, Result<Option<RulesExplanation>, String>>,
    ruling: HashSet<PathBuf>,
    /// Something was applied since the scan, and the index couldn't be
    /// brought up to date.
    pub stale: bool,
    /// The index on screen came from the cache: the folder whose scan it was.
    pub cached: Option<PathBuf>,
    /// The index on screen was patched after an apply or undo.
    pub patched: bool,
    /// Waiting for the cached index after an apply or undo.
    reloading: bool,
    /// Files of the folder on screen, read once per visit (filtering and
    /// sorting reuse them).
    files: Option<(u32, Vec<Row>)>,
}

impl Explore {
    pub fn new(root: PathBuf) -> Self {
        Explore {
            root,
            index: None,
            tags: HashMap::new(),
            scanning: None,
            seq: 0,
            error: None,
            cwd: 0,
            rows: vec![],
            state: TableState::default(),
            remember: HashMap::new(),
            sort: Sort::Size,
            by: SizeKind::Alloc,
            marks: BTreeMap::new(),
            filter: String::new(),
            typing: false,
            info: false,
            explained: HashMap::new(),
            explaining: HashSet::new(),
            rules: HashMap::new(),
            ruling: HashSet::new(),
            stale: false,
            cached: None,
            patched: false,
            reloading: false,
            files: None,
        }
    }

    /// First look: the cached index at once (if there is one), then a scan.
    pub fn open(&mut self, ctx: &Ctx) {
        self.scan_with(ctx, true);
    }

    pub fn start_scan(&mut self, ctx: &Ctx) {
        self.scan_with(ctx, false);
    }

    fn scan_with(&mut self, ctx: &Ctx, cached: bool) {
        if let Some((p, _)) = &self.scanning {
            p.cancel.store(true, Ordering::Relaxed);
        }
        self.seq += 1;
        self.error = None;
        self.reloading = false;
        let progress = Arc::new(ScanProgress::default());
        self.scanning = Some((progress.clone(), Instant::now()));
        worker::scan(ctx.tx.clone(), self.seq, self.root.clone(), ctx.policy.clone(), progress, cached);
    }

    /// Put a new index on screen, staying in the same folder and row
    /// (ids change between indexes).
    fn install(&mut self, s: Scanned) {
        let here = self.index.as_ref().map(|i| i.path(self.cwd));
        let row = self.selected().map(|r| r.path.clone());
        self.remember.clear();
        self.cwd = here.and_then(|p| s.index.lookup(&p)).unwrap_or(0);
        self.tags = s.tags;
        self.cached = s.cached_from;
        self.index = Some(s.index);
        self.files = None;
        self.stale = false;
        self.patched = false;
        self.explained.clear();
        self.rules.clear();
        // marks that no longer exist are dropped
        self.marks.retain(|p, _| p.symlink_metadata().is_ok());
        self.rebuild();
        if let Some(r) = row {
            self.select_path(&r);
        }
    }

    pub fn on_scanned(&mut self, seq: u64, result: Result<Scanned, String>) -> Act {
        if seq != self.seq {
            return Act::None;
        }
        self.scanning = None;
        match result {
            Ok(s) => {
                self.install(s);
                Act::None
            }
            // a cached index stays on screen
            Err(e) if self.index.is_some() => Act::Toast(format!("{e} — showing the cached index")),
            Err(e) => {
                self.error = Some(e.clone());
                Act::Toast(e)
            }
        }
    }

    pub fn on_cached(&mut self, seq: u64, scanned: Option<Scanned>, ctx: &Ctx) -> Act {
        if seq != self.seq {
            return Act::None;
        }
        let reloading = std::mem::take(&mut self.reloading);
        match scanned {
            // never replace what's on screen with something older
            Some(s) if self.index.as_ref().is_none_or(|i| i.stats.started_ms <= s.index.stats.started_ms) => {
                self.install(s);
                self.patched = reloading;
            }
            // nothing usable after a change: look again
            _ if reloading => self.start_scan(ctx),
            _ => {}
        }
        Act::None
    }

    /// An apply or undo finished: show the index the cache was patched to,
    /// or rescan when that isn't possible.
    pub fn after_change(&mut self, ctx: &Ctx, synced: tdy_scan::cache::Synced) {
        let Some(idx) = &self.index else {
            self.stale = true;
            return;
        };
        let (started, finished) = (idx.stats.started_ms, idx.stats.started_ms.saturating_add(idx.stats.elapsed_ms as i64));
        if self.scanning.is_some() || (finished >= synced.started_ms && started <= synced.ended_ms) {
            // a scan that ran during the change may have seen part of it
            self.start_scan(ctx);
        } else if started > synced.ended_ms {
            // scanned after it: shows it already
        } else if synced.patched + synced.dropped > 0 {
            self.seq += 1;
            self.reloading = true;
            worker::reload(ctx.tx.clone(), self.seq, self.root.clone(), ctx.policy.clone());
        } else {
            self.stale = true;
        }
    }

    pub fn selected(&self) -> Option<&Row> {
        self.rows.get(self.state.selected()?)
    }

    fn cwd_path(&self) -> PathBuf {
        self.index.as_ref().map(|i| i.path(self.cwd)).unwrap_or_else(|| self.root.clone())
    }

    /// Rows for the folder on screen: subfolders from the index, files read now.
    pub fn rebuild(&mut self) {
        let Some(idx) = &self.index else {
            self.rows.clear();
            return;
        };
        let dir = idx.path(self.cwd);
        let mut rows: Vec<Row> = idx
            .children(self.cwd)
            .iter()
            .map(|&c| {
                let n = &idx.nodes[c as usize];
                // The lossy name is for display; the path keeps the exact bytes.
                let name = String::from_utf8_lossy(idx.name(c)).into_owned();
                Row {
                    kind: RowKind::Dir(c),
                    path: dir.join(tdy_scan::path::bytes_to_path(idx.name(c))),
                    name,
                    bytes: Bytes { logical: n.logical, alloc: n.alloc, private: n.private },
                    items: n.files,
                    mtime: n.mtime_max,
                    tag: self.tags.get(&c).map(|t| t.id),
                    flags: n.flags,
                }
            })
            .collect();
        if self.files.as_ref().is_none_or(|(id, _)| *id != self.cwd) {
            let mut f = Vec::new();
            list_files(&dir, &mut f);
            self.files = Some((self.cwd, f));
        }
        rows.extend(self.files.as_ref().map(|(_, f)| f.iter().cloned()).into_iter().flatten());
        if !self.filter.is_empty() {
            let q = self.filter.to_lowercase();
            rows.retain(|r| r.name.to_lowercase().contains(&q));
        }
        let by = self.by;
        match self.sort {
            Sort::Size => rows.sort_by(|a, b| b.size(by).cmp(&a.size(by)).then_with(|| a.name.cmp(&b.name))),
            Sort::Name => rows.sort_by(|a, b| b.is_dir().cmp(&a.is_dir()).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))),
            Sort::Age => rows.sort_by(|a, b| a.mtime.cmp(&b.mtime).then_with(|| b.size(by).cmp(&a.size(by)))),
            Sort::Items => rows.sort_by(|a, b| b.items.cmp(&a.items).then_with(|| b.size(by).cmp(&a.size(by)))),
        }
        self.rows = rows;
        let sel = self.remember.get(&self.cwd).copied().unwrap_or(0).min(self.rows.len().saturating_sub(1));
        self.state.select(if self.rows.is_empty() { None } else { Some(sel) });
    }

    fn select_path(&mut self, p: &Path) {
        if let Some(i) = self.rows.iter().position(|r| r.path == p) {
            self.state.select(Some(i));
        }
    }

    pub fn enter(&mut self) {
        if let Some(Row { kind: RowKind::Dir(id), .. }) = self.selected().cloned() {
            self.remember.insert(self.cwd, self.state.selected().unwrap_or(0));
            self.cwd = id;
            self.filter.clear();
            self.rebuild();
        }
    }

    /// Returns false at the top of the scan.
    pub fn up(&mut self) -> bool {
        let Some(idx) = &self.index else { return false };
        if self.cwd == 0 {
            return false;
        }
        let from = idx.path(self.cwd);
        self.remember.insert(self.cwd, self.state.selected().unwrap_or(0));
        self.cwd = idx.nodes[self.cwd as usize].parent;
        self.filter.clear();
        self.rebuild();
        self.select_path(&from);
        true
    }

    fn move_by(&mut self, d: isize) {
        if self.rows.is_empty() {
            return;
        }
        let cur = self.state.selected().unwrap_or(0) as isize;
        let n = (cur + d).clamp(0, self.rows.len() as isize - 1);
        self.state.select(Some(n as usize));
    }

    pub fn toggle_mark(&mut self) {
        if let Some(r) = self.selected().cloned() {
            if self.marks.remove(&r.path).is_none() {
                self.marks.insert(r.path, r.bytes);
            }
            self.move_by(1);
        }
    }

    fn marked_bytes(&self) -> u64 {
        self.marks.values().map(|b| b.alloc).sum()
    }

    /// Marked items, or the selected row when nothing is marked.
    fn targets(&self) -> Vec<(PathBuf, Bytes)> {
        if !self.marks.is_empty() {
            return self.marks.iter().map(|(p, b)| (p.clone(), *b)).collect();
        }
        self.selected().map(|r| vec![(r.path.clone(), r.bytes)]).unwrap_or_default()
    }

    fn propose_dialog(&self, action: Action) -> Act {
        let items = self.targets();
        if items.is_empty() {
            return Act::Toast("nothing selected".into());
        }
        let bytes: u64 = items.iter().map(|(_, b)| b.alloc).sum();
        let verb = if action == Action::Trash { "Move to the trash" } else { "Move to Tidy's quarantine" };
        let mut lines =
            vec![Line::from(vec![Span::raw(format!("{verb}: {} item(s), ", items.len())), Span::styled(fmt::bytes(bytes), bold())])];
        for (p, b) in items.iter().take(6) {
            lines.push(Line::from(vec![Span::styled(format!("  {:>9}  ", fmt::bytes(b.alloc)), dim()), Span::raw(display_path(p))]));
        }
        if items.len() > 6 {
            lines.push(Line::from(Span::styled(format!("  … {} more", items.len() - 6), dim())));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "This creates a plan approved by you. Guards still check every item, and nothing moves until you apply it on the Plans screen.",
            dim(),
        )));
        Act::Dialog(Dialog {
            title: format!("{} {} item(s)?", action.as_str(), items.len()),
            lines,
            confirm: Some(Confirm::Propose(action, items)),
        })
    }

    pub fn on_proposed(&mut self, action: Action, result: Result<ProposeResult, String>) -> Act {
        match result {
            Err(e) => Act::Dialog(Dialog { title: "Could not create the plan".into(), lines: vec![Line::raw(e)], confirm: None }),
            Ok(r) => {
                for p in &r.accepted {
                    self.marks.remove(p);
                }
                if !r.rejected.is_empty() {
                    let mut lines = vec![Line::raw(match &r.plan {
                        Some(id) => format!(
                            "Plan {id}: {} {} item(s), {}. The guards refused these:",
                            action.as_str(),
                            r.entries,
                            fmt::bytes(r.bytes)
                        ),
                        None => "The guards refused every item:".into(),
                    })];
                    for (p, why) in r.rejected.iter().take(10) {
                        lines.push(Line::from(vec![
                            Span::raw(format!("  {}  ", display_path(p))),
                            Span::styled(why.clone(), fg(Color::Red)),
                        ]));
                    }
                    if r.rejected.len() > 10 {
                        lines.push(Line::from(Span::styled(format!("  … {} more", r.rejected.len() - 10), dim())));
                    }
                    if let Some(id) = &r.plan {
                        lines.push(Line::raw(""));
                        lines.push(Line::raw(format!("The plan is on the Plans screen (2), as {id}.")));
                    }
                    return Act::Dialog(Dialog { title: "Some items were refused".into(), lines, confirm: None });
                }
                match r.plan {
                    Some(id) => Act::OpenPlan(id),
                    None => Act::Toast("nothing to do".into()),
                }
            }
        }
    }

    fn known(&self, row: &Row) -> Option<Known> {
        let RowKind::Dir(id) = row.kind else { return None };
        let idx = self.index.as_ref()?;
        let n = &idx.nodes[id as usize];
        Some(Known {
            files: n.files,
            logical: n.logical,
            newest: n.mtime_max,
            git_roots: tdy_rules::guards::git_roots_in_index(idx, id),
            detector: self.tags.get(&id).map(|t| (t.id.to_string(), t.what.clone(), t.hint.to_string(), t.min_age)),
        })
    }

    /// With the details panel open, explain the selected row (once).
    pub fn want_explain(&mut self, ctx: &Ctx) {
        if !self.info {
            return;
        }
        let Some(r) = self.selected().cloned() else { return };
        if self.explained.contains_key(&r.path) || self.explaining.contains(&r.path) {
            return;
        }
        self.explaining.insert(r.path.clone());
        let known = self.known(&r);
        worker::explain(ctx.tx.clone(), ctx.guards.clone(), r.path, known);
    }

    #[cfg(test)]
    pub fn is_explained(&self, p: &Path) -> bool {
        self.explained.contains_key(p)
    }

    pub fn on_explained(&mut self, path: PathBuf, result: Result<Explanation, String>) {
        self.explaining.remove(&path);
        self.explained.insert(path, result);
    }

    pub fn on_rules(&mut self, path: PathBuf, result: Result<Option<RulesExplanation>, String>) {
        self.ruling.remove(&path);
        self.rules.insert(path, result);
    }

    /// Returns (handled, action).
    pub fn on_key(&mut self, k: KeyEvent, ctx: &Ctx) -> (bool, Act) {
        if self.typing {
            match k.code {
                KeyCode::Enter => self.typing = false,
                KeyCode::Esc => {
                    self.typing = false;
                    self.filter.clear();
                    self.rebuild();
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.rebuild();
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.rebuild();
                }
                _ => {}
            }
            return (true, Act::None);
        }
        let act = match k.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_by(1);
                Act::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_by(-1);
                Act::None
            }
            KeyCode::PageDown => {
                self.move_by(20);
                Act::None
            }
            KeyCode::PageUp => {
                self.move_by(-20);
                Act::None
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.move_by(-(self.rows.len() as isize));
                Act::None
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.move_by(self.rows.len() as isize);
                Act::None
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                self.enter();
                Act::None
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                if self.up() {
                    Act::None
                } else {
                    Act::Toast(format!("top of the scan ({})", display_path(&self.root)))
                }
            }
            KeyCode::Char(' ') => {
                self.toggle_mark();
                Act::None
            }
            KeyCode::Char('c') => {
                let n = self.marks.len();
                self.marks.clear();
                Act::Toast(format!("{n} mark(s) cleared"))
            }
            KeyCode::Char('t') => self.propose_dialog(Action::Trash),
            KeyCode::Char('z') => self.propose_dialog(Action::Quarantine),
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.rebuild();
                Act::None
            }
            KeyCode::Char('b') => {
                let measured = self.index.as_ref().is_some_and(|i| i.stats.private_measured);
                self.by = match self.by {
                    SizeKind::Alloc => SizeKind::Logical,
                    SizeKind::Logical if measured => SizeKind::Private,
                    _ => SizeKind::Alloc,
                };
                self.rebuild();
                Act::None
            }
            KeyCode::Char('i') => {
                self.info = !self.info;
                Act::None
            }
            KeyCode::Char('E') => match self.selected().cloned() {
                Some(r) if !self.ruling.contains(&r.path) => {
                    self.info = true;
                    self.rules.remove(&r.path);
                    self.ruling.insert(r.path.clone());
                    worker::rules(ctx.tx.clone(), ctx.policy.clone(), r.path);
                    Act::None
                }
                _ => Act::None,
            },
            KeyCode::Char('/') => {
                self.typing = true;
                Act::None
            }
            KeyCode::Char('r') => {
                self.start_scan(ctx);
                Act::None
            }
            // plans for the folder on screen
            KeyCode::Char('p') => Act::NewPlan(Start { kind: Some(PlanKind::Cleanup), folder: Some(self.cwd_path()), run: true }),
            KeyCode::Char('a') => Act::NewPlan(Start { kind: Some(PlanKind::Ask), folder: Some(self.cwd_path()), run: false }),
            KeyCode::Char('o') => match self.selected() {
                Some(r) => reveal(&r.path),
                None => Act::None,
            },
            KeyCode::Esc => {
                if let Some((p, _)) = &self.scanning {
                    p.cancel.store(true, Ordering::Relaxed);
                    Act::Toast("cancelling the scan…".into())
                } else if !self.filter.is_empty() {
                    self.filter.clear();
                    self.rebuild();
                    Act::None
                } else {
                    return (true, Act::None);
                }
            }
            _ => return (false, Act::None),
        };
        self.want_explain(ctx);
        (true, act)
    }

    pub fn hints(&self) -> String {
        if self.typing {
            return "type to filter · ⏎ keep · Esc clear".into();
        }
        "⏎ open · ⌫ up · space mark · t trash · p plan here · a ask · i details · / filter · ? more".into()
    }

    pub fn draw(&mut self, f: &mut Frame<'_>, area: Rect, ctx: &Ctx) {
        let [head, body] = Layout::vertical([Constraint::Length(2), Constraint::Min(3)]).areas(area);
        self.draw_head(f, head);
        let (list, side) = if self.info {
            let [l, s] = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(body);
            (l, Some(s))
        } else {
            (body, None)
        };
        self.draw_table(f, list);
        if let Some(s) = side {
            self.want_explain(ctx);
            self.draw_info(f, s);
        }
    }

    fn draw_head(&self, f: &mut Frame<'_>, area: Rect) {
        let by_label = match self.by {
            SizeKind::Alloc => "on disk",
            SizeKind::Logical => "apparent",
            SizeKind::Private => "unshared",
        };
        let mut l1 = vec![Span::styled(format!(" {}", display_path(&self.cwd_path())), bold().patch(accent()))];
        if let Some(idx) = &self.index {
            let n = &idx.nodes[self.cwd as usize];
            let size = match self.by {
                SizeKind::Logical => n.logical,
                SizeKind::Alloc => n.alloc,
                SizeKind::Private => n.private,
            };
            l1.push(Span::raw(format!("  {} {by_label}", fmt::bytes(size))));
            l1.push(Span::styled(
                format!(" · {} · {}", plural(n.files, "file", "files"), plural(n.dirs as u64, "folder", "folders")),
                dim(),
            ));
        }
        l1.push(Span::styled(format!("   sort {} · bytes {by_label}", self.sort.label()), dim()));
        let age = self.index.as_ref().map(|i| fmt::age(tdy_scan::cache::age_secs(i))).unwrap_or_default();
        let l2 = if let Some((p, t)) = &self.scanning {
            let what = if self.cached.is_some() {
                format!(" {} refreshing the cached index ({age} old) ", spinner())
            } else {
                format!(" {} scanning ", spinner())
            };
            Line::from(vec![
                Span::styled(what, accent()),
                Span::raw(format!(
                    "{} files · {} folders · {} · {:.1}s",
                    crate::ui::thousands(p.files.load(Ordering::Relaxed)),
                    crate::ui::thousands(p.dirs.load(Ordering::Relaxed)),
                    fmt::bytes(p.bytes.load(Ordering::Relaxed)),
                    t.elapsed().as_secs_f64()
                )),
                Span::styled("   Esc cancels", dim()),
            ])
        } else if self.typing || !self.filter.is_empty() {
            Line::from(vec![
                Span::styled(" filter: ", dim()),
                Span::raw(self.filter.clone()),
                Span::raw(if self.typing { "▏" } else { "" }),
            ])
        } else if let Some(e) = &self.error {
            Line::from(Span::styled(format!(" {e}"), fg(Color::Red)))
        } else {
            let mut s = vec![];
            if !self.marks.is_empty() {
                s.push(Span::styled(
                    format!(" ● {} marked · {} ", self.marks.len(), fmt::bytes(self.marked_bytes())),
                    fg(Color::Green).add_modifier(Modifier::BOLD),
                ));
            }
            if self.stale {
                s.push(Span::styled(" changed since the scan — r to rescan ", fg(Color::Yellow)));
            }
            if s.is_empty()
                && let Some(from) = &self.cached
            {
                let of = if *from == self.root { String::new() } else { format!(" of {}", display_path(from)) };
                let since = if self.patched { " and kept up to date since" } else { "" };
                s.push(Span::styled(format!(" cached index{of}, scanned {age} ago{since} · r rescans"), dim()));
            }
            if let Some(idx) = &self.index
                && s.is_empty()
            {
                s.push(Span::styled(
                    format!(
                        " scanned {} in {}{}",
                        display_path(&self.root),
                        if idx.stats.elapsed_ms < 1000 {
                            format!("{} ms", idx.stats.elapsed_ms.max(1))
                        } else {
                            format!("{:.1} s", idx.stats.elapsed_ms as f64 / 1000.0)
                        },
                        if idx.stats.errors > 0 { format!(" · {} unreadable", idx.stats.errors) } else { String::new() }
                    ),
                    dim(),
                ));
            }
            Line::from(s)
        };
        f.render_widget(Paragraph::new(vec![Line::from(l1), l2]), area);
    }

    fn draw_table(&mut self, f: &mut Frame<'_>, area: Rect) {
        let total = self.index.as_ref().map(|i| i.nodes[self.cwd as usize].size(self.by)).unwrap_or(0).max(1);
        let now = fmt::now_secs();
        // Narrow (or beside the details panel): drop the percentage and item
        // count so names keep their room; the detector column only when used.
        let wide = area.width >= 100;
        let tags = self.rows.iter().any(|r| r.tag.is_some());
        let rows: Vec<TRow> = self
            .rows
            .iter()
            .map(|r| {
                let size = r.size(self.by);
                let frac = size as f64 / total as f64;
                let marked = self.marks.contains_key(&r.path);
                let name = match r.kind {
                    RowKind::Dir(_) if r.flags & node_flags::UNREADABLE != 0 => format!("{}/  (unreadable)", r.name),
                    RowKind::Dir(_) => format!("{}/", r.name),
                    RowKind::Link => format!("{} →", r.name),
                    _ => r.name.clone(),
                };
                let name_style = if r.is_dir() { bold().patch(accent()) } else { Style::default() };
                let mut cells = vec![
                    Cell::from(if marked { "●" } else { " " }).style(fg(Color::Green)),
                    Cell::from(format!("{:>9}", fmt::bytes(size))),
                    Cell::from(bar(frac, if wide { 10 } else { 6 })).style(accent()),
                ];
                if wide {
                    cells.push(Cell::from(format!("{:>5.1}%", frac * 100.0)).style(dim()));
                    cells.push(
                        Cell::from(if r.is_dir() { format!("{:>8}", crate::ui::thousands(r.items)) } else { String::new() }).style(dim()),
                    );
                }
                cells.push(Cell::from(if r.mtime > 0 { format!("{:>5}", fmt::age(now - r.mtime)) } else { String::new() }).style(dim()));
                cells.push(Cell::from(name).style(name_style));
                if tags {
                    cells.push(Cell::from(r.tag.unwrap_or("")).style(fg(Color::Yellow)));
                }
                TRow::new(cells)
            })
            .collect();
        let mut widths = vec![Constraint::Length(1), Constraint::Length(9), Constraint::Length(if wide { 10 } else { 6 })];
        let mut header = vec!["", "     size", ""];
        if wide {
            widths.extend([Constraint::Length(6), Constraint::Length(8)]);
            header.extend(["", "   items"]);
        }
        widths.extend([Constraint::Length(5), Constraint::Fill(1)]);
        header.extend(["  age", "name"]);
        if tags {
            widths.push(Constraint::Length(if wide { 18 } else { 13 }));
            header.push("detector");
        }
        let empty = self.rows.is_empty() && self.scanning.is_none() && self.index.is_some();
        let table = Table::new(rows, widths)
            .header(TRow::new(header).style(dim()))
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .block(Block::default().borders(Borders::TOP));
        f.render_stateful_widget(table, area, &mut self.state);
        if empty {
            let msg = if self.filter.is_empty() { "  (empty folder)" } else { "  (nothing matches the filter)" };
            let a = Rect { y: area.y + 2, height: 1, ..area };
            f.render_widget(Paragraph::new(Span::styled(msg, dim())), a);
        }
    }

    fn draw_info(&self, f: &mut Frame<'_>, area: Rect) {
        let block = Block::default().borders(Borders::LEFT | Borders::TOP).title(Span::styled(" details ", bold()));
        let Some(r) = self.selected() else {
            f.render_widget(Paragraph::new("").block(block), area);
            return;
        };
        let k = |s: &str| Span::styled(format!("{s:<10}"), dim());
        let mut lines = vec![
            Line::from(Span::styled(r.name.clone(), bold())),
            Line::from(vec![k("path"), Span::raw(display_path(&r.path))]),
            Line::from(vec![
                k("on disk"),
                Span::raw(fmt::bytes(r.bytes.alloc)),
                Span::styled("  apparent ", dim()),
                Span::raw(fmt::bytes(r.bytes.logical)),
            ]),
        ];
        if self.index.as_ref().is_some_and(|i| i.stats.private_measured) {
            lines.push(Line::from(vec![k("unshared"), Span::raw(fmt::bytes(r.bytes.private))]));
        }
        if r.is_dir() {
            lines.push(Line::from(vec![k("files"), Span::raw(crate::ui::thousands(r.items))]));
        }
        if r.mtime > 0 {
            lines.push(Line::from(vec![k("changed"), Span::raw(format!("{} ago", fmt::age(fmt::now_secs() - r.mtime)))]));
        }
        if let RowKind::Dir(id) = r.kind
            && let Some(t) = self.tags.get(&id)
        {
            lines.push(Line::from(vec![k("detector"), Span::styled(t.id, fg(Color::Yellow))]));
            lines.push(Line::from(vec![k(""), Span::raw(format!("{} — {}", t.what, t.hint))]));
        }
        lines.push(Line::raw(""));
        match self.explained.get(&r.path) {
            None => lines.push(Line::from(Span::styled(format!("{} checking guards…", spinner()), dim()))),
            Some(Err(e)) => lines.push(Line::from(Span::styled(e.clone(), fg(Color::Red)))),
            Some(Ok(x)) => {
                lines.push(Line::from(Span::styled("guards", bold())));
                for (a, res) in &x.guards {
                    lines.push(match res {
                        Ok(()) => Line::from(vec![Span::raw(format!("  {:<7}", a.as_str())), Span::styled("allowed", fg(Color::Green))]),
                        Err(why) => Line::from(vec![
                            Span::raw(format!("  {:<7}", a.as_str())),
                            Span::styled("blocked ", fg(Color::Red)),
                            Span::styled(why.clone(), dim()),
                        ]),
                    });
                }
            }
        }
        lines.push(Line::raw(""));
        match self.rules.get(&r.path) {
            None if self.ruling.contains(&r.path) => {
                lines.push(Line::from(Span::styled(format!("{} asking the policy rules…", spinner()), dim())))
            }
            None => lines.push(Line::from(Span::styled("E: ask the policy rules (may call the judge)", dim()))),
            Some(Err(e)) => lines.push(Line::from(Span::styled(format!("rules: {e}"), fg(Color::Red)))),
            Some(Ok(None)) => lines.push(Line::from(Span::styled("no policy rule applies here", dim()))),
            Some(Ok(Some(x))) => {
                lines.push(Line::from(Span::styled("policy rules", bold())));
                for (id, p, tau, action) in &x.rows {
                    lines.push(Line::from(vec![
                        Span::raw(format!("  {id:<18}")),
                        Span::raw(format!("p={} ", p.map(|p| format!("{p:.2}")).unwrap_or("–".into()))),
                        Span::styled(format!("τ={tau:.2} {action}"), dim()),
                    ]));
                }
                lines.push(Line::from(vec![Span::raw("  → "), Span::styled(x.verdict.clone(), bold())]));
                if let Some(j) = &x.judge {
                    lines.push(Line::from(Span::styled(format!("  {j}"), dim())));
                }
            }
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
    }
}

/// Files (and links, sockets…) directly inside `dir`. Large folders are
/// stat-ed on several threads.
fn list_files(dir: &Path, rows: &mut Vec<Row>) {
    let Ok(entries) = tdy_scan::list_dir(dir, ReadOptions { private: cfg!(target_os = "macos") }) else { return };
    rows.reserve(entries.len());
    for e in entries {
        let kind = match e.kind {
            Kind::Dir => continue,
            Kind::File => RowKind::File,
            Kind::Symlink => RowKind::Link,
            Kind::Other => RowKind::Other,
        };
        let alloc = if e.flags & tdy_scan::entry::flags::DATALESS != 0 { 0 } else { e.alloc };
        rows.push(Row {
            kind,
            path: dir.join(tdy_scan::path::bytes_to_path(&e.name)),
            name: String::from_utf8_lossy(&e.name).into_owned(),
            bytes: Bytes { logical: e.logical, alloc, private: e.private.unwrap_or(alloc).min(alloc) },
            items: 1,
            mtime: e.mtime,
            tag: None,
            flags: 0,
        });
    }
}

/// `████▌     ` for a fraction of `width` cells.
pub fn bar(frac: f64, width: usize) -> String {
    const PARTS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let eighths = (frac.clamp(0.0, 1.0) * (width * 8) as f64).round() as usize;
    let mut s = "█".repeat(eighths / 8);
    if eighths / 8 < width {
        s.push(PARTS[eighths % 8]);
        s.push_str(&" ".repeat(width - eighths / 8 - 1));
    }
    s
}

/// Show `p` in Finder (macOS) or open its folder (Linux).
fn reveal(p: &Path) -> Act {
    use std::process::{Command, Stdio};
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg("-R").arg(p);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(if p.is_dir() { p } else { p.parent().unwrap_or(p) });
        c
    };
    match cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(mut child) => {
            // reap it so it doesn't linger as a zombie
            std::thread::spawn(move || child.wait());
            Act::None
        }
        Err(e) => Act::Toast(format!("cannot open a file manager: {e}")),
    }
}
