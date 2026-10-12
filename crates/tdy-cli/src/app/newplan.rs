//! New plan: make a plan for a folder, one of four ways —
//!
//! - **Cleanup**: the detectors and your policy rules (`tdy plan`)
//! - **Ask**: a request in plain English, run as a one-off rule (`tdy ask`)
//! - **Duplicates**: identical files (`tdy dupes --plan`)
//! - **Archive**: large folders untouched for long (`tdy archive`)
//!
//! — with live progress; the finished plan opens on the Plans screen.

use super::worker::{self, PlanResult};
use super::{Act, Ctx, Dialog, accent, bold, dim, fg, plural, spinner};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tdy_scan::path::{display_path, expand_tilde};
use tdy_scan::{ScanProgress, fmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Cleanup,
    Ask,
    Dupes,
    Archive,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Cleanup, Kind::Ask, Kind::Dupes, Kind::Archive];
    pub fn title(self) -> &'static str {
        match self {
            Kind::Cleanup => "Cleanup",
            Kind::Ask => "Ask",
            Kind::Dupes => "Duplicates",
            Kind::Archive => "Archive",
        }
    }
    fn about(self) -> &'static str {
        match self {
            Kind::Cleanup => "Detectors find build output and caches; your policy rules judge the rest.",
            Kind::Ask => "Say what to do in plain English. Folder, age, size and kind are checked in code; the judge decides the rest.",
            Kind::Dupes => "Identical files: copies become copy-on-write clones where the disk can share storage, else go to the trash.",
            Kind::Archive => "Large folders untouched for long: packed into .tar.zst (original quarantined) or compressed in place.",
        }
    }
}

/// The text field being typed into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Folder,
    Request,
    MinSize,
    Older,
}

/// How another screen opens this one.
#[derive(Clone, Debug)]
pub struct Start {
    pub kind: Option<Kind>,
    pub folder: Option<PathBuf>,
    /// Start right away.
    pub run: bool,
}

pub struct Running {
    pub phase: &'static str,
    pub scan: Option<Arc<ScanProgress>>,
    pub judged: (usize, usize),
    pub started: Instant,
}

pub struct NewPlan {
    pub kind: Kind,
    pub path: String,
    pub editing: Option<Field>,
    pub request: String,
    // cleanup
    pub detectors: bool,
    pub rules: bool,
    // duplicates
    pub dupes_min: String,
    // archive
    pub older: String,
    pub archive_min: String,
    pub transparent: bool,
    pub running: Option<Running>,
    pub result: Option<Result<PlanResult, String>>,
}

impl NewPlan {
    pub fn new(root: &Path) -> Self {
        NewPlan {
            kind: Kind::Cleanup,
            path: display_path(root),
            editing: None,
            request: String::new(),
            detectors: true,
            rules: true,
            dupes_min: "1MB".into(),
            older: "1y".into(),
            archive_min: "100MB".into(),
            transparent: false,
            running: None,
            result: None,
        }
    }

    /// Opened from another screen.
    pub fn start(&mut self, s: Start, ctx: &Ctx) -> Act {
        if self.running.is_some() {
            return Act::Toast("a plan is still being made here".into());
        }
        if let Some(k) = s.kind {
            self.kind = k;
        }
        if let Some(f) = &s.folder {
            self.path = display_path(f);
        }
        self.editing = None;
        if s.run {
            return self.run(ctx);
        }
        if self.kind == Kind::Ask {
            self.editing = Some(Field::Request);
        }
        Act::None
    }

    pub fn on_scanning(&mut self, p: Arc<ScanProgress>) {
        if let Some(r) = &mut self.running {
            r.phase = "scanning";
            r.scan = Some(p);
        }
    }
    pub fn on_phase(&mut self, phase: &'static str) {
        if let Some(r) = &mut self.running {
            r.phase = phase;
        }
    }
    pub fn on_judging(&mut self, done: usize, total: usize) {
        if let Some(r) = &mut self.running {
            r.judged = (done, total);
        }
    }
    pub fn on_done(&mut self, r: Result<PlanResult, String>) -> Act {
        self.running = None;
        let act = match &r {
            Ok(c) if c.actions > 0 => Act::OpenPlan(c.plan.clone()),
            Ok(_) => Act::Toast("nothing to do here: the plan is empty".into()),
            Err(e) => {
                Act::Dialog(Dialog { title: format!("{} failed", self.kind.title()), lines: vec![Line::raw(e.clone())], confirm: None })
            }
        };
        self.result = Some(r);
        act
    }

    fn folder(&self) -> Result<PathBuf, String> {
        match std::fs::canonicalize(expand_tilde(self.path.trim())) {
            Ok(p) if p.is_dir() => Ok(p),
            Ok(p) => Err(format!("{} is not a folder", display_path(&p))),
            Err(e) => Err(format!("{}: {e}", self.path.trim())),
        }
    }

    /// The request as understood, or what's wrong with it.
    fn understood(&self) -> Result<tdy_rules::ask::Request, String> {
        let here = self.folder()?;
        if self.request.trim().is_empty() {
            return Err("type what to do, e.g. “trash screenshots older than 30 days”".into());
        }
        tdy_rules::ask::parse(&self.request, &here)
    }

    fn run(&mut self, ctx: &Ctx) -> Act {
        if self.running.is_some() {
            return Act::Toast("already running".into());
        }
        let root = match self.folder() {
            Ok(r) => r,
            Err(e) => return Act::Toast(e),
        };
        let tx = ctx.tx.clone();
        match self.kind {
            Kind::Cleanup => {
                if !self.detectors && !self.rules {
                    return Act::Toast("turn on detectors (d) or policy rules (p) first".into());
                }
                worker::cleanup(tx, root, self.detectors, self.rules);
            }
            Kind::Ask => match self.understood() {
                Ok(req) => worker::ask(tx, req),
                Err(e) => {
                    self.editing = Some(Field::Request);
                    return Act::Toast(e);
                }
            },
            Kind::Dupes => {
                let Some(min) = fmt::parse_bytes(&self.dupes_min) else {
                    return Act::Toast(format!("“{}” is not a size (try 1MB)", self.dupes_min));
                };
                worker::dupes(tx, root, min);
            }
            Kind::Archive => {
                let Some(older) = fmt::parse_duration(&self.older) else {
                    return Act::Toast(format!("“{}” is not an age (try 6mo or 1y)", self.older));
                };
                let Some(min_size) = fmt::parse_bytes(&self.archive_min) else {
                    return Act::Toast(format!("“{}” is not a size (try 100MB)", self.archive_min));
                };
                let opts = crate::space_cmds::ArchiveOpts { older, min_size, max_ratio: 0.8, transparent: self.transparent };
                worker::archive(tx, root, opts);
            }
        }
        self.editing = None;
        self.result = None;
        self.running = Some(Running { phase: "starting", scan: None, judged: (0, 0), started: Instant::now() });
        Act::None
    }

    fn field_mut(&mut self, f: Field) -> &mut String {
        match f {
            Field::Folder => &mut self.path,
            Field::Request => &mut self.request,
            Field::MinSize if self.kind == Kind::Dupes => &mut self.dupes_min,
            Field::MinSize => &mut self.archive_min,
            Field::Older => &mut self.older,
        }
    }

    fn switch(&mut self, by: isize) {
        let i = Kind::ALL.iter().position(|k| *k == self.kind).unwrap_or(0) as isize;
        self.kind = Kind::ALL[(i + by).rem_euclid(Kind::ALL.len() as isize) as usize];
        self.result = None;
    }

    /// Returns (handled, action).
    pub fn on_key(&mut self, k: KeyEvent, ctx: &Ctx) -> (bool, Act) {
        if let Some(f) = self.editing {
            match k.code {
                // a typed request runs on Enter
                KeyCode::Enter if f == Field::Request => return (true, self.run(ctx)),
                KeyCode::Enter | KeyCode::Esc => self.editing = None,
                KeyCode::Backspace => {
                    self.field_mut(f).pop();
                }
                KeyCode::Char(c) => self.field_mut(f).push(c),
                _ => {}
            }
            return (true, Act::None);
        }
        let idle = self.running.is_none();
        let a = match (k.code, self.kind) {
            (KeyCode::Left, _) if idle => {
                self.switch(-1);
                Act::None
            }
            (KeyCode::Right, _) if idle => {
                self.switch(1);
                Act::None
            }
            (KeyCode::Char('e'), _) if idle => {
                self.editing = Some(Field::Folder);
                Act::None
            }
            (KeyCode::Char('w'), Kind::Ask) if idle => {
                self.editing = Some(Field::Request);
                Act::None
            }
            (KeyCode::Char('d'), Kind::Cleanup) if idle => {
                self.detectors = !self.detectors;
                Act::None
            }
            (KeyCode::Char('p'), Kind::Cleanup) if idle => {
                self.rules = !self.rules;
                Act::None
            }
            (KeyCode::Char('m'), Kind::Dupes | Kind::Archive) if idle => {
                self.editing = Some(Field::MinSize);
                Act::None
            }
            (KeyCode::Char('g'), Kind::Archive) if idle => {
                self.editing = Some(Field::Older);
                Act::None
            }
            (KeyCode::Char('c'), Kind::Archive) if idle => {
                self.transparent = !self.transparent;
                Act::None
            }
            (KeyCode::Enter, Kind::Ask) if idle && self.request.trim().is_empty() => {
                self.editing = Some(Field::Request);
                Act::None
            }
            (KeyCode::Enter, _) => self.run(ctx),
            (KeyCode::Char('o'), _) => match &self.result {
                Some(Ok(c)) if c.actions > 0 => Act::OpenPlan(c.plan.clone()),
                _ => Act::None,
            },
            (KeyCode::Esc, _) => {
                if let Some(Running { scan: Some(p), phase: "scanning", .. }) = &self.running {
                    p.cancel.store(true, Ordering::Relaxed);
                    Act::Toast("cancelling…".into())
                } else {
                    Act::None
                }
            }
            _ => return (false, Act::None),
        };
        (true, a)
    }

    pub fn hints(&self) -> String {
        match self.editing {
            Some(Field::Request) => return "type what to do · ⏎ make the plan · Esc stop typing".into(),
            Some(_) => return "type · ⏎ done".into(),
            None => {}
        }
        if self.running.is_some() {
            return "working… · Esc cancels a scan".into();
        }
        let kind = match self.kind {
            Kind::Cleanup => "d detectors · p policy rules",
            Kind::Ask => "w write the request",
            Kind::Dupes => "m minimum size",
            Kind::Archive => "g age · m size · c compress in place",
        };
        format!("⏎ make the plan · ←→ kind · e folder · {kind} · o open result")
    }

    pub fn draw(&self, f: &mut Frame<'_>, area: Rect, ctx: &Ctx) {
        let [tabs, form, status] = Layout::vertical([Constraint::Length(2), Constraint::Length(11), Constraint::Min(3)]).areas(area);
        let mut t = vec![Span::raw(" ")];
        for k in Kind::ALL {
            t.push(if k == self.kind {
                Span::styled(format!(" {} ", k.title()), Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD))
            } else {
                Span::styled(format!(" {} ", k.title()), dim())
            });
            t.push(Span::raw(" "));
        }
        t.push(Span::styled("  ←→", dim()));
        f.render_widget(Paragraph::new(vec![Line::from(t)]), tabs);

        let check = |on: bool| if on { Span::styled("[x] ", accent()) } else { Span::styled("[ ] ", dim()) };
        let k = |s: &str| Span::styled(format!(" {s:<11}"), dim());
        let text = |s: &str, field: Field, key: &str| {
            let on = self.editing == Some(field);
            vec![
                Span::styled(s.to_string(), if on { bold().patch(accent()) } else { bold() }),
                Span::raw(if on { "▏" } else { "" }),
                Span::styled(if on { String::new() } else { format!("   {key}") }, dim()),
            ]
        };
        let mut lines = vec![Line::from(Span::styled(format!(" {}", self.kind.about()), dim())), Line::raw("")];
        let mut folder = vec![k("folder")];
        folder.extend(text(&self.path, Field::Folder, "e edit"));
        lines.push(Line::from(folder));
        let pol = &ctx.policy;
        let judge = judge_name(pol);
        match self.kind {
            Kind::Cleanup => {
                let needs_judge = pol.rules.iter().any(|r| !r.when.is_empty());
                let mut policy_line = vec![k("policy"), check(self.rules), Span::raw(format!("{} rules", pol.rules.len()))];
                if !pol.presets.is_empty() {
                    policy_line.push(Span::styled(format!(" · presets {}", pol.presets.join(", ")), dim()));
                }
                if needs_judge {
                    policy_line.push(if judge.is_empty() {
                        Span::styled(" · no judge configured: model rules will be skipped", fg(Color::Yellow))
                    } else {
                        Span::styled(format!(" · judge {judge}"), dim())
                    });
                }
                policy_line.push(Span::styled("   p", dim()));
                lines.push(Line::from(vec![
                    k("detectors"),
                    check(self.detectors),
                    Span::raw("build artefacts and caches"),
                    Span::styled("   d", dim()),
                ]));
                lines.push(Line::from(policy_line));
            }
            Kind::Ask => {
                let mut req = vec![k("request")];
                if self.request.is_empty() && self.editing != Some(Field::Request) {
                    req.push(Span::styled("e.g. trash screenshots older than 30 days", dim()));
                    req.push(Span::styled("   w write", dim()));
                } else {
                    req.extend(text(&self.request, Field::Request, "w edit"));
                }
                lines.push(Line::from(req));
                if !self.request.trim().is_empty() {
                    match self.understood() {
                        Ok(r) => {
                            for (key, v) in r.describe() {
                                let warn = key == "judge" && r.criterion.is_some() && judge.is_empty();
                                lines.push(Line::from(vec![
                                    Span::styled(format!("   {key:>9}  "), dim()),
                                    if warn {
                                        Span::styled(format!("{v} — but no judge is configured"), fg(Color::Yellow))
                                    } else {
                                        Span::raw(v)
                                    },
                                ]));
                            }
                        }
                        Err(e) => lines.push(Line::from(Span::styled(format!("   {e}"), fg(Color::Yellow)))),
                    }
                }
            }
            Kind::Dupes => {
                let mut v = vec![k("at least")];
                v.extend(text(&self.dupes_min, Field::MinSize, "m edit"));
                lines.push(Line::from(v));
            }
            Kind::Archive => {
                let mut v = vec![k("untouched")];
                v.extend(text(&self.older, Field::Older, "g edit"));
                lines.push(Line::from(v));
                let mut v = vec![k("at least")];
                v.extend(text(&self.archive_min, Field::MinSize, "m edit"));
                lines.push(Line::from(v));
                lines.push(Line::from(vec![
                    k("in place"),
                    check(self.transparent),
                    Span::raw("compress in place (APFS, btrfs) instead of archiving"),
                    Span::styled("   c", dim()),
                ]));
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(" The result is a plan: nothing changes until you apply it (Plans).", dim())));
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::default().borders(Borders::TOP)), form);

        let mut out: Vec<Line> = vec![];
        if let Some(r) = &self.running {
            let detail = match (r.phase, &r.scan) {
                ("scanning", Some(p)) => format!(
                    "{} files · {} folders · {}",
                    crate::ui::thousands(p.files.load(Ordering::Relaxed)),
                    crate::ui::thousands(p.dirs.load(Ordering::Relaxed)),
                    fmt::bytes(p.bytes.load(Ordering::Relaxed))
                ),
                ("judging", _) if r.judged.1 > 0 => format!("{}/{} items", r.judged.0, r.judged.1),
                _ => String::new(),
            };
            out.push(Line::from(vec![
                Span::styled(format!(" {} {} ", spinner(), r.phase), bold().patch(accent())),
                Span::raw(detail),
                Span::styled(format!("   {:.1}s", r.started.elapsed().as_secs_f64()), dim()),
            ]));
        }
        match &self.result {
            Some(Ok(c)) => {
                let mut l = vec![
                    Span::styled(format!(" plan {} ", c.plan), bold().patch(fg(Color::Green))),
                    Span::raw(format!("{} · {} reclaimable", plural(c.actions as u64, "action", "actions"), fmt::bytes(c.reclaimable))),
                ];
                let mut extra = vec![];
                if let Some(found) = &c.found {
                    extra.push(found.clone());
                }
                if c.blocked > 0 {
                    extra.push(if c.blocked_bytes > 0 {
                        format!("{} blocked by guards ({})", c.blocked, fmt::bytes(c.blocked_bytes))
                    } else {
                        format!("{} blocked by guards", c.blocked)
                    });
                }
                extra.push(format!("{:.1}s", c.elapsed_ms as f64 / 1000.0));
                l.push(Span::styled(format!(" · {}", extra.join(" · ")), dim()));
                out.push(Line::from(l));
                if let Some(j) = &c.judge {
                    out.push(Line::from(Span::styled(format!(" {j}"), dim())));
                }
                for w in &c.warnings {
                    out.push(Line::from(Span::styled(format!(" ⚠ {w}"), fg(Color::Yellow))));
                }
                if c.actions > 0 {
                    out.push(Line::raw(""));
                    out.push(Line::from(vec![Span::styled(" o ", bold().patch(accent())), Span::raw("review it on the Plans screen")]));
                }
            }
            Some(Err(e)) => out.push(Line::from(Span::styled(format!(" {e}"), fg(Color::Red)))),
            None => {}
        }
        f.render_widget(Paragraph::new(out).wrap(Wrap { trim: false }).block(Block::default().borders(Borders::TOP)), status);
    }
}

/// The judge the policy uses, if any.
fn judge_name(pol: &tdy_rules::Policy) -> String {
    if !pol.judge.cascade.is_empty() {
        return pol.judge.cascade.join(" → ");
    }
    match (&pol.judge.default, pol.judge.backends.len()) {
        (Some(d), _) => d.clone(),
        (None, 1) => pol.judge.backends.keys().next().cloned().unwrap_or_default(),
        _ => String::new(),
    }
}
