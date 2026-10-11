mod judge_cmds;
mod plan_cmds;
mod scan_cmds;
mod tui;
mod ui;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "tidy", version, about = "Analyse, clean up, compress and organise files on disk", propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scan a directory and show where the space goes.
    Scan(scan_cmds::ScanCmd),
    /// Largest files, or directories by bytes held directly.
    Top(scan_cmds::TopCmd),
    /// What changed since the last saved snapshot.
    Diff(scan_cmds::DiffCmd),
    /// Propose actions (detectors + policy rules) as a reviewable plan.
    Plan(plan_cmds::PlanCmd),
    /// List plans.
    Plans,
    /// Print a plan's entries.
    Show(plan_cmds::ShowCmd),
    /// Approve or reject plan entries (TUI, $EDITOR, or bulk flags).
    Review(plan_cmds::ReviewCmd),
    /// Carry out the approved entries of a plan.
    Apply(plan_cmds::ApplyCmd),
    /// Reverse applied entries of a plan.
    Undo(plan_cmds::UndoCmd),
    /// Why a path would (not) be touched: detectors, guards, policy rules.
    Explain(plan_cmds::ExplainCmd),
    /// Permanently delete old quarantine buckets.
    Purge(plan_cmds::PurgeCmd),
    /// List built-in detectors.
    Detectors,
    /// Write an example policy to ~/.config/tidy/policy.toml.
    Init(plan_cmds::InitCmd),
}

fn main() {
    // Exit quietly when piped into `head` and friends.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    let r = match cli.cmd {
        Cmd::Scan(c) => scan_cmds::cmd_scan(c),
        Cmd::Top(c) => scan_cmds::cmd_top(c),
        Cmd::Diff(c) => scan_cmds::cmd_diff(c),
        Cmd::Plan(c) => plan_cmds::cmd_plan(c),
        Cmd::Plans => plan_cmds::cmd_plans(),
        Cmd::Show(c) => plan_cmds::cmd_show(c),
        Cmd::Review(c) => plan_cmds::cmd_review(c),
        Cmd::Apply(c) => plan_cmds::cmd_apply(c),
        Cmd::Undo(c) => plan_cmds::cmd_undo(c),
        Cmd::Explain(c) => plan_cmds::cmd_explain(c),
        Cmd::Purge(c) => plan_cmds::cmd_purge(c),
        Cmd::Detectors => plan_cmds::cmd_detectors(),
        Cmd::Init(c) => plan_cmds::cmd_init(c),
    };
    if let Err(e) = r {
        eprintln!("tidy: {e:#}");
        std::process::exit(1);
    }
}
