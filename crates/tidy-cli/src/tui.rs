//! Interactive plan review (ratatui).

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use tidy_plan::review::Filter;
use tidy_plan::{store, Plan, Status};
use tidy_scan::{fmt, path::display_path};

struct App<'a> {
    plan: &'a mut Plan,
    base: Filter,
    query: String,
    typing: bool,
    view: Vec<usize>,
    state: TableState,
    detail: bool,
    dirty: bool,
    msg: String,
}

impl App<'_> {
    fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        self.view = self
            .plan
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| self.base.matches(e))
            .filter(|(_, e)| q.is_empty() || e.path.to_string_lossy().to_lowercase().contains(&q) || e.source.rule.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
        let sel = self.state.selected().unwrap_or(0).min(self.view.len().saturating_sub(1));
        self.state.select(if self.view.is_empty() { None } else { Some(sel) });
    }

    fn current(&mut self) -> Option<&mut tidy_plan::Entry> {
        let i = *self.view.get(self.state.selected()?)?;
        Some(&mut self.plan.entries[i])
    }

    fn set(&mut self, st: Status) {
        if let Some(e) = self.current() {
            if e.status.reviewable() {
                e.status = st;
                self.dirty = true;
            }
        }
        self.down();
    }

    fn set_all_visible(&mut self, st: Status) {
        let mut n = 0;
        for &i in &self.view {
            let e = &mut self.plan.entries[i];
            if e.status.reviewable() && e.status != st {
                e.status = st;
                n += 1;
            }
        }
        if n > 0 {
            self.dirty = true;
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
}

pub fn review(plan: &mut Plan, base: &Filter) -> Result<()> {
    let mut app = App { plan, base: base.clone(), query: String::new(), typing: false, view: vec![], state: TableState::default(), detail: false, dirty: false, msg: String::new() };
    app.refilter();
    let mut terminal = ratatui::init();
    let res = run(&mut terminal, &mut app);
    ratatui::restore();
    let save = res?;
    if save && app.dirty {
        store::save(app.plan)?;
        let c = app.plan.counts();
        println!("{}: approved {} · rejected {} · pending {}", app.plan.header.id, c.approved, c.rejected, c.pending);
        if c.approved > 0 {
            println!("next: tidy apply {}  (add --dry-run to rehearse)", app.plan.header.id);
        }
    } else if app.dirty {
        println!("changes discarded");
    }
    Ok(())
}

/// Returns whether to save.
fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App<'_>) -> Result<bool> {
    loop {
        terminal.draw(|f| draw(f, app))?;
        let Event::Key(k) = event::read()? else { continue };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        if app.typing {
            match k.code {
                KeyCode::Enter | KeyCode::Esc => app.typing = false,
                KeyCode::Backspace => {
                    app.query.pop();
                    app.refilter();
                }
                KeyCode::Char(c) => {
                    app.query.push(c);
                    app.refilter();
                }
                _ => {}
            }
            continue;
        }
        app.msg.clear();
        match (k.code, k.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(false),
            (KeyCode::Char('q') | KeyCode::Esc, _) => {
                if app.dirty {
                    app.msg = "unsaved changes: s = save & quit, Q = quit without saving".into();
                } else {
                    return Ok(false);
                }
            }
            (KeyCode::Char('Q'), _) => return Ok(false),
            (KeyCode::Char('s'), _) => return Ok(true),
            (KeyCode::Down | KeyCode::Char('j'), _) => app.down(),
            (KeyCode::Up | KeyCode::Char('k'), _) => app.up(),
            (KeyCode::PageDown, _) => (0..20).for_each(|_| app.down()),
            (KeyCode::PageUp, _) => (0..20).for_each(|_| app.up()),
            (KeyCode::Home | KeyCode::Char('g'), _) => app.state.select(if app.view.is_empty() { None } else { Some(0) }),
            (KeyCode::End | KeyCode::Char('G'), _) => app.state.select(app.view.len().checked_sub(1)),
            (KeyCode::Char('a'), _) => app.set(Status::Approved),
            (KeyCode::Char('r') | KeyCode::Char('x'), _) => app.set(Status::Rejected),
            (KeyCode::Char('p') | KeyCode::Char('u'), _) => app.set(Status::Pending),
            (KeyCode::Char(' '), _) => {
                let next = match app.current().map(|e| e.status) {
                    Some(Status::Approved) => Status::Rejected,
                    Some(Status::Rejected) => Status::Pending,
                    _ => Status::Approved,
                };
                app.set(next);
            }
            (KeyCode::Char('A'), _) => app.set_all_visible(Status::Approved),
            (KeyCode::Char('R'), _) => app.set_all_visible(Status::Rejected),
            (KeyCode::Char('/'), _) => {
                app.typing = true;
            }
            (KeyCode::Enter | KeyCode::Tab, _) => app.detail = !app.detail,
            _ => {}
        }
    }
}

fn status_cell(s: Status) -> Cell<'static> {
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

fn draw(f: &mut ratatui::Frame<'_>, app: &mut App<'_>) {
    let detail_h = if app.detail { 9 } else { 0 };
    let [head, body, detail, foot] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(3), Constraint::Length(detail_h), Constraint::Length(1)]).areas(f.area());
    let c = app.plan.counts();
    let approved_bytes: u64 = app.plan.entries.iter().filter(|e| e.status == Status::Approved).map(|e| e.bytes.private).sum();
    let title = Line::from(vec![
        Span::styled(format!("tidy review {} ", app.plan.header.id), Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!(
            " {} entries · approved {} ({}) · rejected {} · pending {} · {} reclaimable",
            c.total,
            c.approved,
            fmt::bytes(approved_bytes),
            c.rejected,
            c.pending,
            fmt::bytes(c.reclaimable)
        ))
        .dim(),
    ]);
    let filter_line = if app.typing || !app.query.is_empty() {
        Line::from(vec![Span::raw(" filter: ").dim(), Span::raw(app.query.clone()), Span::raw(if app.typing { "▏" } else { "" })])
    } else {
        Line::from(Span::raw(format!(" {} shown", app.view.len())).dim())
    };
    f.render_widget(Paragraph::new(vec![title, filter_line]), head);

    let rows: Vec<Row> = app
        .view
        .iter()
        .map(|&i| {
            let e = &app.plan.entries[i];
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
        [Constraint::Length(10), Constraint::Length(6), Constraint::Length(10), Constraint::Length(9), Constraint::Fill(1), Constraint::Length(24), Constraint::Length(1)],
    )
    .header(Row::new(vec!["status", "id", "action", "size", "path", "why", ""]).style(Style::default().add_modifier(Modifier::BOLD)))
    .row_highlight_style(Style::default().bg(Color::DarkGray))
    .block(Block::default().borders(Borders::TOP));
    f.render_stateful_widget(table, body, &mut app.state);

    if app.detail {
        if let Some(&i) = app.state.selected().and_then(|s| app.view.get(s)) {
            let e = &app.plan.entries[i];
            let mut lines = vec![
                Line::from(vec![Span::raw("reason  ").dim(), Span::raw(e.reason.clone())]),
                Line::from(vec![
                    Span::raw("bytes   ").dim(),
                    Span::raw(format!("{} apparent · {} on disk · {} unshared", fmt::bytes(e.bytes.logical), fmt::bytes(e.bytes.alloc), fmt::bytes(e.bytes.private))),
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
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(Block::default().borders(Borders::TOP).title(" details ")), detail);
        }
    }
    let help = if !app.msg.is_empty() {
        Line::from(Span::raw(format!(" {}", app.msg)).yellow())
    } else {
        Line::from(Span::raw(" a approve · r reject · p pending · space cycle · A/R all shown · / filter · ⏎ details · s save+quit · q quit").dim())
    };
    f.render_widget(Paragraph::new(help), foot);
}
