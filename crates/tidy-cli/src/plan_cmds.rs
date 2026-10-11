//! `tidy plan | plans | show | review | apply | undo | explain | purge | detectors | init`.

use crate::ui;
use anyhow::{Context, Result, bail};
use clap::Args;
use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::PathBuf;
use tidy_plan::review::{self, Filter};
use tidy_plan::{Action, ApplyOptions, Event, Status, store};
use tidy_rules::Policy;
use tidy_rules::guards::{Facts, Guards};
use tidy_rules::planner::{self, PlanRequest};
use tidy_scan::{ScanOptions, fmt, path::display_path, path::expand_tilde};

#[derive(Args)]
pub struct PlanCmd {
    /// Directories to plan for.
    #[arg(default_value = "~")]
    pub paths: Vec<String>,
    /// Policy file (default ~/.config/tidy/policy.toml if present).
    #[arg(long)]
    pub policy: Option<PathBuf>,
    /// Skip the deterministic detectors (L1).
    #[arg(long)]
    pub no_detectors: bool,
    /// Skip natural-language policy rules (L2).
    #[arg(long)]
    pub no_rules: bool,
    /// Override the policy's mode.
    #[arg(long)]
    pub autonomous: bool,
    #[arg(short = 'j', long, default_value_t = 0)]
    pub threads: usize,
    /// Show guard-blocked candidates too.
    #[arg(long)]
    pub show_blocked: bool,
    /// Number of entries to print.
    #[arg(short = 'n', long, default_value_t = 15)]
    pub lines: usize,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Default, Clone)]
pub struct FilterArgs {
    /// Rule or detector id (prefix match: `photos.` matches `photos.dogs`).
    #[arg(long)]
    pub rule: Option<String>,
    #[arg(long)]
    pub action: Option<String>,
    /// Minimum model probability.
    #[arg(long)]
    pub min_p: Option<f64>,
    /// Minimum size, e.g. 100MB.
    #[arg(long)]
    pub min_size: Option<String>,
    /// Only entries under this path.
    #[arg(long)]
    pub under: Option<String>,
    /// Entry ids (prefixes allowed). Repeatable.
    #[arg(long = "entry", short = 'e')]
    pub entries: Vec<String>,
    /// Only entries with this status.
    #[arg(long)]
    pub status: Option<String>,
}

impl FilterArgs {
    pub fn to_filter(&self) -> Result<Filter> {
        Ok(Filter {
            rule: self.rule.clone(),
            action: self.action.as_deref().map(|a| a.parse::<Action>()).transpose().map_err(anyhow::Error::msg)?,
            min_p: self.min_p,
            min_size: self.min_size.as_deref().map(|s| fmt::parse_bytes(s).context("--min-size")).transpose()?,
            under: self.under.as_deref().map(expand_tilde),
            ids: self.entries.clone(),
            status: self.status.as_deref().map(parse_status).transpose()?,
        })
    }
}

fn parse_status(s: &str) -> Result<Status> {
    Ok(match s {
        "pending" => Status::Pending,
        "approved" | "approve" => Status::Approved,
        "rejected" | "reject" => Status::Rejected,
        "applied" => Status::Applied,
        "failed" => Status::Failed,
        "skipped" => Status::Skipped,
        "undone" => Status::Undone,
        _ => bail!("unknown status `{s}`"),
    })
}

#[derive(Args)]
pub struct ShowCmd {
    /// Plan id, unique prefix, `latest`, or a path.
    pub plan: Option<String>,
    #[command(flatten)]
    pub filter: FilterArgs,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct ReviewCmd {
    pub plan: Option<String>,
    /// Edit statuses in $EDITOR (git-rebase style).
    #[arg(long)]
    pub edit: bool,
    #[arg(long, conflicts_with_all = ["reject_all", "approve", "reject", "reset"])]
    pub approve_all: bool,
    #[arg(long, conflicts_with_all = ["approve", "reject", "reset"])]
    pub reject_all: bool,
    /// Approve entries matching the filters.
    #[arg(long)]
    pub approve: bool,
    /// Reject entries matching the filters.
    #[arg(long)]
    pub reject: bool,
    /// Set matching entries back to pending.
    #[arg(long)]
    pub reset: bool,
    #[command(flatten)]
    pub filter: FilterArgs,
}

#[derive(Args)]
pub struct ApplyCmd {
    pub plan: Option<String>,
    /// Check everything, change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Only these entries (still must be approved).
    #[arg(long = "entry", short = 'e')]
    pub entries: Vec<String>,
    /// Skip re-fingerprinting (faster, less safe).
    #[arg(long)]
    pub no_verify: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct UndoCmd {
    pub plan: Option<String>,
    #[arg(long = "entry", short = 'e')]
    pub entries: Vec<String>,
}

#[derive(Args)]
pub struct ExplainCmd {
    pub path: String,
    #[arg(long)]
    pub policy: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct PurgeCmd {
    /// Remove quarantine buckets older than this.
    #[arg(long, default_value = "30d")]
    pub older: String,
    /// Actually delete (otherwise only list).
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args)]
pub struct InitCmd {
    /// Overwrite an existing policy file.
    #[arg(long)]
    pub force: bool,
}

pub fn load_policy(path: Option<&std::path::Path>) -> Result<(Policy, Option<String>, Option<PathBuf>)> {
    Policy::load_or_default(path)
}

pub fn cmd_plan(c: PlanCmd) -> Result<()> {
    let (policy, hash, ppath) = load_policy(c.policy.as_deref())?;
    let mut policy = policy;
    if c.autonomous {
        policy.mode = tidy_plan::Mode::Autonomous;
    }
    let roots: Vec<PathBuf> =
        c.paths.iter().map(|p| std::fs::canonicalize(expand_tilde(p)).with_context(|| p.to_string())).collect::<Result<_>>()?;
    let guards = Guards::new(policy.guard_config());
    let header = planner::new_header(roots.clone(), policy.mode, ppath, hash, None);
    let req = PlanRequest { roots, scan: ScanOptions { threads: c.threads, top_k: 0, ..Default::default() }, detectors: !c.no_detectors };
    let started = std::time::Instant::now();
    let mut planned = planner::plan_detectors(&req, &policy, &guards, header)?;
    if !c.no_rules && !policy.rules.is_empty() {
        // A missing key or unreachable judge must not block deterministic cleanup.
        if let Err(e) = crate::judge_cmds::add_policy_entries(&mut planned, &policy, &guards) {
            eprintln!("{}", ui::yellow(&format!("policy rules skipped: {e:#}")));
        }
    }
    let plan = &planned.plan;
    let path = store::save(plan)?;
    let counts = plan.counts();
    if c.json {
        println!(
            "{}",
            serde_json::json!({"plan": plan.header.id, "path": path, "counts": counts, "blocked": planned.blocked.len(), "elapsed_ms": started.elapsed().as_millis() as u64})
        );
        return Ok(());
    }
    let blocked_bytes: u64 = planned.blocked.iter().map(|b| b.bytes).sum();
    println!(
        "{} {}  {}",
        ui::bold("plan"),
        ui::bold(&plan.header.id),
        ui::dim(&format!(
            "{} actions · {} reclaimable · {} blocked by guards ({}) · {:.1}s",
            counts.total,
            fmt::bytes(counts.reclaimable),
            planned.blocked.len(),
            fmt::bytes(blocked_bytes),
            started.elapsed().as_secs_f64()
        ))
    );
    let mut shown: Vec<&tidy_plan::Entry> = plan.entries.iter().collect();
    shown.sort_by_key(|e| std::cmp::Reverse(e.bytes.private));
    for e in shown.iter().take(c.lines) {
        println!("  {}", review::line(e));
    }
    if plan.entries.len() > c.lines {
        println!("  {}", ui::dim(&format!("… {} more (tidy show {})", plan.entries.len() - c.lines, plan.header.id)));
    }
    if c.show_blocked && !planned.blocked.is_empty() {
        println!("{}", ui::bold("blocked"));
        for b in planned.blocked.iter().take(c.lines) {
            println!("  {:>9}  {}  {}", fmt::bytes(b.bytes), display_path(&b.path), ui::dim(&format!("{} · {}", b.by, b.block)));
        }
    }
    if plan.header.mode == tidy_plan::Mode::Autonomous {
        println!("{}", ui::dim(&format!("autonomous: entries are pre-approved → tidy apply {}", plan.header.id)));
    } else if !plan.entries.is_empty() {
        println!("{}", ui::dim(&format!("next: tidy review {0}   (or: tidy review {0} --edit | --approve-all)", plan.header.id)));
    }
    Ok(())
}

pub fn cmd_plans() -> Result<()> {
    let plans = store::list()?;
    if plans.is_empty() {
        println!("no plans yet — run `tidy plan`");
    }
    for p in plans {
        let c = p.counts();
        let roots: Vec<String> = p.header.roots.iter().map(|r| display_path(r)).collect();
        println!(
            "{}  {:>5} ago  {:>4} entries  {:>9}  {}  {}",
            ui::bold(&p.header.id),
            fmt::age(fmt::now_secs() - p.header.created),
            c.total,
            fmt::bytes(c.reclaimable),
            ui::dim(&format!("pending {} · approved {} · applied {} · undone {}", c.pending, c.approved, c.applied, c.undone)),
            roots.join(", ")
        );
    }
    Ok(())
}

pub fn cmd_show(c: ShowCmd) -> Result<()> {
    let (plan, path) = store::load(c.plan.as_deref())?;
    let f = c.filter.to_filter()?;
    if c.json {
        for e in plan.entries.iter().filter(|e| f.matches(e)) {
            println!("{}", serde_json::to_string(e)?);
        }
        return Ok(());
    }
    let cnt = plan.counts();
    println!(
        "{} {}  {}",
        ui::bold("plan"),
        ui::bold(&plan.header.id),
        ui::dim(&format!("{} · {} entries · {} reclaimable", display_path(&path), cnt.total, fmt::bytes(cnt.reclaimable)))
    );
    for e in plan.entries.iter().filter(|e| f.matches(e)) {
        println!("  {}", review::line(e));
        if let Some(err) = e.outcome.as_ref().and_then(|o| o.error.as_ref()) {
            println!("    {}", ui::dim(err));
        }
    }
    Ok(())
}

pub fn cmd_review(c: ReviewCmd) -> Result<()> {
    let (mut plan, _) = store::load(c.plan.as_deref())?;
    let f = c.filter.to_filter()?;
    let bulk = c.approve_all || c.reject_all || c.approve || c.reject || c.reset;
    if bulk {
        if (c.approve || c.reject || c.reset) && f.is_empty() {
            bail!("--approve/--reject/--reset need a filter (e.g. --rule, --entry, --min-p); use --approve-all to approve everything");
        }
        let status = if c.approve_all || c.approve {
            Status::Approved
        } else if c.reject_all || c.reject {
            Status::Rejected
        } else {
            Status::Pending
        };
        let n = review::set_status(&mut plan, &f, status, "human");
        store::save(&plan)?;
        let cnt = plan.counts();
        println!(
            "{} {n} entries → {}  {}",
            ui::bold(&plan.header.id),
            status.as_str(),
            ui::dim(&format!("approved {} · rejected {} · pending {}", cnt.approved, cnt.rejected, cnt.pending))
        );
        return Ok(());
    }
    if c.edit {
        return edit_in_editor(&mut plan);
    }
    if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
        return crate::tui::review(&mut plan, &f);
    }
    // Non-interactive: print the editor view.
    print!("{}", review::render_editor(&plan));
    Ok(())
}

fn edit_in_editor(plan: &mut tidy_plan::Plan) -> Result<()> {
    let dir = tidy_ops::dirs::data_dir().join("tmp");
    tidy_ops::fsops::create_private_dir(&dir)?;
    let file = dir.join(format!("review-{}.txt", plan.header.id));
    std::fs::write(&file, review::render_editor(plan))?;
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(&file).status()?;
    if !status.success() {
        bail!("editor exited with {status}; plan unchanged");
    }
    let text = std::fs::read_to_string(&file)?;
    let _ = std::fs::remove_file(&file);
    let (a, r, p) = review::parse_editor(plan, &text)?;
    store::save(plan)?;
    println!("{}: approved {a} · rejected {r} · pending {p}", ui::bold(&plan.header.id));
    Ok(())
}

/// Apply-time guard: re-run L0 guards with fresh facts.
pub fn apply_guard(policy: &Policy) -> impl Fn(&tidy_plan::Entry) -> Result<(), String> {
    apply_guard_with(policy.guard_config())
}

/// Apply-time guard: re-runs every L0 guard with fresh facts — the
/// fingerprint's newest mtime (verified unchanged since planning), current
/// file flags (cloud placeholder, locked) — and checks move destinations.
pub fn apply_guard_with(cfg: tidy_rules::GuardConfig) -> impl Fn(&tidy_plan::Entry) -> Result<(), String> {
    let guards = Guards::new(cfg);
    move |e: &tidy_plan::Entry| {
        let facts = Facts {
            mtime_max: Some(e.fingerprint.mtime),
            flags: current_flags(&e.path),
            regenerable: e.source.layer == tidy_plan::Layer::L1,
            min_age: None,
        };
        guards.check(&e.path, e.action, &facts, None).map_err(|b| b.to_string())?;
        if let Some(d) = &e.dest
            && matches!(e.action, Action::Move | Action::Copy | Action::Rename | Action::Archive)
        {
            guards.check_destination(d).map_err(|b| b.to_string())?;
        }
        Ok(())
    }
}

/// Entry flags (dataless placeholder, immutable) for a path right now.
fn current_flags(p: &std::path::Path) -> u32 {
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        use tidy_scan::entry::flags;
        let Ok(md) = std::fs::symlink_metadata(p) else { return 0 };
        let f = md.st_flags();
        let mut out = 0;
        if f & 0x4000_0000 != 0 {
            out |= flags::DATALESS;
        }
        if f & (0x0000_0002 | 0x0002_0000) != 0 {
            out |= flags::IMMUTABLE;
        }
        return out;
    }
    #[allow(unreachable_code)]
    {
        let _ = p;
        0
    }
}

pub fn cmd_apply(c: ApplyCmd) -> Result<()> {
    let (mut plan, _) = store::load(c.plan.as_deref())?;
    let policy = match &plan.header.policy {
        Some(p) if p.exists() => Policy::load(p)?.0,
        _ => Policy::default(),
    };
    let approved = plan.entries.iter().filter(|e| e.status == Status::Approved).count();
    if approved == 0 {
        println!("nothing approved in {} — run `tidy review {}`", plan.header.id, plan.header.id);
        return Ok(());
    }
    let only: Option<HashSet<String>> = if c.entries.is_empty() {
        None
    } else {
        Some(plan.entries.iter().filter(|e| c.entries.iter().any(|x| e.id.starts_with(x.as_str()))).map(|e| e.id.clone()).collect())
    };
    let opts = ApplyOptions { dry_run: c.dry_run, only, verify: !c.no_verify };
    let guard = apply_guard(&policy);
    let json = c.json;
    let report = tidy_plan::apply(&mut plan, &opts, &guard, &mut |ev| print_event(&ev, json))?;
    if json {
        println!("{}", serde_json::json!({"plan": plan.header.id, "report": report}));
        return Ok(());
    }
    let mut parts = Vec::new();
    if c.dry_run {
        parts.push(format!("{} would apply", report.would));
    } else {
        parts.push(format!("{} applied", report.applied));
    }
    if report.skipped > 0 {
        parts.push(format!("{} skipped", report.skipped));
    }
    if report.failed > 0 {
        parts.push(format!("{} failed", report.failed));
    }
    if report.freed > 0 {
        parts.push(format!("{} freed", fmt::bytes(report.freed)));
    }
    if report.parked > 0 {
        parts.push(format!("{} in trash/quarantine", fmt::bytes(report.parked)));
    }
    println!("{} {}", ui::bold(&plan.header.id), parts.join(" · "));
    if !c.dry_run && report.applied > 0 {
        println!("{}", ui::dim(&format!("undo with: tidy undo {}", plan.header.id)));
    }
    Ok(())
}

fn print_event(ev: &Event<'_>, json: bool) {
    if json {
        let (kind, e, msg) = match ev {
            Event::Ok(e, m) => ("ok", Some(*e), m.clone()),
            Event::Skip(e, m) => ("skip", Some(*e), m.clone()),
            Event::Fail(e, m) => ("fail", Some(*e), m.clone()),
            Event::WouldDo(e, m) => ("would", Some(*e), m.clone()),
            Event::Note(m) => ("note", None, m.clone()),
        };
        println!("{}", serde_json::json!({"event": kind, "entry": e.map(|e| &e.id), "path": e.map(|e| &e.path), "message": msg}));
        return;
    }
    match ev {
        Event::Ok(e, m) => {
            println!("  {} {:<5} {}", ui::green("✓"), e.id, m.replace(&tidy_scan::path::home_dir().display().to_string(), "~"))
        }
        Event::Skip(e, m) => println!("  {} {:<5} {}  {}", ui::yellow("-"), e.id, display_path(&e.path), ui::dim(m)),
        Event::Fail(e, m) => println!("  {} {:<5} {}  {}", ui::red("✗"), e.id, display_path(&e.path), m),
        Event::WouldDo(e, m) => {
            println!("  {} {:<5} {}", ui::dim("·"), e.id, m.replace(&tidy_scan::path::home_dir().display().to_string(), "~"))
        }
        Event::Note(m) => println!("  {}", ui::dim(m)),
    }
}

pub fn cmd_undo(c: UndoCmd) -> Result<()> {
    let (mut plan, _) = store::load(c.plan.as_deref())?;
    let only: Option<HashSet<String>> = if c.entries.is_empty() {
        None
    } else {
        Some(plan.entries.iter().filter(|e| c.entries.iter().any(|x| e.id.starts_with(x.as_str()))).map(|e| e.id.clone()).collect())
    };
    let r = tidy_plan::undo(&mut plan, only.as_ref(), &mut |ev| print_event(&ev, false))?;
    println!("{} {} restored · {} not undoable · {} failed", ui::bold(&plan.header.id), r.undone, r.skipped, r.failed);
    Ok(())
}

pub fn cmd_explain(c: ExplainCmd) -> Result<()> {
    let p = std::fs::canonicalize(expand_tilde(&c.path)).with_context(|| c.path.clone())?;
    let (policy, _, _) = load_policy(c.policy.as_deref())?;
    let guards = Guards::new(policy.guard_config());
    let md = std::fs::symlink_metadata(&p)?;
    let (files, logical, mtime) = if md.is_dir() {
        tidy_ops::fingerprint::tree_summary(&p)?
    } else {
        use std::os::unix::fs::MetadataExt;
        (1, md.len(), md.mtime())
    };
    let det = if md.is_dir() { tidy_rules::detect::classify(&p) } else { None };
    let regenerable = det.is_some();
    let min_age = det.and_then(|d| d.3);
    let facts = Facts { mtime_max: (mtime != 0).then_some(mtime), flags: 0, regenerable, min_age };
    let actions = [Action::Delete, Action::Trash, Action::Move, Action::Tag];
    let results: Vec<(Action, Result<(), tidy_rules::Block>)> = actions.iter().map(|&a| (a, guards.check(&p, a, &facts, None))).collect();
    if c.json {
        let v = serde_json::json!({
            "path": p, "files": files, "logical": logical, "newest_mtime": mtime,
            "detector": det.map(|d| serde_json::json!({"id": d.0, "what": d.1, "hint": d.2})),
            "guards": results.iter().map(|(a, r)| serde_json::json!({"action": a.as_str(), "allowed": r.is_ok(), "block": r.as_ref().err().map(|b| b.to_string())})).collect::<Vec<_>>(),
        });
        println!("{v}");
        return Ok(());
    }
    println!(
        "{}  {}",
        ui::bold(&display_path(&p)),
        ui::dim(&format!(
            "{} · {} files · newest change {} ago",
            fmt::bytes(logical),
            ui::thousands(files),
            fmt::age(fmt::now_secs() - mtime)
        ))
    );
    match det {
        Some((id, what, hint, _)) => println!("  L1 detector  {} — {} ({})", ui::bold(id), what, hint),
        None => println!("  L1 detector  {}", ui::dim("none")),
    }
    for (a, r) in &results {
        match r {
            Ok(()) => println!("  L0 {:<9} {}", a.as_str(), ui::green("allowed")),
            Err(b) => println!("  L0 {:<9} {} {}", a.as_str(), ui::red("blocked"), ui::dim(&b.to_string())),
        }
    }
    crate::judge_cmds::explain_rules(&p, &policy)?;
    Ok(())
}

pub fn cmd_purge(c: PurgeCmd) -> Result<()> {
    let older = fmt::parse_duration(&c.older).context("--older")?;
    let cutoff = fmt::now_secs() - older;
    let mut total = 0u64;
    let mut doomed = Vec::new();
    for (p, mtime) in tidy_ops::trash::quarantine_buckets()? {
        if mtime < cutoff {
            let (_, logical, _) = tidy_ops::fingerprint::tree_summary(&p).unwrap_or((0, 0, 0));
            total += logical;
            println!(
                "  {:>9}  {}  {}",
                fmt::bytes(logical),
                display_path(&p),
                ui::dim(&format!("{} old", fmt::age(fmt::now_secs() - mtime)))
            );
            doomed.push(p);
        }
    }
    if doomed.is_empty() {
        println!("nothing in quarantine older than {}", c.older);
        return Ok(());
    }
    if !c.yes {
        println!("{} in {} bucket(s) would be permanently deleted — rerun with --yes", fmt::bytes(total), doomed.len());
        return Ok(());
    }
    for p in &doomed {
        tidy_ops::fsops::remove_tree(p)?;
    }
    println!("purged {} from quarantine", fmt::bytes(total));
    Ok(())
}

pub fn cmd_detectors() -> Result<()> {
    for id in tidy_rules::detect::detector_ids() {
        println!("{id}");
    }
    Ok(())
}

pub fn cmd_init(c: InitCmd) -> Result<()> {
    let p = tidy_rules::policy::default_path();
    if p.exists() && !c.force {
        bail!("{} already exists (use --force to overwrite)", p.display());
    }
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&p, tidy_rules::policy::EXAMPLE)?;
    println!("wrote {}", display_path(&p));
    Ok(())
}
