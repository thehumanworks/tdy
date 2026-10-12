//! Plan review (ratatui): a component used by `tidy review` on its own and
//! by the Plans screen of `tidy ui`.

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use tidy_plan::review::Filter;
use tidy_plan::{Plan, Status, store};
use tidy_scan::{fmt, path::display_path};

pub struct ReviewView {
    pub plan: Plan,
    base: Filter,
    query: String,
    typing: bool,
    view: Vec<usize>,
    state: TableState,
    detail: bool,
    /// Unsaved status changes (standalone mode).
    pub dirty: bool,
    /// Save after every change instead of on exit (inside `tidy ui`).
    pub autosave: bool,
    pub msg: String,
}

/// What a key did, for the host to act on.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Handled,
    /// Not a review key; the host may use it.
    Unhandled,
    /// q / Esc: leave (the standalone host asks first when there are unsaved changes).
    Close,
    SaveClose,
    DiscardClose,
}

impl ReviewView {
    pub fn new(plan: Plan, base: Filter) -> Self {
        let mut v = ReviewView {
            plan,
            base,
            query: String::new(),
            typing: false,
            view: vec![],
            state: TableState::default(),
            detail: false,
            dirty: false,
            autosave: false,
            msg: String::new(),
        };
        v.refilter();
        v
    }

    /// Swap in a fresh copy of the plan (after apply/undo), keeping the cursor.
    pub fn replace_plan(&mut self, plan: Plan) {
        self.plan = plan;
        self.dirty = false;
        self.refilter();
    }

    pub fn typing(&self) -> bool {
        self.typing
    }

    fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        self.view = self
            .plan
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| self.base.matches(e))
            .filter(|(_, e)| {
                q.is_empty() || e.path.to_string_lossy().to_lowercase().contains(&q) || e.source.rule.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect();
        let sel = self.state.selected().unwrap_or(0).min(self.view.len().saturating_sub(1));
        self.state.select(if self.view.is_empty() { None } else { Some(sel) });
    }

    fn current(&mut self) -> Option<&mut tidy_plan::Entry> {
        let i = *self.view.get(self.state.selected()?)?;
        Some(&mut self.plan.entries[i])
    }

    fn changed(&mut self) {
        if self.autosave {
            if let Err(e) = store::save(&self.plan) {
                self.msg = format!("could not save: {e:#}");
                self.dirty = true;
            }
        } else {
            self.dirty = true;
        }
    }

    fn set(&mut self, st: Status) {
        let mut hit = false;
        if let Some(e) = self.current()
            && e.status.reviewable()
        {
            tidy_plan::review::set_one(e, st, "human");
            hit = true;
        }
        if hit {
            self.changed();
        }
        self.down();
    }

    fn set_all_visible(&mut self, st: Status) {
        let mut n = 0;
        for &i in &self.view {
            let e = &mut self.plan.entries[i];
            if e.status.reviewable() && e.status != st {
                tidy_plan::review::set_one(e, st, "human");
                n += 1;
            }
        }
        if n > 0 {
            self.changed();
        }
        self.msg = format!("{n} entries → {}", st.as_str());
    }

    fn down(&mut self) {
        if let Some(s) = self.state.selected() {
            self.state.select(Some((s + 1).min(self.view.len().saturating_sub(1))));
        }
    }
    fn up(&mut self) {
        if let Some(s) = self.state.selected() {
            self.state.select(Some(s.saturating_sub(1)));
        }
    }

    pub fn handle(&mut self, k: KeyEvent) -> Outcome {
        if self.typing {
            match k.code {
                KeyCode::Enter | KeyCode::Esc => self.typing = false,
                KeyCode::Backspace => {
                    self.query.pop();
                    self.refilter();
                }
                KeyCode::Char(c) => {
                    self.query.push(c);
                    self.refilter();
                }
                _ => {}
            }
            return Outcome::Handled;
        }
        self.msg.clear();
        match (k.code, k.modifiers) {
            (KeyCode::Char('q') | KeyCode::Esc, _) => return Outcome::Close,
            (KeyCode::Char('Q'), _) if !self.autosave => return Outcome::DiscardClose,
            (KeyCode::Char('s'), _) if !self.autosave => return Outcome::SaveClose,
            (KeyCode::Down | KeyCode::Char('j'), _) => self.down(),
            (KeyCode::Up | KeyCode::Char('k'), _) => self.up(),
            (KeyCode::PageDown, _) => (0..20).for_each(|_| self.down()),
            (KeyCode::PageUp, _) => (0..20).for_each(|_| self.up()),
            (KeyCode::Home | KeyCode::Char('g'), _) => self.state.select(if self.view.is_empty() { None } else { Some(0) }),
            (KeyCode::End | KeyCode::Char('G'), _) => self.state.select(self.view.len().checked_sub(1)),
            (KeyCode::Char('a'), _) => self.set(Status::Approved),
            (KeyCode::Char('r') | KeyCode::Char('x'), _) => self.set(Status::Rejected),
            (KeyCode::Char('p') | KeyCode::Char('u'), _) => self.set(Status::Pending),
            (KeyCode::Char(' '), _) => {
                let next = match self.current().map(|e| e.status) {
                    Some(Status::Approved) => Status::Rejected,
                    Some(Status::Rejected) => Status::Pending,
                    _ => Status::Approved,
                };
                self.set(next);
            }
            (KeyCode::Char('A'), _) => self.set_all_visible(Status::Approved),
            (KeyCode::Char('R'), _) => self.set_all_visible(Status::Rejected),
            (KeyCode::Char('/'), _) => self.typing = true,
            (KeyCode::Enter, _) => self.detail = !self.detail,
            (KeyCode::Tab, _) if !self.autosave => self.detail = !self.detail,
            _ => return Outcome::Unhandled,
        }
        Outcome::Handled
    }

    pub fn draw(&mut self, f: &mut ratatui::Frame<'_>, area: Rect, hint: &str) {
        let detail_h = if self.detail { 9 } else { 0 };
        let [head, body, detail, foot] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(3), Constraint::Length(detail_h), Constraint::Length(1)]).areas(area);
        let c = self.plan.counts();
        let approved_bytes: u64 = self.plan.entries.iter().filter(|e| e.status == Status::Approved).map(|e| e.bytes.private).sum();
        let title = Line::from(vec![
            Span::styled(format!(" plan {} ", self.plan.header.id), Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!(
                " {} · approved {} ({}) · rejected {} · pending {} · applied {} · {} reclaimable",
                if c.total == 1 { "1 entry".to_string() } else { format!("{} entries", c.total) },
                c.approved,
                fmt::bytes(approved_bytes),
                c.rejected,
                c.pending,
                c.applied,
                fmt::bytes(c.reclaimable)
            ))
            .dim(),
        ]);
        let filter_line = if self.typing || !self.query.is_empty() {
            Line::from(vec![Span::raw(" filter: ").dim(), Span::raw(self.query.clone()), Span::raw(if self.typing { "▏" } else { "" })])
        } else {
            Line::from(Span::raw(format!(" {} shown", self.view.len())).dim())
        };
        f.render_widget(Paragraph::new(vec![title, filter_line]), head);

        let rows: Vec<Row> = self
            .view
            .iter()
            .map(|&i| {
                let e = &self.plan.entries[i];
                let what = match &e.dest {
                    Some(d) => format!("{} → {}", display_path(&e.path), display_path(d)),
                    None if !e.tags.is_empty() => format!("{} +{}", display_path(&e.path), e.tags.join(",")),
                    None => display_path(&e.path),
                };
                let size = if e.bytes.private > 0 { fmt::bytes(e.bytes.private) } else { String::new() };
                let undo = if e.undo == tidy_plan::Undo::Restore { "↺" } else { "" };
                Row::new(vec![
                    status_cell(e.status),
                    Cell::from(e.id.clone()).dim(),
                    Cell::from(e.action.as_str()),
                    Cell::from(size),
                    Cell::from(what),
                    Cell::from(e.provenance()).dim(),
                    Cell::from(undo),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(10),
                Constraint::Length(6),
                Constraint::Length(10),
                Constraint::Length(9),
                Constraint::Fill(1),
                Constraint::Length(if area.width >= 110 { 24 } else { 12 }),
                Constraint::Length(1),
            ],
        )
        .header(Row::new(vec!["status", "id", "action", "size", "path", "why", ""]).style(Style::default().add_modifier(Modifier::BOLD)))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .block(Block::default().borders(Borders::TOP));
        f.render_stateful_widget(table, body, &mut self.state);

        if self.detail
            && let Some(&i) = self.state.selected().and_then(|s| self.view.get(s))
        {
            let e = &self.plan.entries[i];
            let mut lines = vec![
                Line::from(vec![Span::raw("reason  ").dim(), Span::raw(e.reason.clone())]),
                Line::from(vec![
                    Span::raw("bytes   ").dim(),
                    Span::raw(format!(
                        "{} apparent · {} on disk · {} unshared",
                        fmt::bytes(e.bytes.logical),
                        fmt::bytes(e.bytes.alloc),
                        fmt::bytes(e.bytes.private)
                    )),
                ]),
                Line::from(vec![
                    Span::raw("state   ").dim(),
                    Span::raw(format!(
                        "{}{} · newest change {} ago · undo {}",
                        e.fingerprint.files.map(|n| format!("{n} files")).unwrap_or_else(|| "file".into()),
                        e.fingerprint.hash.as_ref().map(|_| " · hashed").unwrap_or(""),
                        fmt::age(fmt::now_secs() - e.fingerprint.mtime),
                        if e.undo == tidy_plan::Undo::Restore { "✓" } else { "✗ (permanent)" }
                    )),
                ]),
            ];
            if let Some(ev) = &e.evidence {
                lines.push(Line::from(vec![Span::raw("judge   ").dim(), Span::raw(crate::judge_cmds::evidence_summary(ev))]));
            }
            for a in &e.alternatives {
                lines.push(Line::from(vec![
                    Span::raw("alt     ").dim(),
                    Span::raw(format!("{} {} {}", a.rule, a.action.as_str(), a.p.map(|p| format!("{p:.2}")).unwrap_or_default())),
                ]));
            }
            if let Some(err) = e.outcome.as_ref().and_then(|o| o.error.as_ref()) {
                lines.push(Line::from(vec![Span::raw("error   ").dim(), Span::raw(err.clone()).red()]));
            }
            f.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: true }).block(Block::default().borders(Borders::TOP).title(" details ")),
                detail,
            );
        }
        let help = if !self.msg.is_empty() {
            Line::from(Span::raw(format!(" {}", self.msg)).yellow())
        } else {
            Line::from(Span::raw(format!(" a approve · r reject · p pending · A all · / filter · ⏎ details{hint}")).dim())
        };
        f.render_widget(Paragraph::new(help), foot);
    }
}

pub fn status_cell(s: Status) -> Cell<'static> {
    let (t, c) = match s {
        Status::Approved => ("✓ approve", Color::Green),
        Status::Rejected => ("✗ reject", Color::Red),
        Status::Pending => ("· pending", Color::Gray),
        Status::Applied => ("● applied", Color::Cyan),
        Status::Failed => ("! failed", Color::Red),
        Status::Skipped => ("- skipped", Color::Yellow),
        Status::Undone => ("↺ undone", Color::Magenta),
    };
    Cell::from(t).style(Style::default().fg(c))
}

/// `tidy review`: review one plan full-screen; save on `s`.
pub fn review(plan: Plan, base: &Filter) -> Result<()> {
    let mut v = ReviewView::new(plan, base.clone());
    let mut terminal = ratatui::init();
    let res = run(&mut terminal, &mut v);
    ratatui::restore();
    let save = res?;
    if save && v.dirty {
        store::save(&v.plan)?;
        let c = v.plan.counts();
        println!("{}: approved {} · rejected {} · pending {}", v.plan.header.id, c.approved, c.rejected, c.pending);
        if c.approved > 0 {
            println!("next: tidy apply {}  (add --dry-run to rehearse)", v.plan.header.id);
        }
    } else if v.dirty {
        println!("changes discarded");
    }
    Ok(())
}

/// Returns whether to save.
fn run(terminal: &mut ratatui::DefaultTerminal, v: &mut ReviewView) -> Result<bool> {
    loop {
        terminal.draw(|f| v.draw(f, f.area(), " · s save+quit · q quit"))?;
        let Event::Key(k) = event::read()? else { continue };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(false);
        }
        match v.handle(k) {
            Outcome::Close if v.dirty => v.msg = "unsaved changes: s = save & quit, Q = quit without saving".into(),
            Outcome::Close | Outcome::DiscardClose => return Ok(false),
            Outcome::SaveClose => return Ok(true),
            Outcome::Handled | Outcome::Unhandled => {}
        }
    }
}
