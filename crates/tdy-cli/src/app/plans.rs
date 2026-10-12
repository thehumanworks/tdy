//! Plans: the list of plans, review of one plan, and apply/undo with a
//! dry-run preview and a confirmation.

use super::worker::{self, ExecOp};
use super::{Act, Confirm, Ctx, Dialog, bold, dim, fg, plural, spinner};
use crate::plan_cmds::EventKind;
use crate::tui::{Outcome, ReviewView};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use tdy_plan::review::Filter;
use tdy_plan::{Counts, Mode, Status, store};
use tdy_scan::fmt;
use tdy_scan::path::display_path;

pub struct PlanRow {
    pub id: String,
    pub created: i64,
    pub roots: String,
    pub counts: Counts,
    pub note: Option<String>,
    pub mode: Mode,
}

pub struct Plans {
    pub rows: Vec<PlanRow>,
    pub state: TableState,
    pub review: Option<ReviewView>,
    pub log: Vec<(EventKind, String)>,
    pub show_log: bool,
    /// A job running on a plan.
    pub busy: Option<(String, ExecOp)>,
    pub error: Option<String>,
}

impl Plans {
    pub fn new() -> Self {
        let mut p =
            Plans { rows: vec![], state: TableState::default(), review: None, log: vec![], show_log: false, busy: None, error: None };
        p.refresh();
        p
    }

    pub fn refresh(&mut self) {
        match store::list() {
            Ok(mut plans) => {
                plans.sort_by_key(|p| std::cmp::Reverse(p.header.created));
                self.rows = plans
                    .iter()
                    .map(|p| PlanRow {
                        id: p.header.id.clone(),
                        created: p.header.created,
                        roots: p.header.roots.iter().map(|r| display_path(r)).collect::<Vec<_>>().join(", "),
                        counts: p.counts(),
                        note: p.header.note.clone(),
                        mode: p.header.mode,
                    })
                    .collect();
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
        let sel = self.state.selected().unwrap_or(0).min(self.rows.len().saturating_sub(1));
        self.state.select(if self.rows.is_empty() { None } else { Some(sel) });
    }

    pub fn open(&mut self, id: &str) -> anyhow::Result<()> {
        let (plan, _) = store::load(Some(id))?;
        let mut v = ReviewView::new(plan, Filter::default());
        v.autosave = true;
        self.review = Some(v);
        // A running job keeps its log (its dry-run summary reads it).
        if self.busy.is_none() {
            self.log.clear();
            self.show_log = false;
        }
        self.refresh();
        if let Some(i) = self.rows.iter().position(|r| r.id == id) {
            self.state.select(Some(i));
        }
        Ok(())
    }

    pub fn typing(&self) -> bool {
        self.review.as_ref().is_some_and(|r| r.typing())
    }

    fn reload(&mut self, id: &str) {
        if let Some(v) = &mut self.review
            && v.plan.header.id == id
            && let Ok((plan, _)) = store::load(Some(id))
        {
            v.replace_plan(plan);
        }
        self.refresh();
    }

    pub fn start(&mut self, ctx: &Ctx, id: String, op: ExecOp) {
        if let Some((p, _)) = &self.busy {
            self.error = Some(format!("plan {p} is busy"));
            return;
        }
        self.log.clear();
        self.show_log = true;
        self.busy = Some((id.clone(), op));
        worker::exec(ctx.tx.clone(), id, op);
    }

    pub fn on_line(&mut self, _plan: &str, kind: EventKind, text: String) {
        self.log.push((kind, text));
        if self.log.len() > 2000 {
            self.log.drain(..500);
        }
    }

    pub fn on_done(&mut self, id: String, op: ExecOp, result: Result<tdy_plan::Report, String>) -> Act {
        self.busy = None;
        let r = match result {
            Ok(r) => r,
            Err(e) => {
                self.reload(&id);
                return Act::Dialog(Dialog { title: format!("{} failed", op_name(op)), lines: vec![Line::raw(e)], confirm: None });
            }
        };
        match op {
            ExecOp::DryRun => {
                let skipped: Vec<&(EventKind, String)> = self.log.iter().filter(|(k, _)| *k == EventKind::Skip).collect();
                let mut lines = vec![Line::from(vec![
                    Span::styled(format!("{} would apply", plural(r.would as u64, "entry", "entries")), bold()),
                    Span::raw(if r.skipped > 0 { format!(", {} would be skipped", r.skipped) } else { String::new() }),
                ])];
                for (_, t) in skipped.iter().take(8) {
                    lines.push(Line::from(Span::styled(format!("  - {t}"), fg(Color::Yellow))));
                }
                if skipped.len() > 8 {
                    lines.push(Line::from(Span::styled(format!("  … {} more (L shows the log)", skipped.len() - 8), dim())));
                }
                if r.would == 0 {
                    return Act::Dialog(Dialog { title: format!("Nothing to apply in {id}"), lines, confirm: None });
                }
                lines.push(Line::raw(""));
                lines.push(Line::raw("Fingerprints and guards are checked again for every entry while applying."));
                Act::Dialog(Dialog { title: format!("Apply plan {id}?"), lines, confirm: Some(Confirm::Apply(id)) })
            }
            ExecOp::Apply | ExecOp::Undo => {
                self.reload(&id);
                let mut parts = vec![];
                if op == ExecOp::Apply {
                    parts.push(format!("{} applied", r.applied));
                } else {
                    parts.push(format!("{} restored", r.undone));
                }
                if r.skipped > 0 {
                    parts.push(format!("{} skipped", r.skipped));
                }
                if r.failed > 0 {
                    parts.push(format!("{} failed", r.failed));
                }
                if r.freed > 0 {
                    parts.push(format!("{} freed", fmt::bytes(r.freed)));
                }
                if r.parked > 0 {
                    parts.push(format!("{} in trash/quarantine", fmt::bytes(r.parked)));
                }
                Act::Toast(format!("{id}: {}", parts.join(" · ")))
            }
        }
    }

    /// Returns (handled, action).
    pub fn on_key(&mut self, k: KeyEvent, ctx: &Ctx) -> (bool, Act) {
        if let Some(v) = &mut self.review {
            let id = v.plan.header.id.clone();
            // While a job runs on this plan, the file belongs to it: no edits.
            let edits = matches!(k.code, KeyCode::Char('a' | 'r' | 'x' | 'p' | 'u' | ' ' | 'A' | 'R'));
            if edits && !v.typing() && self.busy.as_ref().is_some_and(|(p, _)| *p == id) {
                return (true, Act::Toast("wait for the job on this plan to finish".into()));
            }
            match v.handle(k) {
                Outcome::Handled => return (true, Act::None),
                Outcome::Close | Outcome::SaveClose | Outcome::DiscardClose => {
                    if self.show_log && self.busy.is_none() && !self.log.is_empty() && k.code == KeyCode::Esc {
                        self.show_log = false;
                        return (true, Act::None);
                    }
                    self.review = None;
                    self.refresh();
                    return (true, Act::None);
                }
                Outcome::Unhandled => {}
            }
            let busy = self.busy.is_some();
            return match k.code {
                KeyCode::Char('L') => {
                    self.show_log = !self.show_log;
                    (true, Act::None)
                }
                KeyCode::Char('X') if busy => (true, Act::Toast("a job is already running".into())),
                KeyCode::Char('X') => {
                    let c = v.plan.counts();
                    if c.approved == 0 {
                        return (true, Act::Toast("nothing approved yet — a approves an entry, A everything shown".into()));
                    }
                    self.start(ctx, id, ExecOp::DryRun);
                    (true, Act::Toast("dry run…".into()))
                }
                KeyCode::Char('U') if busy => (true, Act::Toast("a job is already running".into())),
                KeyCode::Char('U') => {
                    let applied: Vec<&tdy_plan::Entry> = v.plan.entries.iter().filter(|e| e.status == Status::Applied).collect();
                    if applied.is_empty() {
                        return (true, Act::Toast("nothing applied in this plan".into()));
                    }
                    let permanent = applied.iter().filter(|e| e.undo != tdy_plan::Undo::Restore).count();
                    let mut lines =
                        vec![Line::raw(format!("Restore {} applied entr{}.", applied.len(), if applied.len() == 1 { "y" } else { "ies" }))];
                    if permanent > 0 {
                        lines.push(Line::from(Span::styled(
                            format!("{permanent} cannot be undone (deletes, dedupes, compression) and will be skipped."),
                            fg(Color::Yellow),
                        )));
                    }
                    (true, Act::Dialog(Dialog { title: format!("Undo plan {id}?"), lines, confirm: Some(Confirm::Undo(id)) }))
                }
                _ => (false, Act::None),
            };
        }
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(s) = self.state.selected() {
                    self.state.select(Some((s + 1).min(self.rows.len().saturating_sub(1))));
                }
                (true, Act::None)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(s) = self.state.selected() {
                    self.state.select(Some(s.saturating_sub(1)));
                }
                (true, Act::None)
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                let Some(id) = self.state.selected().and_then(|i| self.rows.get(i)).map(|r| r.id.clone()) else {
                    return (true, Act::None);
                };
                match self.open(&id) {
                    Ok(()) => (true, Act::None),
                    Err(e) => (true, Act::Toast(format!("cannot open {id}: {e:#}"))),
                }
            }
            KeyCode::Char('r') => {
                self.refresh();
                (true, Act::None)
            }
            KeyCode::Char('n') => (true, Act::NewPlan(super::newplan::Start { kind: None, folder: None, run: false })),
            _ => (false, Act::None),
        }
    }

    pub fn hints(&self) -> String {
        match &self.review {
            Some(_) if self.busy.is_some() => "working… · L log".into(),
            Some(_) => "X apply (dry run first) · U undo · L log · Esc back".into(),
            None => "⏎ open · n new plan · r refresh".into(),
        }
    }

    pub fn draw(&mut self, f: &mut Frame<'_>, area: Rect) {
        let log_h = if self.show_log && (!self.log.is_empty() || self.busy.is_some()) { 10 } else { 0 };
        let [main, log] = Layout::vertical([Constraint::Min(5), Constraint::Length(log_h)]).areas(area);
        if let Some(v) = &mut self.review {
            v.draw(f, main, "");
        } else {
            self.draw_list(f, main);
        }
        if log_h > 0 {
            self.draw_log(f, log);
        }
    }

    fn draw_list(&mut self, f: &mut Frame<'_>, area: Rect) {
        let [head, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(area);
        let title = match &self.error {
            Some(e) => Line::from(Span::styled(format!(" {e}"), fg(Color::Red))),
            None if self.rows.is_empty() => Line::from(Span::styled(
                " No plans yet. n makes one (cleanup, a request in plain English, duplicates, archives), or mark items on Explore and press t.",
                dim(),
            )),
            None => Line::from(vec![
                Span::styled(format!(" {} plan{}", self.rows.len(), if self.rows.len() == 1 { "" } else { "s" }), bold()),
                Span::styled("  newest first", dim()),
            ]),
        };
        f.render_widget(Paragraph::new(title), head);
        let now = fmt::now_secs();
        let rows: Vec<Row> = self
            .rows
            .iter()
            .map(|p| {
                let c = &p.counts;
                let state = if c.pending > 0 {
                    Cell::from(format!("{} to review", c.pending)).style(fg(Color::Yellow))
                } else if c.approved > 0 {
                    Cell::from(format!("{} ready", c.approved)).style(fg(Color::Green))
                } else if c.applied > 0 {
                    Cell::from(format!("{} applied", c.applied)).style(fg(Color::Cyan))
                } else {
                    Cell::from("done").style(dim())
                };
                let what = match &p.note {
                    Some(n) => format!("{}  ({n})", p.roots),
                    None => p.roots.clone(),
                };
                Row::new(vec![
                    Cell::from(p.id.clone()).style(bold()),
                    Cell::from(if now - p.created < 60 {
                        "  just now".to_string()
                    } else {
                        format!("{:>5} ago", fmt::age(now - p.created))
                    })
                    .style(dim()),
                    Cell::from(format!("{:>4}", c.total)),
                    Cell::from(format!("{:>9}", fmt::bytes(c.reclaimable))),
                    state,
                    Cell::from(if p.mode == Mode::Autonomous { "auto" } else { "" }).style(dim()),
                    Cell::from(what),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(9),
                Constraint::Length(10),
                Constraint::Length(5),
                Constraint::Length(10),
                Constraint::Length(14),
                Constraint::Length(5),
                Constraint::Fill(1),
            ],
        )
        .header(Row::new(vec!["plan", "created", "  n", "reclaim", "state", "", "roots"]).style(dim()))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .block(Block::default().borders(Borders::TOP));
        f.render_stateful_widget(table, body, &mut self.state);
    }

    fn draw_log(&self, f: &mut Frame<'_>, area: Rect) {
        let title = match &self.busy {
            Some((id, op)) => format!(" {} {} {id} ", spinner(), op_name(*op)),
            None => " log (L hides) ".into(),
        };
        let h = area.height.saturating_sub(1) as usize;
        let lines: Vec<Line> = self.log[self.log.len().saturating_sub(h)..]
            .iter()
            .map(|(k, t)| {
                let (mark, style) = match k {
                    EventKind::Ok => ("✓", fg(Color::Green)),
                    EventKind::Skip => ("-", fg(Color::Yellow)),
                    EventKind::Fail => ("✗", fg(Color::Red)),
                    EventKind::Would => ("·", dim()),
                    EventKind::Note => (" ", dim()),
                };
                Line::from(vec![Span::styled(format!(" {mark} "), style), Span::raw(t.clone())])
            })
            .collect();
        f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::TOP).title(Span::styled(title, bold()))), area);
    }
}

fn op_name(op: ExecOp) -> &'static str {
    match op {
        ExecOp::DryRun => "dry run",
        ExecOp::Apply => "apply",
        ExecOp::Undo => "undo",
    }
}
