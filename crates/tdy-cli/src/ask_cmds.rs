//! `tdy ask "trash installers in Downloads older than 30 days"`: a plan
//! from a request in plain English, used by the CLI and the UI.

use crate::judge_cmds::L2Summary;
use crate::ui;
use anyhow::{Context, Result, bail};
use clap::Args;
use tdy_plan::{Mode, Plan, store};
use tdy_rules::ask::Request;
use tdy_rules::guards::Guards;
use tdy_rules::planner::{self, Planned};
use tdy_scan::path::{display_path, expand_tilde};

#[derive(Args)]
pub struct AskCmd {
    /// What to do, in plain English (quotes optional), e.g.
    /// `trash screenshots in Downloads older than 30 days`.
    #[arg(required = true, trailing_var_arg = true)]
    pub request: Vec<String>,
    /// Where folders named without a path are looked for, and where to
    /// look when the request names no folder (default: the current folder).
    #[arg(long = "from", value_name = "FOLDER")]
    pub from: Option<String>,
    /// Only show how the request was understood.
    #[arg(long)]
    pub explain: bool,
    #[arg(long)]
    pub json: bool,
}

/// A plan made from a request, not saved yet.
pub struct Asked {
    pub plan: Plan,
    pub summary: L2Summary,
    pub blocked: usize,
    pub blocked_bytes: u64,
}

/// Run `req` as a one-off rule next to your keep rules (presets
/// included), which still protect what they protect. Entries wait for
/// review whatever the policy's mode.
pub fn ask_plan(req: &Request, progress: &(dyn Fn(usize, usize) + Sync)) -> Result<Asked> {
    let (policy, hash, ppath) = crate::plan_cmds::load_policy(None)?;
    let guards = Guards::new(policy.guard_config());
    let mut p = policy.clone();
    p.rules.retain(|r| r.action == "keep");
    p.rules.push(req.rule());
    p.mode = Mode::Manual;
    if req.criterion.is_some() && crate::judge_cmds::build_judge(&p, false)?.is_none() {
        bail!(
            "deciding which items are “{}” needs a judge, and none is configured (see `tdy init`); \
             or describe the items by kind, age or size only",
            req.criterion.as_deref().unwrap_or_default()
        );
    }
    let mut header = planner::new_header(vec![req.scope.clone()], Mode::Manual, ppath, hash, None);
    header.note = Some(format!("asked: {}", req.text));
    let mut planned = Planned { plan: Plan { header, entries: vec![] }, blocked: vec![], indexes: vec![] };
    let summary = crate::judge_cmds::policy_entries(&mut planned, &p, &guards, progress)?;
    let blocked_bytes = planned.blocked.iter().map(|b| b.bytes).sum();
    Ok(Asked { plan: planned.plan, summary, blocked: planned.blocked.len(), blocked_bytes })
}

pub fn cmd_ask(c: AskCmd) -> Result<()> {
    let here = match &c.from {
        Some(f) => std::fs::canonicalize(expand_tilde(f)).with_context(|| f.clone())?,
        None => std::env::current_dir()?,
    };
    let text = c.request.join(" ");
    let req = tdy_rules::ask::parse(&text, &here).map_err(anyhow::Error::msg)?;
    let lines = req.describe();
    if !c.json {
        for (k, v) in &lines {
            println!("  {}  {v}", ui::dim(&format!("{k:>5}")));
        }
    }
    if c.explain {
        if c.json {
            let m: serde_json::Map<String, serde_json::Value> = lines.into_iter().map(|(k, v)| (k.to_string(), v.into())).collect();
            println!("{}", serde_json::json!({"understood": m}));
        }
        return Ok(());
    }
    let progress: Box<dyn Fn(usize, usize) + Sync> =
        if c.json { Box::new(|_, _| {}) } else { Box::new(crate::judge_cmds::progress_bar("judging")) };
    let a = ask_plan(&req, &*progress)?;
    let path = store::save(&a.plan)?;
    let counts = a.plan.counts();
    if c.json {
        println!(
            "{}",
            serde_json::json!({"plan": a.plan.header.id, "path": path, "counts": counts, "blocked": a.blocked,
                               "understood": lines.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<std::collections::BTreeMap<_, _>>()})
        );
        return Ok(());
    }
    if let Some(j) = a.summary.judge_line() {
        eprintln!("{}", ui::dim(&j));
    }
    for w in &a.summary.warnings {
        eprintln!("{}", ui::yellow(&format!("warning: {w}")));
    }
    println!(
        "{} {}  {}",
        ui::bold("plan"),
        ui::bold(&a.plan.header.id),
        ui::dim(&format!(
            "{} {} · {} reclaimable · {} blocked",
            counts.total,
            if counts.total == 1 { "entry" } else { "entries" },
            tdy_scan::fmt::bytes(counts.reclaimable),
            a.blocked
        ))
    );
    for e in a.plan.entries.iter().take(20) {
        println!("  {}", tdy_plan::review::line(e));
    }
    if a.plan.entries.is_empty() {
        println!("{}", ui::dim(&format!("nothing in {} matched", display_path(&req.scope))));
    } else {
        println!("{}", ui::dim(&format!("next: tdy review {}", a.plan.header.id)));
    }
    Ok(())
}
