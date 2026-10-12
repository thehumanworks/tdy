//! `tdy ui`: a full-screen app with three screens.
//!
//! - **Explore** browses the disk on the fast scanner and turns marked
//!   items into a plan.
//! - **Plans** reviews plans, applies them (after a dry run and a
//!   confirmation) and undoes them.
//! - **New plan** makes a plan for a folder: a cleanup (detectors and
//!   policy rules), a request in plain English, duplicates or archives.
//!
//! Disk and network work runs on background threads (`worker`); the screen
//! only ever draws state and reacts to keys.

mod explore;
mod newplan;
mod plans;
mod worker;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};
use tdy_plan::{Action, Bytes};
use tdy_rules::Policy;
use tdy_rules::guards::Guards;
use tdy_scan::path::{display_path, expand_tilde};
use worker::Msg;

// ---- look ------------------------------------------------------------------

static COLOR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Foreground colour, unless NO_COLOR is set.
pub(crate) fn fg(c: Color) -> Style {
    if *COLOR.get_or_init(|| std::env::var_os("NO_COLOR").is_none()) { Style::default().fg(c) } else { Style::default() }
}
pub(crate) fn accent() -> Style {
    fg(Color::Cyan)
}
pub(crate) fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}
pub(crate) fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// `1 file`, `2 files`, `1,234 files`.
pub(crate) fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", crate::ui::thousands(n), if n == 1 { one } else { many })
}

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
pub(crate) fn spinner() -> &'static str {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
    SPINNER[(t / 100) as usize % SPINNER.len()]
}

// ---- shared types ------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Explore,
    Plans,
    NewPlan,
}

impl Tab {
    const ALL: [Tab; 3] = [Tab::Explore, Tab::Plans, Tab::NewPlan];
    fn title(self) -> &'static str {
        match self {
            Tab::Explore => "Explore",
            Tab::Plans => "Plans",
            Tab::NewPlan => "New plan",
        }
    }
}

/// Things a screen asks the app to do.
pub enum Act {
    None,
    Dialog(Dialog),
    Toast(String),
    /// Open this plan on the Plans screen.
    OpenPlan(String),
    /// Go to the New plan screen.
    NewPlan(newplan::Start),
}

pub enum Confirm {
    Propose(Action, Vec<(PathBuf, Bytes)>),
    Apply(String),
    Undo(String),
}

pub struct Dialog {
    pub title: String,
    pub lines: Vec<Line<'static>>,
    /// `None`: information only, any key closes it.
    pub confirm: Option<Confirm>,
}

/// What background jobs need.
pub struct Ctx {
    pub tx: Sender<Msg>,
    pub policy: Arc<Policy>,
    /// Shared for explanations (open-file and git probes are cached per instance).
    pub guards: Arc<Guards>,
}

pub struct App {
    pub tab: Tab,
    pub explore: explore::Explore,
    pub plans: plans::Plans,
    pub newplan: newplan::NewPlan,
    pub ctx: Ctx,
    rx: Receiver<Msg>,
    pub dialog: Option<Dialog>,
    /// Dialogs waiting their turn: one never replaces another, and dialogs
    /// from background jobs wait until no text is being typed.
    queue: std::collections::VecDeque<(Dialog, bool)>,
    /// Keys are ignored until then, so a dialog that pops up from a
    /// background job can't catch a keystroke meant for something else.
    armed_at: Option<Instant>,
    /// How long a background dialog ignores keys after it appears.
    arm: Duration,
    help: bool,
    toast: Option<(String, Instant)>,
    pub quit: bool,
}

impl App {
    pub fn new(root: PathBuf) -> Self {
        let (tx, rx) = channel();
        let (policy, warning) = match crate::plan_cmds::load_policy(None) {
            Ok((p, _, _)) => (p, None),
            Err(e) => (Policy::default(), Some(format!("policy not loaded: {e:#}"))),
        };
        let policy = Arc::new(policy);
        let guards = Arc::new(Guards::new(policy.guard_config()));
        let ctx = Ctx { tx, policy, guards };
        let mut app = App {
            tab: Tab::Explore,
            explore: explore::Explore::new(root.clone()),
            plans: plans::Plans::new(),
            newplan: newplan::NewPlan::new(&root),
            ctx,
            rx,
            dialog: None,
            queue: Default::default(),
            armed_at: None,
            arm: Duration::from_millis(400),
            help: false,
            toast: warning.map(|w| (w, Instant::now())),
            quit: false,
        };
        app.explore.open(&app.ctx);
        app
    }

    pub fn busy(&self) -> bool {
        self.explore.scanning.is_some() || self.plans.busy.is_some() || self.newplan.running.is_some()
    }

    fn toast(&mut self, s: impl Into<String>) {
        self.toast = Some((s.into(), Instant::now()));
    }

    /// `background`: the action comes from a finished job, not a key the
    /// user just pressed.
    fn act(&mut self, a: Act, background: bool) {
        match a {
            Act::None => {}
            Act::Dialog(d) => {
                self.queue.push_back((d, background));
                self.promote();
            }
            Act::Toast(s) => self.toast(s),
            // A finished job only jumps to the plan when nothing else is going
            // on (and not away from a plan being reviewed).
            Act::OpenPlan(id) if background && (self.tab == Tab::Plans || self.typing() || self.dialog.is_some() || self.help) => {
                self.toast(format!("plan {id} is ready — 2 opens the Plans screen"));
                self.plans.refresh();
            }
            Act::NewPlan(start) => {
                self.tab = Tab::NewPlan;
                self.help = false;
                let a = self.newplan.start(start, &self.ctx);
                self.act(a, background);
            }
            Act::OpenPlan(id) => {
                self.tab = Tab::Plans;
                if let Err(e) = self.plans.open(&id) {
                    self.toast(format!("cannot open plan {id}: {e:#}"));
                }
            }
        }
    }

    /// Show the next queued dialog when the screen is free. Dialogs from
    /// background jobs ignore keys briefly, so a keystroke already on its way
    /// can't answer them.
    fn promote(&mut self) {
        if self.dialog.is_none()
            && !self.typing()
            && !self.help
            && let Some((d, background)) = self.queue.pop_front()
        {
            self.armed_at = background.then(|| Instant::now() + self.arm);
            self.dialog = Some(d);
        }
    }

    /// A screen is taking text input: keys go to it, not to global shortcuts.
    fn typing(&self) -> bool {
        match self.tab {
            Tab::Explore => self.explore.typing,
            Tab::Plans => self.plans.typing(),
            Tab::NewPlan => self.newplan.editing.is_some(),
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) {
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            self.request_quit();
            return;
        }
        if self.dialog.is_some() && self.armed_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        if let Some(d) = self.dialog.take() {
            match (k.code, d.confirm) {
                (KeyCode::Char('y') | KeyCode::Char('Y'), Some(c)) => self.confirmed(c),
                (KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q'), Some(_)) => {}
                (_, Some(c)) => {
                    self.dialog = Some(Dialog { title: d.title, lines: d.lines, confirm: Some(c) });
                    return;
                }
                (_, None) => {}
            }
            self.promote();
            return;
        }
        if self.help {
            self.help = false;
            self.promote();
            return;
        }
        if !self.typing() {
            match k.code {
                KeyCode::Char('?') => {
                    self.help = true;
                    return;
                }
                KeyCode::Char('1') => return self.switch(Tab::Explore),
                KeyCode::Char('2') => return self.switch(Tab::Plans),
                KeyCode::Char('3') => return self.switch(Tab::NewPlan),
                KeyCode::Tab => {
                    let i = Tab::ALL.iter().position(|t| *t == self.tab).unwrap_or(0);
                    return self.switch(Tab::ALL[(i + 1) % Tab::ALL.len()]);
                }
                KeyCode::BackTab => {
                    let i = Tab::ALL.iter().position(|t| *t == self.tab).unwrap_or(0);
                    return self.switch(Tab::ALL[(i + Tab::ALL.len() - 1) % Tab::ALL.len()]);
                }
                _ => {}
            }
        }
        let (handled, a) = match self.tab {
            Tab::Explore => self.explore.on_key(k, &self.ctx),
            Tab::Plans => self.plans.on_key(k, &self.ctx),
            Tab::NewPlan => self.newplan.on_key(k, &self.ctx),
        };
        self.act(a, false);
        // typing may just have ended: a waiting dialog can show now
        self.promote();
        if !handled && matches!(k.code, KeyCode::Char('q')) {
            self.request_quit();
        }
    }

    fn switch(&mut self, t: Tab) {
        self.tab = t;
        self.promote();
        if t == Tab::Plans && self.plans.review.is_none() {
            self.plans.refresh();
        }
    }

    fn request_quit(&mut self) {
        if matches!(self.plans.busy, Some((_, worker::ExecOp::Apply | worker::ExecOp::Undo))) {
            self.toast("an apply or undo is running — wait for it to finish");
            return;
        }
        if let Some((p, _)) = &self.explore.scanning {
            p.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.quit = true;
    }

    fn confirmed(&mut self, c: Confirm) {
        match c {
            Confirm::Propose(action, items) => {
                self.toast(format!("checking {} item(s)…", items.len()));
                worker::propose(self.ctx.tx.clone(), action, items);
            }
            Confirm::Apply(id) => self.plans.start(&self.ctx, id, worker::ExecOp::Apply),
            Confirm::Undo(id) => self.plans.start(&self.ctx, id, worker::ExecOp::Undo),
        }
    }

    pub fn on_msg(&mut self, m: Msg) {
        let a = match m {
            Msg::Scanned { seq, result } => self.explore.on_scanned(seq, result),
            Msg::Cached { seq, scanned } => self.explore.on_cached(seq, scanned, &self.ctx),
            Msg::Explained { path, result } => {
                self.explore.on_explained(path, result);
                Act::None
            }
            Msg::Rules { path, result } => {
                self.explore.on_rules(path, result);
                Act::None
            }
            Msg::Proposed { action, result } => self.explore.on_proposed(action, result),
            Msg::PlanScanning(p) => {
                self.newplan.on_scanning(p);
                Act::None
            }
            Msg::PlanPhase(p) => {
                self.newplan.on_phase(p);
                Act::None
            }
            Msg::PlanJudging(d, t) => {
                self.newplan.on_judging(d, t);
                Act::None
            }
            Msg::PlanDone(r) => {
                let a = self.newplan.on_done(r);
                self.plans.refresh();
                a
            }
            Msg::ExecLine { plan, kind, text } => {
                self.plans.on_line(&plan, kind, text);
                Act::None
            }
            Msg::ExecDone { plan, op, result, synced } => {
                // (a failed run may still have changed some entries)
                if op != worker::ExecOp::DryRun {
                    self.explore.after_change(&self.ctx, synced);
                }
                self.plans.on_done(plan, op, result)
            }
        };
        self.act(a, true);
    }

    pub fn draw(&mut self, f: &mut Frame<'_>) {
        let [top, body, bottom] = Layout::vertical([Constraint::Length(1), Constraint::Min(5), Constraint::Length(1)]).areas(f.area());
        // tab bar
        let mut spans = vec![Span::styled(" tdy ", bold().patch(accent())), Span::raw(" ")];
        for (i, t) in Tab::ALL.iter().enumerate() {
            let label = format!(" {} {} ", i + 1, t.title());
            spans.push(if *t == self.tab {
                Span::styled(label, Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD))
            } else {
                Span::styled(label, dim())
            });
            spans.push(Span::raw(" "));
        }
        if self.busy() {
            spans.push(Span::styled(format!(" {} ", spinner()), accent()));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), top);
        let help = Paragraph::new(Line::from(Span::styled("? help · q quit ", dim()))).alignment(ratatui::layout::Alignment::Right);
        f.render_widget(help, top);

        match self.tab {
            Tab::Explore => self.explore.draw(f, body, &self.ctx),
            Tab::Plans => self.plans.draw(f, body),
            Tab::NewPlan => self.newplan.draw(f, body, &self.ctx),
        }

        // bottom: toast, else the screen's key hints
        if let Some((t, at)) = &self.toast
            && at.elapsed() > Duration::from_secs(5)
        {
            let _ = t;
            self.toast = None;
        }
        let line = match &self.toast {
            Some((t, _)) => Line::from(Span::styled(format!(" {t}"), fg(Color::Yellow))),
            None => Line::from(Span::styled(
                format!(
                    " {}",
                    match self.tab {
                        Tab::Explore => self.explore.hints(),
                        Tab::Plans => self.plans.hints(),
                        Tab::NewPlan => self.newplan.hints(),
                    }
                ),
                dim(),
            )),
        };
        f.render_widget(Paragraph::new(line), bottom);

        if self.help {
            draw_help(f, self.tab);
        }
        if let Some(d) = &self.dialog {
            draw_dialog(f, d);
        }
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let [v] = Layout::vertical([Constraint::Length(h.min(area.height))]).flex(Flex::Center).areas(area);
    let [r] = Layout::horizontal([Constraint::Length(w.min(area.width))]).flex(Flex::Center).areas(v);
    r
}

fn draw_dialog(f: &mut Frame<'_>, d: &Dialog) {
    let mut lines = d.lines.clone();
    lines.push(Line::raw(""));
    lines.push(match d.confirm {
        Some(_) => Line::from(vec![
            Span::styled(" y ", bold().patch(fg(Color::Green))),
            Span::raw("yes   "),
            Span::styled(" n ", bold()),
            Span::raw("no"),
        ]),
        None => Line::from(Span::styled("any key to close", dim())),
    });
    let w = (lines.iter().map(|l| l.width()).max().unwrap_or(20) as u16 + 4).clamp(40, 100);
    let h = lines.len() as u16 + 2;
    let area = centered(f.area(), w, h);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).title(Span::styled(format!(" {} ", d.title), bold())),
        ),
        area,
    );
}

fn draw_help(f: &mut Frame<'_>, tab: Tab) {
    let mut rows: Vec<(&str, &str)> =
        vec![("1 2 3 / Tab", "switch screen"), ("?", "this help"), ("q / Ctrl-C", "quit (inside a plan: back)"), ("", "")];
    rows.extend(match tab {
        Tab::Explore => vec![
            ("↑↓ j k  PgUp PgDn  g G", "move"),
            ("→ l ⏎ / ← h ⌫", "open folder / go up"),
            ("space", "mark or unmark"),
            ("t / z", "trash / quarantine marked (or selected)"),
            ("c", "clear marks"),
            ("i", "details: sizes, detector, guards"),
            ("E", "ask the policy rules (may call the judge)"),
            ("s / b", "sort order / which bytes to show"),
            ("/", "filter by name"),
            ("p", "plan this folder: detectors + policy rules"),
            ("a", "ask about this folder in plain English"),
            ("o", "reveal in Finder / file manager"),
            ("r", "rescan   (Esc cancels a running scan)"),
        ],
        Tab::Plans => vec![
            ("↑↓ ⏎", "choose and open a plan"),
            ("n", "new plan (from the list)"),
            ("a r p space", "approve / reject / pending / cycle"),
            ("A R", "approve / reject everything shown"),
            ("/ ⏎", "filter / details"),
            ("X", "apply: dry run first, then confirm"),
            ("U", "undo applied entries (confirm)"),
            ("L", "show or hide the log"),
            ("Esc q", "back to the list"),
        ],
        Tab::NewPlan => vec![
            ("← →", "kind: cleanup, ask, duplicates, archive"),
            ("e", "edit the folder"),
            ("⏎", "make the plan (it opens on Plans when ready)"),
            ("Cleanup: d / p", "toggle detectors / policy rules"),
            ("Ask: w", "write the request in plain English"),
            ("Duplicates: m", "minimum file size"),
            ("Archive: g / m / c", "age / size / compress in place"),
            ("o", "open the result on the Plans screen"),
            ("Esc", "stop typing; cancel a scan"),
        ],
    });
    let lines: Vec<Line> =
        rows.iter().map(|(k, v)| Line::from(vec![Span::styled(format!(" {k:<23}"), accent()), Span::raw(v.to_string())])).collect();
    let area = centered(f.area(), 78, lines.len() as u16 + 2);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(Span::styled(format!(" {} — keys ", tab.title()), bold())),
        ),
        area,
    );
}

/// `tdy ui [PATH]` (and bare `tdy` in a terminal).
pub fn run(path: Option<String>) -> Result<()> {
    let root = std::fs::canonicalize(expand_tilde(path.as_deref().unwrap_or(".")))?;
    if !root.is_dir() {
        anyhow::bail!("{} is not a folder", display_path(&root));
    }
    let mut app = App::new(root);
    let mut terminal = ratatui::init();
    let res = (|| -> Result<()> {
        while !app.quit {
            terminal.draw(|f| app.draw(f))?;
            let timeout = if app.busy() { Duration::from_millis(100) } else { Duration::from_millis(500) };
            if event::poll(timeout)?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                app.on_key(k);
            }
            while let Ok(m) = app.rx.try_recv() {
                app.on_msg(m);
            }
        }
        Ok(())
    })();
    ratatui::restore();
    res
}

#[cfg(test)]
mod tests;
