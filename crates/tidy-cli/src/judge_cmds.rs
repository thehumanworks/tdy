//! Policy rules (L2) in the CLI: judges, plan entries, explain, calibrate,
//! policy check/test, bench.

use crate::ui;
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use std::collections::HashSet;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use tidy_judge::{Answer, BackendConfig, Cached, Cascade, Judge, Question, Request};
use tidy_rules::Policy;
use tidy_rules::calib::{self, Calibration, Label};
use tidy_rules::guards::Guards;
use tidy_rules::items::{self, Item};
use tidy_rules::l2::{self, CompiledRule, OutcomeKind};
use tidy_rules::planner::Planned;
use tidy_scan::{fmt, path::display_path, path::expand_tilde};

/// USD per 1M input tokens, where the vendor publishes it.
fn price_per_mtok(model: &str) -> Option<f64> {
    let m = model.to_lowercase();
    if m.contains("gpt-6-luna") {
        Some(0.10)
    } else if m.contains("jev") {
        Some(0.042)
    } else if m.contains("clef-flash") {
        None
    } else if m.contains("clef") {
        Some(0.24)
    } else {
        None
    }
}

fn backend_config(name: &str, policy: &Policy) -> Result<BackendConfig> {
    let b = policy.judge.backends.get(name).with_context(|| format!("judge `{name}` is not defined"))?;
    Ok(BackendConfig {
        name: name.to_string(),
        kind: b.kind.clone(),
        model: b.model.clone(),
        endpoint: b.endpoint.clone(),
        api_key_env: b.api_key_env.clone(),
        images: b.images,
        max_questions: b.max_questions,
        timeout_secs: b.timeout_secs,
        concurrency: policy.judge.concurrency.unwrap_or(32),
    })
}

pub struct BuiltJudge {
    pub judge: Box<dyn Judge>,
    pub id: String,
    pub models: Vec<String>,
}

/// The policy's judge: a cascade when configured, else the default backend.
/// Every stage is wrapped in the persistent decision cache.
pub fn build_judge(policy: &Policy, no_cache: bool) -> Result<Option<BuiltJudge>> {
    let names: Vec<String> = if !policy.judge.cascade.is_empty() {
        policy.judge.cascade.clone()
    } else if let Some(d) = &policy.judge.default {
        vec![d.clone()]
    } else if policy.judge.backends.len() == 1 {
        policy.judge.backends.keys().cloned().collect()
    } else {
        return Ok(None);
    };
    let cache_dir = (!no_cache).then(tidy_ops::dirs::decisions_cache_dir);
    let mut stages: Vec<Box<dyn Judge>> = Vec::new();
    let mut models = Vec::new();
    for n in &names {
        let cfg = backend_config(n, policy)?;
        models.push(cfg.model.clone());
        let j = tidy_judge::build(&cfg)?;
        stages.push(Box::new(Cached::open(j, cache_dir.clone())));
    }
    let judge: Box<dyn Judge> = if stages.len() == 1 {
        stages.pop().unwrap()
    } else {
        let band = policy.judge.uncertain.unwrap_or([0.2, 0.8]);
        Box::new(Cascade { stages, band: (band[0], band[1]) })
    };
    let id = judge.id();
    Ok(Some(BuiltJudge { judge, id, models }))
}

fn progress_bar(label: &'static str) -> impl Fn(usize, usize) + Sync {
    let tty = std::io::stderr().is_terminal();
    move |done, total| {
        if tty && (done == total || done % 8 == 0) {
            eprint!("\r\x1b[2K{label} {done}/{total}");
            if done == total {
                eprint!("\r\x1b[2K");
            }
        }
    }
}

/// What evaluating the policy rules for a plan did.
#[derive(Debug, Default, Clone)]
pub struct L2Summary {
    pub judge: Option<String>,
    pub items: usize,
    pub requests: usize,
    pub cache_hits: usize,
    pub input_tokens: u64,
    /// Estimated input cost in USD, when the model's price is known.
    pub cost: Option<f64>,
    pub elapsed_ms: u64,
    pub added: usize,
    pub warnings: Vec<String>,
}

impl L2Summary {
    /// One line for humans, e.g. `judge luna:… · 13 items · 9 requests (2 cached) · …`.
    pub fn judge_line(&self) -> Option<String> {
        let j = self.judge.as_ref()?;
        Some(format!(
            "judge {j} · {} items · {} requests ({} cached) · {} input tokens{} · {:.1}s",
            self.items,
            self.requests,
            self.cache_hits,
            ui::thousands(self.input_tokens),
            self.cost.map(|c| format!(" · ≈${c:.4}")).unwrap_or_default(),
            self.elapsed_ms as f64 / 1000.0
        ))
    }
}

/// Evaluate the policy rules (L2) and add their entries to the plan.
/// `progress(done, total)` follows the judge calls. Nothing is printed.
pub fn policy_entries(
    planned: &mut Planned,
    policy: &Policy,
    guards: &Guards,
    progress: &(dyn Fn(usize, usize) + Sync),
) -> Result<L2Summary> {
    let built = build_judge(policy, false)?;
    let mut sum = L2Summary { judge: built.as_ref().map(|b| b.id.clone()), ..Default::default() };
    for r in &policy.rules {
        if let Some(j) = &r.judge
            && Some(j) != policy.judge.default.as_ref()
        {
            sum.warnings.push(format!("rule `{}`: per-rule judges are not supported yet; using the default judge", r.id));
        }
    }
    let calib = Calibration::load();
    let rules = l2::compile(policy, &planned.plan.header.roots, built.as_ref().map(|b| b.id.as_str()), &calib);
    let conc = policy.judge.concurrency.unwrap_or(32);
    let run = l2::run(policy, &rules, built.as_ref().map(|b| b.judge.as_ref()), guards, conc, progress)?;
    let mut taken: HashSet<String> = planned.plan.entries.iter().map(|e| e.id.clone()).collect();
    let mut es = l2::entries(&run, policy.mode, built.as_ref().map(|b| b.id.as_str()), &mut taken);
    // L1 artifacts win: drop L2 entries at or inside an L1 path.
    let l1: Vec<PathBuf> = planned.plan.entries.iter().map(|e| e.path.clone()).collect();
    es.retain(|e| !l1.iter().any(|p| e.path.starts_with(p)));
    for o in &run.outcomes {
        if let OutcomeKind::Blocked { rule, why } = &o.kind {
            planned.blocked.push(tidy_rules::planner::Blocked {
                path: o.item.path.clone(),
                by: rule.clone(),
                block: tidy_rules::Block { guard: "policy", reason: why.clone() },
                bytes: o.item.alloc,
            });
        }
    }
    let errors: Vec<&str> =
        run.outcomes.iter().filter_map(|o| if let OutcomeKind::Error(e) = &o.kind { Some(e.as_str()) } else { None }).collect();
    if !errors.is_empty() {
        sum.warnings.push(format!("{} item(s) could not be judged: {}", errors.len(), errors[0]));
    }
    if run.unjudged > 0 {
        sum.warnings.push(unjudged_warning(run.unjudged, policy));
    }
    if let Some(b) = &built {
        sum.cost = b.models.first().and_then(|m| price_per_mtok(m)).map(|p| run.input_tokens as f64 / 1e6 * p);
        planned.plan.header.judge = Some(b.id.clone());
    }
    sum.items = run.outcomes.len();
    sum.requests = run.requests;
    sum.cache_hits = run.cache_hits;
    sum.input_tokens = run.input_tokens;
    sum.elapsed_ms = run.elapsed_ms;
    sum.added = es.len();
    planned.plan.entries.extend(es);
    Ok(sum)
}

/// `policy_entries` for the CLI: a progress line on stderr, then the summary.
pub fn add_policy_entries(planned: &mut Planned, policy: &Policy, guards: &Guards) -> Result<()> {
    let sum = policy_entries(planned, policy, guards, &progress_bar("judging"))?;
    if let Some(line) = sum.judge_line() {
        eprintln!("{}", ui::dim(&line));
    }
    for w in &sum.warnings {
        eprintln!("{}", ui::yellow(w));
    }
    Ok(())
}

/// Explain what happens to rules that need a judge when none is configured.
pub fn unjudged_warning(n: usize, policy: &Policy) -> String {
    let keeps: Vec<&str> = policy.rules.iter().filter(|r| r.action == "keep" && !r.when.is_empty()).map(|r| r.id.as_str()).collect();
    let mut s = format!("{n} item(s) need a decision model but no [judge] is configured: model-decided rules were skipped");
    if !keeps.is_empty() {
        s.push_str(&format!(" and {} kept them to be safe", keeps.join(", ")));
    }
    s
}

pub fn evidence_summary(v: &serde_json::Value) -> String {
    let mut parts = Vec::new();
    if let Some(rules) = v.get("rules").and_then(|r| r.as_object()) {
        let mut rs: Vec<(&String, f64)> = rules.iter().filter_map(|(k, v)| Some((k, v.as_f64()?))).collect();
        rs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        parts.push(rs.iter().take(4).map(|(k, p)| format!("{k} {p:.2}")).collect::<Vec<_>>().join(", "));
    }
    if let Some(a) = v.get("answers") {
        if let Some(p) = a.pointer("/safety.regenerable/p").and_then(|x| x.as_f64()) {
            parts.push(format!("regenerable {p:.2}"));
        }
        if let Some(s) = a.pointer("/safety.loss/score").and_then(|x| x.as_f64()) {
            parts.push(format!("loss {s:.1}/3"));
        }
    }
    if v.get("cached").and_then(|c| c.as_bool()) == Some(true) {
        parts.push("cached".into());
    }
    parts.join(" · ")
}

/// How the policy rules see one path (for `tidy explain` and the TUI).
#[derive(Debug, Clone)]
pub struct RulesExplanation {
    /// (rule id, probability if answered, threshold, action)
    pub rows: Vec<(String, Option<f64>, f64, String)>,
    pub verdict: String,
    /// e.g. `judge luna:… · 540 ms · cached`
    pub judge: Option<String>,
}

/// Evaluate the policy rules for one path; may call the judge (cached).
/// `None` when no rule applies to it.
pub fn rules_for_path(p: &Path, policy: &Policy) -> Result<Option<RulesExplanation>> {
    if policy.rules.is_empty() {
        return Ok(None);
    }
    let calib = Calibration::load();
    let built = build_judge(policy, false).ok().flatten();
    let rules = l2::compile(policy, &[p.parent().unwrap_or(p).to_path_buf()], built.as_ref().map(|b| b.id.as_str()), &calib);
    let now = fmt::now_secs();
    let it = item_for(p)?;
    let app: Vec<&CompiledRule> = l2::prune(rules.iter().filter(|r| r.applies(&it, now)).collect());
    if app.is_empty() {
        return Ok(None);
    }
    let media = built.as_ref().map(|b| l2::Media::load(policy, b.judge.caps()));
    let req = l2::build_request(policy, &it, &app, now, true, media.as_ref());
    let resp = match &built {
        Some(b) if !req.questions.is_empty() => Some(b.judge.decide(&req)?),
        _ => None,
    };
    let guards = Guards::new(policy.guard_config());
    let (kind, ps) = l2::decide(&it, &app, resp.as_ref(), &guards, now);
    let rows = app
        .iter()
        .map(|r| (r.rule.id.clone(), ps.iter().find(|x| x.0 == r.rule.id).map(|x| x.1), r.threshold, r.rule.action.clone()))
        .collect();
    let verdict = match kind {
        OutcomeKind::Kept { rule, p } => {
            format!("kept by {rule} ({})", p.map(|p| format!("{p:.2}")).unwrap_or("unanswered — kept to be safe".into()))
        }
        OutcomeKind::Propose { entries } => entries
            .iter()
            .map(|e| {
                format!(
                    "{} via {} ({:.2}){}{}",
                    e.action.as_str(),
                    e.rule,
                    e.p,
                    e.dest.as_ref().map(|d| format!(" → {}", display_path(d))).unwrap_or_default(),
                    if e.uncertain { " · uncertain" } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join("; "),
        OutcomeKind::Blocked { rule, why } => format!("{rule} blocked: {why}"),
        OutcomeKind::NoRule => "no rule fires".into(),
        OutcomeKind::Error(e) => format!("error: {e}"),
    };
    let judge = resp.map(|r| format!("judge {} · {} ms{}", r.judge, r.latency_ms, if r.cached { " · cached" } else { "" }));
    Ok(Some(RulesExplanation { rows, verdict, judge }))
}

/// Explain policy rules for one path (used by `tidy explain`).
pub fn explain_rules(p: &Path, policy: &Policy) -> Result<()> {
    if policy.rules.is_empty() {
        return Ok(());
    }
    let Some(x) = rules_for_path(p, policy)? else {
        println!("  L2 rules     {}", ui::dim("none apply (scope/filter)"));
        return Ok(());
    };
    for (id, p, tau, action) in &x.rows {
        println!("  L2 {:<18} p={}  τ={:.2}  {}", id, p.map(|p| format!("{p:.2}")).unwrap_or("–".into()), tau, ui::dim(action));
    }
    println!("  L2 verdict   {}", ui::bold(&x.verdict));
    if let Some(j) = &x.judge {
        println!("  {}", ui::dim(j));
    }
    Ok(())
}

fn item_for(p: &Path) -> Result<Item> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::symlink_metadata(p)?;
    let mut it = Item {
        path: p.to_path_buf(),
        is_dir: md.is_dir(),
        logical: md.len(),
        alloc: md.blocks() * 512,
        mtime: md.mtime(),
        flags: 0,
        depth: 1,
        dir: None,
    };
    if it.is_dir {
        items::summarize_dir(&mut it);
    }
    Ok(it)
}

// ---- tidy judge ----------------------------------------------------------------

#[derive(Subcommand)]
pub enum JudgeCmd {
    /// Send a tiny request to each configured backend; show latency and tokens.
    Ping {
        #[arg(long)]
        policy: Option<PathBuf>,
        /// Only this backend.
        #[arg(long)]
        backend: Option<String>,
    },
    /// Ask ad-hoc yes/no questions about a text.
    Ask {
        text: String,
        /// Question (repeatable).
        #[arg(short, long = "question", required = true)]
        questions: Vec<String>,
        #[arg(long)]
        policy: Option<PathBuf>,
        #[arg(long)]
        backend: Option<String>,
    },
}

pub fn cmd_judge(c: JudgeCmd) -> Result<()> {
    match c {
        JudgeCmd::Ping { policy, backend } => {
            let (pol, _, _) = crate::plan_cmds::load_policy(policy.as_deref())?;
            let names: Vec<String> = match backend {
                Some(b) => vec![b],
                None => pol.judge.backends.keys().cloned().collect(),
            };
            if names.is_empty() {
                bail!("no judge backends in the policy (see `tidy init`)");
            }
            let req = Request {
                text: "ITEM\nname: Figma-124.3.dmg\nfolder: ~/Downloads\nkind: file (.dmg, disk image)\nsize: 412 MB\nlast modified: 5mo ago\ndownloaded from: https://www.figma.com/download/desktop".into(),
                images: vec![],
                questions: vec![
                    Question::predicate("installer", "Is this true of ITEM? An installer or disk image of an application that can be downloaded again."),
                    Question::predicate("finance", "Is this true of ITEM? A tax return, invoice or other finance document."),
                ],
            };
            for n in names {
                let cfg = backend_config(&n, &pol)?;
                let j = match tidy_judge::build(&cfg) {
                    Ok(j) => j,
                    Err(e) => {
                        println!("{} {n}: {e}", ui::red("✗"));
                        continue;
                    }
                };
                match j.decide(&req) {
                    Ok(r) => println!(
                        "{} {:<12} {:>5} ms  {:>4} tokens  installer={:.2} finance={:.2}  {}",
                        ui::green("✓"),
                        n,
                        r.latency_ms,
                        r.input_tokens,
                        r.answers.get("installer").and_then(Answer::p).unwrap_or(-1.0),
                        r.answers.get("finance").and_then(Answer::p).unwrap_or(-1.0),
                        ui::dim(&j.id())
                    ),
                    Err(e) => println!("{} {n}: {e}", ui::red("✗")),
                }
            }
            Ok(())
        }
        JudgeCmd::Ask { text, questions, policy, backend } => {
            let (pol, _, _) = crate::plan_cmds::load_policy(policy.as_deref())?;
            let name = backend
                .or(pol.judge.default.clone())
                .or_else(|| pol.judge.backends.keys().next().cloned())
                .context("no judge configured")?;
            let j = tidy_judge::build(&backend_config(&name, &pol)?)?;
            let req = Request {
                text,
                images: vec![],
                questions: questions.iter().enumerate().map(|(i, q)| Question::predicate(format!("q{i}"), q.clone())).collect(),
            };
            let r = j.decide(&req)?;
            for (i, q) in questions.iter().enumerate() {
                let p = r.answers.get(&format!("q{i}")).and_then(Answer::p);
                println!("{:>5}  {q}", p.map(|p| format!("{p:.2}")).unwrap_or("–".into()));
            }
            println!("{}", ui::dim(&format!("{} · {} ms · {} tokens", r.judge, r.latency_ms, r.input_tokens)));
            Ok(())
        }
    }
}

// ---- tidy policy ---------------------------------------------------------------

#[derive(Subcommand)]
pub enum PolicyCmd {
    /// Validate the policy and show how each rule is configured.
    Check {
        #[arg(long)]
        policy: Option<PathBuf>,
    },
    /// Run the policy's test cases (`<policy>.tests.toml`) through the judge.
    Test {
        #[arg(long)]
        policy: Option<PathBuf>,
        /// Test file (default: next to the policy).
        #[arg(long)]
        tests: Option<PathBuf>,
        /// Bypass the decision cache.
        #[arg(long)]
        no_cache: bool,
    },
}

#[derive(serde::Deserialize)]
struct TestFile {
    #[serde(rename = "case", default)]
    cases: Vec<TestCase>,
}

#[derive(serde::Deserialize)]
struct TestCase {
    #[serde(default)]
    name: Option<String>,
    /// A real file or folder.
    #[serde(default)]
    path: Option<String>,
    /// Or a synthetic item.
    #[serde(default)]
    item: Option<SynthItem>,
    /// Rule id that must win, "none", or "keep" (any keep rule); a list
    /// accepts any of them.
    #[serde(deserialize_with = "tidy_rules::policy::one_or_many")]
    expect: Vec<String>,
}

#[derive(serde::Deserialize)]
struct SynthItem {
    name: String,
    folder: String,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    age: Option<String>,
    #[serde(default)]
    where_from: Option<String>,
    #[serde(default)]
    dir: bool,
    #[serde(default)]
    files: Option<u64>,
}

pub fn cmd_policy(c: PolicyCmd) -> Result<()> {
    match c {
        PolicyCmd::Check { policy } => {
            let (pol, hash, path) = crate::plan_cmds::load_policy(policy.as_deref())?;
            let Some(path) = path else {
                println!("no policy file at {} — run `tidy init`", display_path(&tidy_rules::policy::default_path()));
                return Ok(());
            };
            println!(
                "{} {}  {}",
                ui::bold("policy"),
                display_path(&path),
                ui::dim(&format!("{} · mode {:?} · {} rules", hash.unwrap_or_default(), pol.mode, pol.rules.len()))
            );
            if !pol.presets.is_empty() {
                println!("  {}", ui::dim(&format!("presets: {}", pol.presets.join(", "))));
            }
            let calib = Calibration::load();
            let built = build_judge(&pol, true);
            let jid = built.as_ref().ok().and_then(|b| b.as_ref()).map(|b| b.id.clone());
            let rules = l2::compile(&pol, &[cwd()], jid.as_deref(), &calib);
            for r in &rules {
                let tau = if r.deterministic() && !r.is_keep() { "τ –".to_string() } else { format!("τ={:.2}", r.threshold) };
                let calibrated = jid.as_deref().and_then(|j| calib.get(j, &r.rule.id)).is_some();
                let src = if r.deterministic() && !r.is_keep() {
                    "no model"
                } else if r.rule.threshold.is_some() && r.rule.preset.is_none() {
                    "explicit"
                } else if calibrated {
                    "calibrated"
                } else if r.rule.threshold.is_some() {
                    "preset"
                } else {
                    "default"
                };
                let scopes: Vec<String> = r.scopes.iter().map(|s| display_path(s)).collect();
                println!(
                    "  {:<20} {:<10} {tau} ({src})  {}  {}",
                    r.rule.id,
                    r.rule.action,
                    if scopes.is_empty() { ui::yellow("scope missing!") } else { scopes.join(", ") },
                    ui::dim(&format!(
                        "{} criteria{}{}{}",
                        r.rule.when.any.len() + r.rule.when.all.len() + r.rule.when.none.len(),
                        if r.deterministic() { " · deterministic" } else { "" },
                        r.rule.check.as_deref().map(|c| format!(" · check {c}")).unwrap_or_default(),
                        r.rule.preset.as_deref().map(|p| format!(" · preset {p}")).unwrap_or_default()
                    ))
                );
            }
            let judged = rules.iter().any(|r| !r.deterministic());
            match built {
                Ok(Some(b)) => println!("{}", ui::dim(&format!("judge {}", b.id))),
                Ok(None) if judged => println!(
                    "{}",
                    ui::yellow("no [judge] configured: model-decided rules will be skipped, and keep rules protect everything in scope")
                ),
                Ok(None) => {}
                Err(e) => println!("{}", ui::yellow(&format!("judge unavailable: {e:#}"))),
            }
            Ok(())
        }
        PolicyCmd::Test { policy, tests, no_cache } => {
            let (pol, _, ppath) = crate::plan_cmds::load_policy(policy.as_deref())?;
            let tpath = match (tests, ppath) {
                (Some(t), _) => Some(t),
                (None, Some(p)) => Some(p.with_extension("tests.toml")).filter(|t| t.exists()),
                (None, None) => bail!("no policy file; pass --policy"),
            };
            let mut cases: Vec<TestCase> = match &tpath {
                Some(t) => {
                    toml::from_str::<TestFile>(&std::fs::read_to_string(t).with_context(|| format!("reading {}", t.display()))?)?.cases
                }
                None => vec![],
            };
            // Enabled presets bring their own cases, unless a user rule replaced theirs.
            for name in &pol.presets {
                let Some(p) = tidy_rules::presets::get(name) else { continue };
                let intact = p.rules()?.iter().all(|r| pol.rules.iter().any(|u| u.id == r.id && u.preset.as_deref() == Some(name)));
                if intact && !p.tests.is_empty() {
                    cases.extend(toml::from_str::<TestFile>(p.tests).with_context(|| format!("preset {name} tests"))?.cases);
                }
            }
            if cases.is_empty() {
                bail!("no test cases: add [[case]] entries to {}", tpath.map(|t| display_path(&t)).unwrap_or("<policy>.tests.toml".into()));
            }
            let built = build_judge(&pol, no_cache)?;
            if built.is_none() && pol.rules.iter().any(|r| !r.when.is_empty()) {
                eprintln!("{}", ui::yellow("no [judge] configured: model-decided rules stay silent and keep rules protect everything"));
            }
            let calib = Calibration::load();
            let rules = l2::compile(&pol, &[cwd()], built.as_ref().map(|b| b.id.as_str()), &calib);
            let media = built.as_ref().map(|b| l2::Media::load(&pol, b.judge.caps()));
            // Guards are not under test here: allow everything.
            let guards = Guards::new(tidy_rules::GuardConfig { min_age: 0, settle: 0, ..Default::default() });
            let now = fmt::now_secs();
            let mut failed = 0;
            for (i, case) in cases.iter().enumerate() {
                let label = case.name.clone().unwrap_or_else(|| format!("case {}", i + 1));
                let (it, text) = match (&case.path, &case.item) {
                    (Some(p), _) => {
                        let it = item_for(&std::fs::canonicalize(expand_tilde(p))?)?;
                        let text = items::evidence(&it, now, true);
                        (it, text)
                    }
                    (None, Some(s)) => synth(s, now),
                    (None, None) => bail!("{label}: needs `path` or `item`"),
                };
                // Unpruned: a case may ask whether a keep rule protects an item
                // even when no action rule would touch it.
                let app: Vec<&CompiledRule> = rules.iter().filter(|r| applies_lenient(r, &it, now)).collect();
                let req = l2::build_request_text(&pol, text, &it, &app, media.as_ref());
                let resp = match &built {
                    Some(b) if !req.questions.is_empty() => Some(tidy_judge::decide_split(b.judge.as_ref(), &req)?),
                    _ => None,
                };
                let (kind, ps) = l2::decide(&it, &app, resp.as_ref(), &guards, now);
                let got = match &kind {
                    OutcomeKind::Kept { rule, .. } => rule.clone(),
                    OutcomeKind::Propose { entries } => {
                        entries.iter().find(|e| e.action.exclusive()).or(entries.first()).map(|e| e.rule.clone()).unwrap_or_default()
                    }
                    _ => "none".into(),
                };
                let ok = case.expect.iter().any(|e| *e == got || (e == "keep" && matches!(kind, OutcomeKind::Kept { .. })));
                let probs = ps.iter().map(|(r, p)| format!("{r} {p:.2}")).collect::<Vec<_>>().join(", ");
                if ok {
                    println!("{} {label}  {}", ui::green("✓"), ui::dim(&format!("→ {got}  [{probs}]")));
                } else {
                    failed += 1;
                    println!(
                        "{} {label}  expected {}, got {}  {}",
                        ui::red("✗"),
                        case.expect.join(" or "),
                        got,
                        ui::dim(&format!("[{probs}]"))
                    );
                }
            }
            println!("{} passed, {} failed", cases.len() - failed, failed);
            if failed > 0 {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

/// Scope match without requiring the scope to exist on this machine.
fn applies_lenient(r: &CompiledRule, it: &Item, now: i64) -> bool {
    let scopes: Vec<PathBuf> =
        if r.rule.scope.is_empty() { vec![PathBuf::from("/")] } else { r.rule.scope.iter().map(|s| expand_tilde(s)).collect() };
    let mut r2 = r.clone();
    r2.scopes = scopes.into_iter().chain(r.scopes.iter().cloned()).collect();
    r2.applies(it, now)
}

fn synth(s: &SynthItem, now: i64) -> (Item, String) {
    let folder = expand_tilde(&s.folder);
    let size = s.size.as_deref().and_then(fmt::parse_bytes).unwrap_or(1_000_000);
    let age = s.age.as_deref().and_then(fmt::parse_duration).unwrap_or(30 * 86_400);
    let mut it =
        Item { path: folder.join(&s.name), is_dir: s.dir, logical: size, alloc: size, mtime: now - age, flags: 0, depth: 1, dir: None };
    if s.dir {
        it.dir = Some(items::DirSummary { files: s.files.unwrap_or(10), dirs: 1, newest: now - age, exts: vec![] });
    }
    let prov = tidy_ops::meta::Provenance { where_from: s.where_from.iter().cloned().collect(), downloaded_by: None, last_used: None };
    let text = items::evidence_with(&it, now, true, Some(prov));
    (it, text)
}

// ---- tidy calibrate ------------------------------------------------------------

#[derive(Args)]
pub struct CalibrateCmd {
    #[arg(long)]
    pub policy: Option<PathBuf>,
    /// How many items to label this session.
    #[arg(short = 'n', long, default_value_t = 150)]
    pub count: usize,
    /// Target precision for actions (keep rules aim for full recall instead).
    #[arg(long, default_value_t = 0.95)]
    pub target: f64,
    /// Only fit thresholds from existing labels (no labelling).
    #[arg(long)]
    pub fit_only: bool,
    /// Where rules without a scope look for items (default: the current folder).
    #[arg(default_value = ".")]
    pub paths: Vec<String>,
}

pub fn cmd_calibrate(c: CalibrateCmd) -> Result<()> {
    let (pol, _, _) = crate::plan_cmds::load_policy(c.policy.as_deref())?;
    let built = build_judge(&pol, false)?.context("calibration needs a judge in the policy")?;
    let roots: Vec<PathBuf> = c.paths.iter().filter_map(|p| std::fs::canonicalize(expand_tilde(p)).ok()).collect();
    let calib = Calibration::load();
    let rules = l2::compile(&pol, &roots, Some(&built.id), &calib);
    let now = fmt::now_secs();
    let mut labels = calib::load_labels();
    if !c.fit_only {
        if !std::io::stdin().is_terminal() {
            bail!("labelling is interactive; run in a terminal (or use --fit-only)");
        }
        let mut items = l2::collect(&rules, now);
        let labelled: HashSet<PathBuf> = labels.iter().map(|l| l.path.clone()).collect();
        items.retain(|(it, app)| !labelled.contains(&it.path) && app.iter().any(|&i| !rules[i].deterministic()));
        // Spread the sample across rules: round-robin by first applicable rule, pseudo-random order.
        items.sort_by_key(|(it, _)| blake3::hash(tidy_scan::path::path_bytes(&it.path)).as_bytes()[0..8].to_vec());
        let mut sample = Vec::new();
        let mut by_rule: std::collections::BTreeMap<usize, Vec<(Item, Vec<usize>)>> = Default::default();
        for x in items {
            by_rule.entry(x.1[0]).or_default().push(x);
        }
        while sample.len() < c.count && by_rule.values().any(|v| !v.is_empty()) {
            for v in by_rule.values_mut() {
                if let Some(x) = v.pop() {
                    sample.push(x);
                }
            }
        }
        sample.truncate(c.count);
        println!("{} items to label · judge {}", sample.len(), built.id);
        let media = l2::Media::load(&pol, built.judge.caps());
        let reqs: Vec<Request> = sample
            .iter()
            .map(|(it, app)| l2::build_request(&pol, it, &app.iter().map(|&i| &rules[i]).collect::<Vec<_>>(), now, true, Some(&media)))
            .collect();
        let pb = progress_bar("judging");
        let resps = tidy_judge::decide_many(built.judge.as_ref(), &reqs, pol.judge.concurrency.unwrap_or(32), &pb);
        let stdin = std::io::stdin();
        for (k, ((it, app), resp)) in sample.iter().zip(resps).enumerate() {
            let resp = resp?;
            let rs: Vec<&CompiledRule> = app.iter().map(|&i| &rules[i]).collect();
            println!("\n{} {}", ui::bold(&format!("[{}/{}]", k + 1, sample.len())), display_path(&it.path));
            for l in reqs[k].text.lines().skip(1).filter(|l| !l.starts_with("path:")) {
                println!("    {}", ui::dim(l));
            }
            for (n, r) in rs.iter().enumerate() {
                let p = l2::rule_probability(r, &resp.answers);
                println!(
                    "  [{}] {:<20} {:<8} model p={}",
                    n + 1,
                    r.rule.id,
                    r.rule.action,
                    p.map(|p| format!("{p:.2}")).unwrap_or("–".into())
                );
            }
            print!("  which rule should apply? [1-{}], 0 = none, s = skip, q = finish: ", rs.len());
            std::io::stdout().flush()?;
            let mut line = String::new();
            stdin.lock().read_line(&mut line)?;
            let a = line.trim();
            if a == "q" {
                break;
            }
            if a == "s" || a.is_empty() {
                continue;
            }
            let rule = match a.parse::<usize>() {
                Ok(0) => None,
                Ok(n) if n <= rs.len() => Some(rs[n - 1].rule.id.clone()),
                _ => {
                    println!("  (skipped: not a choice)");
                    continue;
                }
            };
            let l = Label { path: it.path.clone(), rule, evidence: reqs[k].text.clone(), at: now };
            calib::append_label(&l)?;
            labels.push(l);
        }
    }
    fit_and_save(&pol, &rules, &built, &labels, c.target)
}

/// Whether a rule could apply to a labelled path (scope + name-based filters;
/// size and age are not recorded with labels).
/// The current folder: where rules without a scope apply by default.
fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Whether a rule covers a labelled path. A rule without a scope covers any
/// label: the labels themselves are the chosen sample, wherever they were
/// collected.
fn label_applies(r: &CompiledRule, path: &Path) -> bool {
    if !r.rule.scope.is_empty() && !r.scopes.iter().any(|s| path.starts_with(s)) {
        return false;
    }
    let it = Item { path: path.to_path_buf(), is_dir: false, logical: 0, alloc: 0, mtime: 0, flags: 0, depth: 1, dir: None };
    r.filter.matches_name(&it)
}

fn fit_and_save(pol: &Policy, rules: &[CompiledRule], built: &BuiltJudge, labels: &[Label], target: f64) -> Result<()> {
    let now = fmt::now_secs();
    let mut scored: std::collections::BTreeMap<String, Vec<(f64, bool)>> = Default::default();
    let media = l2::Media::load(pol, built.judge.caps());
    for l in labels {
        let it = Item { path: l.path.clone(), is_dir: false, logical: 0, alloc: 0, mtime: now, flags: 0, depth: 1, dir: None };
        let app: Vec<&CompiledRule> = rules.iter().filter(|r| label_applies(r, &l.path) && !r.deterministic()).collect();
        if app.is_empty() {
            continue;
        }
        let req = l2::build_request_text(pol, l.evidence.clone(), &it, &app, Some(&media));
        let resp = tidy_judge::decide_split(built.judge.as_ref(), &req)?;
        for r in &app {
            if let Some(p) = l2::rule_probability(r, &resp.answers) {
                scored.entry(r.rule.id.clone()).or_default().push((p, l.rule.as_deref() == Some(&r.rule.id)));
            }
        }
    }
    let mut cal = Calibration::load();
    let entry = cal.0.entry(built.id.clone()).or_default();
    println!("\n{:<20} {:>6} {:>9} {:>7} {:>5}", "rule", "τ", "precision", "recall", "n");
    for r in rules {
        let Some(s) = scored.get(&r.rule.id) else { continue };
        let fitted = if r.is_keep() {
            // Protection: lowest probability among true positives, with margin.
            let pos: Vec<f64> = s.iter().filter(|x| x.1).map(|x| x.0).collect();
            pos.iter().cloned().reduce(f64::min).map(|m| {
                let t = (m * 0.9).min(0.5);
                let fired = s.iter().filter(|x| x.0 >= t).count().max(1);
                calib::Fitted {
                    threshold: t,
                    precision: pos.len() as f64 / fired as f64,
                    recall: 1.0,
                    positives: pos.len(),
                    n: s.len(),
                    at: now,
                }
            })
        } else {
            calib::fit_threshold(s, target)
        };
        match fitted {
            Some(f) => {
                println!("{:<20} {:>6.2} {:>9.2} {:>7.2} {:>5}", r.rule.id, f.threshold, f.precision, f.recall, f.n);
                entry.insert(r.rule.id.clone(), f);
            }
            None => println!(
                "{:<20} {:>6} {:>9} {:>7} {:>5}  {}",
                r.rule.id,
                "–",
                "–",
                "–",
                s.len(),
                ui::dim("not enough positive labels / target unreachable")
            ),
        }
    }
    cal.save()?;
    println!("{}", ui::dim(&format!("saved {}", display_path(&calib::calibration_path()))));
    Ok(())
}

// ---- tidy bench judges -----------------------------------------------------------

#[derive(Args)]
pub struct BenchJudgesCmd {
    #[arg(long)]
    pub policy: Option<PathBuf>,
    /// Backends to compare (default: all configured).
    #[arg(long, value_delimiter = ',')]
    pub backends: Vec<String>,
    /// Use cached answers (latency is then meaningless).
    #[arg(long)]
    pub cached: bool,
}

pub fn cmd_bench_judges(c: BenchJudgesCmd) -> Result<()> {
    let (pol, _, _) = crate::plan_cmds::load_policy(c.policy.as_deref())?;
    let labels = calib::load_labels();
    if labels.is_empty() {
        bail!("no labels yet — run `tidy calibrate` first");
    }
    let names: Vec<String> = if c.backends.is_empty() { pol.judge.backends.keys().cloned().collect() } else { c.backends.clone() };
    let now = fmt::now_secs();
    println!("{} labelled items\n", labels.len());
    println!("{:<14} {:>8} {:>8} {:>8} {:>9} {:>10} {:>8}", "backend", "agree", "p50 ms", "p95 ms", "tokens", "cost", "errors");
    for n in names {
        let cfg = backend_config(&n, &pol)?;
        let j: Box<dyn Judge> = match tidy_judge::build(&cfg) {
            Ok(j) if c.cached => Box::new(Cached::open(j, Some(tidy_ops::dirs::decisions_cache_dir()))),
            Ok(j) => j,
            Err(e) => {
                println!("{n:<14} {e}");
                continue;
            }
        };
        let calib = Calibration::load();
        let rules = l2::compile(&pol, &[cwd()], Some(&j.id()), &calib);
        let media = l2::Media::load(&pol, j.caps());
        let guards = Guards::new(tidy_rules::GuardConfig { min_age: 0, settle: 0, ..Default::default() });
        let mut cases = Vec::new();
        for l in &labels {
            let it = Item { path: l.path.clone(), is_dir: false, logical: 0, alloc: 0, mtime: now, flags: 0, depth: 1, dir: None };
            let app: Vec<&CompiledRule> = rules.iter().filter(|r| label_applies(r, &l.path)).collect();
            if app.is_empty() {
                continue;
            }
            cases.push((
                l,
                it,
                l2::build_request_text(
                    &pol,
                    l.evidence.clone(),
                    &Item { path: l.path.clone(), is_dir: false, logical: 0, alloc: 0, mtime: now, flags: 0, depth: 1, dir: None },
                    &app,
                    Some(&media),
                ),
            ));
        }
        let reqs: Vec<Request> = cases.iter().map(|c| c.2.clone()).collect();
        let resps = tidy_judge::decide_many(j.as_ref(), &reqs, cfg.concurrency.min(16), &|_, _| {});
        let (mut agree, mut total, mut errors, mut tokens) = (0, 0, 0, 0u64);
        let mut lat = Vec::new();
        for ((l, it, _), r) in cases.iter().zip(resps) {
            let Ok(r) = r else {
                errors += 1;
                continue;
            };
            lat.push(r.latency_ms);
            tokens += r.input_tokens;
            let app: Vec<&CompiledRule> = rules.iter().filter(|rr| label_applies(rr, &l.path)).collect();
            let (kind, _) = l2::decide(it, &app, Some(&r), &guards, now);
            let got = match &kind {
                OutcomeKind::Kept { rule, .. } => Some(rule.clone()),
                OutcomeKind::Propose { entries } => entries.iter().find(|e| e.action.exclusive()).map(|e| e.rule.clone()),
                _ => None,
            };
            total += 1;
            if got == l.rule {
                agree += 1;
            }
        }
        lat.sort_unstable();
        let pct = |q: f64| lat.get(((lat.len() as f64 - 1.0) * q).round() as usize).copied().unwrap_or(0);
        let cost = price_per_mtok(&cfg.model).map(|p| format!("${:.4}", tokens as f64 / 1e6 * p)).unwrap_or("n/a".into());
        println!(
            "{:<14} {:>7.1}% {:>8} {:>8} {:>9} {:>10} {:>8}",
            n,
            if total > 0 { agree as f64 * 100.0 / total as f64 } else { 0.0 },
            pct(0.5),
            pct(0.95),
            tokens,
            cost,
            errors
        );
        if let Some(u) = j.usage() {
            let _ = u.requests.load(Ordering::Relaxed);
        }
    }
    Ok(())
}
