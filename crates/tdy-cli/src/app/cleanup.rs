//! Cleanup: run the detectors and the policy rules on one folder, with live
//! progress, and hand the resulting plan to the Plans screen.

use super::worker::{self, CleanupResult};
use super::{Act, Ctx, Dialog, accent, bold, dim, fg, plural, spinner};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Color;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tdy_scan::path::{display_path, expand_tilde};
use tdy_scan::{ScanProgress, fmt};

pub struct Running {
    pub phase: &'static str,
    pub scan: Option<Arc<ScanProgress>>,
    pub judged: (usize, usize),
    pub started: Instant,
}

pub struct Cleanup {
    pub path: String,
    pub editing: bool,
    pub detectors: bool,
    pub rules: bool,
    pub running: Option<Running>,
    pub result: Option<Result<CleanupResult, String>>,
}

impl Cleanup {
    pub fn new(root: &Path) -> Self {
        Cleanup { path: display_path(root), editing: false, detectors: true, rules: true, running: None, result: None }
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
    pub fn on_done(&mut self, r: Result<CleanupResult, String>) -> Act {
        self.running = None;
        let act = match &r {
            Ok(c) if c.actions > 0 => Act::Toast(format!("plan {} ready: o opens it on the Plans screen", c.plan)),
            Ok(_) => Act::Toast("nothing to clean up here".into()),
            Err(e) => Act::Dialog(Dialog { title: "Cleanup failed".into(), lines: vec![Line::raw(e.clone())], confirm: None }),
        };
        self.result = Some(r);
        act
    }

    fn run(&mut self, ctx: &Ctx) -> Act {
        if self.running.is_some() {
            return Act::Toast("already running".into());
        }
        let root = match std::fs::canonicalize(expand_tilde(self.path.trim())) {
            Ok(p) if p.is_dir() => p,
            Ok(p) => return Act::Toast(format!("{} is not a folder", display_path(&p))),
            Err(e) => return Act::Toast(format!("{}: {e}", self.path.trim())),
        };
        if !self.detectors && !self.rules {
            return Act::Toast("turn on detectors (d) or policy rules (p) first".into());
        }
        self.result = None;
        self.running = Some(Running { phase: "starting", scan: None, judged: (0, 0), started: Instant::now() });
        worker::cleanup(ctx.tx.clone(), root, self.detectors, self.rules);
        Act::None
    }

    /// Returns (handled, action).
    pub fn on_key(&mut self, k: KeyEvent, ctx: &Ctx) -> (bool, Act) {
        if self.editing {
            match k.code {
                KeyCode::Enter | KeyCode::Esc => self.editing = false,
                KeyCode::Backspace => {
                    self.path.pop();
                }
                KeyCode::Char(c) => self.path.push(c),
                _ => {}
            }
            return (true, Act::None);
        }
        let a = match k.code {
            KeyCode::Char('e') if self.running.is_none() => {
                self.editing = true;
                Act::None
            }
            KeyCode::Char('d') if self.running.is_none() => {
                self.detectors = !self.detectors;
                Act::None
            }
            KeyCode::Char('p') if self.running.is_none() => {
                self.rules = !self.rules;
                Act::None
            }
            KeyCode::Enter => self.run(ctx),
            KeyCode::Char('o') => match &self.result {
                Some(Ok(c)) if c.actions > 0 => Act::OpenPlan(c.plan.clone()),
                _ => Act::None,
            },
            KeyCode::Esc => {
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
        if self.editing {
            return "type a folder · ⏎ done".into();
        }
        if self.running.is_some() {
            return "working… · Esc cancels the scan".into();
        }
        "⏎ run · e edit folder · d detectors · p policy rules · o open result".into()
    }

    pub fn draw(&self, f: &mut Frame<'_>, area: Rect, ctx: &Ctx) {
        let [form, status] = Layout::vertical([Constraint::Length(9), Constraint::Min(3)]).areas(area);
        let check = |on: bool| if on { Span::styled("[x] ", accent()) } else { Span::styled("[ ] ", dim()) };
        let k = |s: &str| Span::styled(format!(" {s:<11}"), dim());
        let pol = &ctx.policy;
        let judge = match (&pol.judge.default, pol.judge.backends.len()) {
            (Some(d), _) => d.clone(),
            (None, 1) => pol.judge.backends.keys().next().cloned().unwrap_or_default(),
            _ => String::new(),
        };
        let needs_judge = pol.rules.iter().any(|r| !r.when.is_empty());
        let mut policy_line = vec![check(self.rules), Span::raw(format!("{} rules", pol.rules.len()))];
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
        let lines = vec![
            Line::from(Span::styled(" Detectors find build output and caches; your policy rules judge the rest.", dim())),
            Line::raw(""),
            Line::from(vec![
                k("folder"),
                Span::styled(self.path.clone(), if self.editing { bold().patch(accent()) } else { bold() }),
                Span::raw(if self.editing { "▏" } else { "" }),
                Span::styled(if self.editing { "" } else { "   e edit" }, dim()),
            ]),
            Line::from(vec![k("detectors"), check(self.detectors), Span::raw("build artefacts and caches"), Span::styled("   d", dim())]),
            Line::from({
                let mut v = vec![k("policy")];
                v.extend(policy_line);
                v.push(Span::styled("   p", dim()));
                v
            }),
            Line::raw(""),
            Line::from(Span::styled(" The result is a plan: nothing changes until you apply it (Plans).", dim())),
        ];
        f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::TOP)), form);

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
                out.push(Line::from(vec![
                    Span::styled(format!(" plan {} ", c.plan), bold().patch(fg(Color::Green))),
                    Span::raw(format!("{} · {} reclaimable", plural(c.actions as u64, "action", "actions"), fmt::bytes(c.reclaimable))),
                    Span::styled(
                        format!(
                            " · {} by guards ({}) · {:.1}s",
                            plural(c.blocked as u64, "blocked", "blocked"),
                            fmt::bytes(c.blocked_bytes),
                            c.elapsed_ms as f64 / 1000.0
                        ),
                        dim(),
                    ),
                ]));
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
