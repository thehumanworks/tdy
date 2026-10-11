//! Policy rules (L2) in the CLI. Filled in by the judge milestone.

use anyhow::Result;
use std::path::Path;
use tidy_rules::guards::Guards;
use tidy_rules::planner::Planned;
use tidy_rules::Policy;

pub fn add_policy_entries(_planned: &mut Planned, _policy: &Policy, _guards: &Guards) -> Result<()> {
    Ok(())
}

pub fn explain_rules(_p: &Path, _policy: &Policy) -> Result<()> {
    Ok(())
}

pub fn evidence_summary(v: &serde_json::Value) -> String {
    v.to_string()
}
