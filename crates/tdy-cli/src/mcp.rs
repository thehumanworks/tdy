//! `tdy mcp`: a Model Context Protocol server over stdio.
//!
//! Agents can scan, explain, plan, propose and reject — but `tdy_apply`
//! only executes entries a human approved (CLI, TUI or editor) or that the
//! policy approved in autonomous mode. Nothing an agent sends can approve.

use anyhow::Result;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use tdy_plan::model::*;
use tdy_plan::{ApplyOptions, review, store};
use tdy_rules::guards::{Facts, Guards};
use tdy_scan::{ScanOptions, SizeKind, fmt, path::expand_tilde};

const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Tidy analyses disk usage and proposes cleanup/organisation as plans. \
Workflow: tdy_scan / tdy_top to understand space; tdy_plan (or tdy_propose for specific actions) to create a plan; \
show it to the user and ask them to approve with `tdy review <plan>` in their terminal; then tdy_apply. \
Refused items stay in the plan with their reasons; the user can explicitly override them with `tdy review <plan> --override-refusals` (optionally `--entry <id>`). You cannot approve or override entries: tdy_apply only runs entries a human approved (or the policy approved in autonomous mode). \
Every applied entry can be inspected with tdy_show_plan and most can be reverted with tdy_undo.";

fn tools() -> Value {
    let path = json!({"type": "string", "description": "Absolute path or ~/…"});
    let opt_path = json!({"type": "string", "description": "Absolute path or ~/… (default: the server's current folder)"});
    let cached = json!({"type": "boolean", "default": false,
        "description": "Answer from the last scan of this folder (or one above it) when there is one: instant, but as old as `cached.age_secs` says."});
    json!([
        {"name": "tdy_scan", "description": "Scan a directory: totals and the largest subtrees (dust-style).",
         "inputSchema": {"type": "object", "properties": {"path": opt_path, "lines": {"type": "integer", "default": 20}, "by": {"type": "string", "enum": ["alloc", "logical", "private"], "default": "alloc"}, "cached": cached}},
         "annotations": {"readOnlyHint": true}},
        {"name": "tdy_top", "description": "Largest files under a path, or directories by bytes held directly (dirs=true).",
         "inputSchema": {"type": "object", "properties": {"path": opt_path, "lines": {"type": "integer", "default": 20}, "dirs": {"type": "boolean", "default": false}, "cached": cached}},
         "annotations": {"readOnlyHint": true}},
        {"name": "tdy_explain", "description": "Why a path would or would not be touched: detector match, guard verdicts per action, policy rule probabilities.",
         "inputSchema": {"type": "object", "properties": {"path": path}, "required": ["path"]},
         "annotations": {"readOnlyHint": true}},
        {"name": "tdy_plan", "description": "Create a plan from built-in detectors and the user's policy rules. Refused candidates are retained with reasons for human override; other entries start pending (unless the policy is autonomous).",
         "inputSchema": {"type": "object", "properties": {"paths": {"type": "array", "items": {"type": "string"}}, "rules": {"type": "boolean", "default": true}, "detectors": {"type": "boolean", "default": true}}, "required": ["paths"]}},
        {"name": "tdy_propose", "description": "Propose specific actions as a new plan (pending until a human approves; guard refusals retained for explicit human override). Actions: trash, quarantine, delete, move, copy, rename, tag, archive.",
         "inputSchema": {"type": "object", "properties": {"actions": {"type": "array", "items": {"type": "object", "properties": {
             "action": {"type": "string"}, "path": {"type": "string"}, "dest": {"type": "string", "description": "Full destination path for move/copy/rename"},
             "tags": {"type": "array", "items": {"type": "string"}}, "reason": {"type": "string"}}, "required": ["action", "path", "reason"]}}}, "required": ["actions"]}},
        {"name": "tdy_plans", "description": "List plans with counts.", "inputSchema": {"type": "object", "properties": {}}, "annotations": {"readOnlyHint": true}},
        {"name": "tdy_show_plan", "description": "Entries of a plan (id, unique prefix or 'latest').",
         "inputSchema": {"type": "object", "properties": {"plan": {"type": "string", "default": "latest"}, "status": {"type": "string"}}},
         "annotations": {"readOnlyHint": true}},
        {"name": "tdy_reject", "description": "Reject (or reset to pending) entries of a plan. Agents cannot approve.",
         "inputSchema": {"type": "object", "properties": {"plan": {"type": "string"}, "entries": {"type": "array", "items": {"type": "string"}}, "all": {"type": "boolean"}, "reset": {"type": "boolean", "description": "Set back to pending instead of rejected"}}, "required": ["plan"]}},
        {"name": "tdy_apply", "description": "Apply a plan's human-approved entries (verifies fingerprints and guards first). Use dry_run to rehearse.",
         "inputSchema": {"type": "object", "properties": {"plan": {"type": "string"}, "dry_run": {"type": "boolean", "default": false}, "entries": {"type": "array", "items": {"type": "string"}}}, "required": ["plan"]},
         "annotations": {"destructiveHint": true}},
        {"name": "tdy_undo", "description": "Reverse applied entries of a plan (trash, quarantine, moves, tags, archives).",
         "inputSchema": {"type": "object", "properties": {"plan": {"type": "string"}, "entries": {"type": "array", "items": {"type": "string"}}}, "required": ["plan"]}},
        {"name": "tdy_policy_test", "description": "Run the policy's test cases through the judge and report pass/fail.",
         "inputSchema": {"type": "object", "properties": {}}, "annotations": {"readOnlyHint": true}},
        {"name": "tdy_dupes", "description": "Find duplicate files (size → sample hash → BLAKE3).",
         "inputSchema": {"type": "object", "properties": {"paths": {"type": "array", "items": {"type": "string"}}, "min_size": {"type": "string", "default": "1MB"}}, "required": ["paths"]},
         "annotations": {"readOnlyHint": true}}
    ])
}

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                writeln!(out, "{}", json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}}))?;
                out.flush()?;
                continue;
            }
        };
        let Some(id) = msg.get("id").cloned() else { continue }; // notification
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        let resp = match method {
            "initialize" => {
                let want = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSIONS[0]);
                let v = if PROTOCOL_VERSIONS.contains(&want) { want } else { PROTOCOL_VERSIONS[0] };
                json!({"jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": v,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "tdy", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS
                }})
            }
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools()}}),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let (val, is_err) = match call(name, &args) {
                    Ok(v) => (v, false),
                    Err(e) => (json!({"error": format!("{e:#}")}), true),
                };
                json!({"jsonrpc": "2.0", "id": id, "result": {
                    "content": [{"type": "text", "text": serde_json::to_string_pretty(&val)?}],
                    "structuredContent": val,
                    "isError": is_err
                }})
            }
            m => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method not found: {m}")}}),
        };
        writeln!(out, "{resp}")?;
        out.flush()?;
    }
    Ok(())
}

/// Plans are addressed by id (or unique prefix / "latest") only — never by
/// a file path an agent could have written itself.
fn plan_id(a: &Value) -> Result<Option<&str>> {
    match a.get("plan").and_then(Value::as_str) {
        None => Ok(None),
        Some(p) if p == "latest" || (!p.is_empty() && p.len() <= 16 && p.bytes().all(|b| b.is_ascii_alphanumeric())) => Ok(Some(p)),
        Some(p) => anyhow::bail!("invalid plan id `{p}`"),
    }
}

fn s<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str)
}

fn strs(a: &Value, k: &str) -> Vec<String> {
    a.get(k).and_then(Value::as_array).map(|v| v.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

/// Paths from an agent are data, never flags: anything that would reach a
/// subprocess as `--autonomous`, `--plan` or `--policy` is refused, and the
/// rest is passed after `--`.
fn agent_paths(a: &Value) -> Result<Vec<String>> {
    let paths = strs(a, "paths");
    if let Some(bad) = paths.iter().find(|p| p.starts_with('-') || p.is_empty()) {
        anyhow::bail!("invalid path `{bad}`");
    }
    Ok(paths)
}

fn canon(p: &str) -> Result<PathBuf> {
    Ok(std::fs::canonicalize(expand_tilde(p))?)
}

fn compact(e: &Entry) -> Value {
    json!({
        "id": e.id, "status": e.status, "approved_by": e.approved_by, "action": e.action, "path": e.path, "dest": e.dest, "tags": e.tags,
        "bytes": e.bytes.private.max(e.bytes.logical.min(e.bytes.alloc)), "rule": e.source.rule, "p": e.source.p,
        "reason": e.reason, "refusal": e.refusal, "override_refusal": e.override_refusal, "undo": e.undo, "error": e.outcome.as_ref().and_then(|o| o.error.clone())
    })
}

/// `"cached": true` asks for the last scan instead of a new one.
fn cached(a: &Value) -> bool {
    a.get("cached").and_then(Value::as_bool) == Some(true)
}

fn cached_json(idx: &tdy_scan::Index, from: Option<&std::path::Path>) -> Value {
    match from {
        Some(f) => json!({"from": f, "age_secs": tdy_scan::cache::age_secs(idx)}),
        None => Value::Null,
    }
}

pub fn call(name: &str, a: &Value) -> Result<Value> {
    match name {
        "tdy_scan" => {
            let root = canon(s(a, "path").unwrap_or("."))?;
            let by: SizeKind = s(a, "by").unwrap_or("alloc").parse().map_err(anyhow::Error::msg)?;
            let n = a.get("lines").and_then(Value::as_u64).unwrap_or(20) as usize;
            let opts = ScanOptions {
                top_k: n.max(64),
                top_by: by,
                private: by == SizeKind::Private || cfg!(target_os = "macos"),
                ..Default::default()
            };
            let (idx, from) = crate::scan_cmds::index_for(&root, &opts, cached(a))?;
            let r = idx.root_node();
            Ok(
                json!({"root": idx.root, "logical": r.logical, "alloc": r.alloc, "private": r.private, "files": r.files, "dirs": idx.stats.dirs,
                      "elapsed_ms": idx.stats.elapsed_ms, "cached": cached_json(&idx, from.as_deref()), "tree": idx.tree(0, n, by, None)}),
            )
        }
        "tdy_top" => {
            let root = canon(s(a, "path").unwrap_or("."))?;
            let n = a.get("lines").and_then(Value::as_u64).unwrap_or(20) as usize;
            let dirs = a.get("dirs").and_then(Value::as_bool) == Some(true);
            let opts = ScanOptions { top_k: if dirs { 0 } else { n }, ..Default::default() };
            let (idx, _) = crate::scan_cmds::index_for(&root, &opts, cached(a))?;
            if dirs {
                Ok(json!(idx.top_dirs_own(n).into_iter().map(|(id, b)| json!({"path": idx.path(id), "own_alloc": b})).collect::<Vec<_>>()))
            } else {
                Ok(json!(
                    idx.top_files
                        .iter()
                        .map(|f| json!({"path": idx.file_path(f), "alloc": f.alloc, "logical": f.logical, "mtime": f.mtime}))
                        .collect::<Vec<_>>()
                ))
            }
        }
        "tdy_explain" => {
            let p = canon(s(a, "path").unwrap_or(""))?;
            let (policy, _, _) = crate::plan_cmds::load_policy(None)?;
            let guards = Guards::new(policy.guard_config());
            let md = std::fs::symlink_metadata(&p)?;
            let (files, logical, mtime) = if md.is_dir() {
                tdy_ops::fingerprint::tree_summary(&p)?
            } else {
                use std::os::unix::fs::MetadataExt;
                (1, md.len(), md.mtime())
            };
            let det = if md.is_dir() { tdy_rules::detect::classify(&p) } else { None };
            let facts = Facts { mtime_max: Some(mtime), flags: 0, regenerable: det.is_some(), min_age: det.and_then(|d| d.3) };
            let guards_v: Vec<Value> = [Action::Delete, Action::Trash, Action::Move, Action::Tag]
                .iter()
                .map(|&act| {
                    let r = guards.check(&p, act, &facts, None);
                    json!({"action": act, "allowed": r.is_ok(), "block": r.err().map(|b| b.to_string())})
                })
                .collect();
            Ok(json!({"path": p, "files": files, "logical": logical, "newest_mtime": mtime,
                      "detector": det.map(|d| json!({"id": d.0, "what": d.1, "hint": d.2})), "guards": guards_v}))
        }
        "tdy_plan" => {
            let paths = agent_paths(a)?;
            let mut args = vec!["plan".to_string(), "--json".to_string()];
            if a.get("rules").and_then(Value::as_bool) == Some(false) {
                args.push("--no-rules".into());
            }
            if a.get("detectors").and_then(Value::as_bool) == Some(false) {
                args.push("--no-detectors".into());
            }
            args.push("--".into());
            args.extend(paths);
            let v = run_self(&args)?;
            let id = v.get("plan").and_then(Value::as_str).unwrap_or("latest").to_string();
            let (plan, _) = store::load(Some(&id))?;
            Ok(json!({"plan": id, "counts": plan.counts(), "entries": plan.entries.iter().take(200).map(compact).collect::<Vec<_>>(),
                      "next": review_next(&plan)}))
        }
        "tdy_propose" => propose(a),
        "tdy_plans" => {
            let plans = store::list()?;
            Ok(json!(plans.iter().map(|p| json!({"id": p.header.id, "created": p.header.created, "roots": p.header.roots, "counts": p.counts(), "note": p.header.note})).collect::<Vec<_>>()))
        }
        "tdy_show_plan" => {
            let (plan, _) = store::load(plan_id(a)?)?;
            let st = s(a, "status");
            Ok(json!({"plan": plan.header.id, "mode": plan.header.mode, "counts": plan.counts(),
                      "entries": plan.entries.iter().filter(|e| st.is_none_or(|x| e.status.as_str().starts_with(&x[..x.len().min(5)]))).map(compact).collect::<Vec<_>>()}))
        }
        "tdy_reject" => {
            let (mut plan, _) = store::load(plan_id(a)?)?;
            let ids = strs(a, "entries");
            let all = a.get("all").and_then(Value::as_bool) == Some(true);
            if ids.is_empty() && !all {
                anyhow::bail!("pass `entries` or `all: true`");
            }
            let to = if a.get("reset").and_then(Value::as_bool) == Some(true) { Status::Pending } else { Status::Rejected };
            let f = review::Filter { ids, ..Default::default() };
            let n = review::set_status(&mut plan, &f, to, "agent");
            store::save(&plan)?;
            Ok(json!({"plan": plan.header.id, "changed": n, "status": to}))
        }
        "tdy_apply" => {
            let (mut plan, _) = store::load(plan_id(a)?)?;
            let wanted: Vec<String> = strs(a, "entries");
            let mut only = HashSet::new();
            let mut refused = Vec::new();
            for e in &plan.entries {
                if !wanted.is_empty() && !wanted.iter().any(|w| e.id.starts_with(w.as_str())) {
                    continue;
                }
                match (e.status, e.approved_by.as_deref()) {
                    (Status::Approved, Some("human" | "policy")) => {
                        only.insert(e.id.clone());
                    }
                    (Status::Pending | Status::Approved, _) => refused.push(e.id.clone()),
                    _ => {}
                }
            }
            if only.is_empty() {
                return Ok(json!({"plan": plan.header.id, "applied": 0, "not_approved": refused,
                                 "message": format!("Nothing is approved by a human. Ask the user to run `tdy review {}`.", plan.header.id)}));
            }
            let policy = match &plan.header.policy {
                Some(p) if p.exists() => tdy_rules::Policy::load(p)?.0,
                _ => tdy_rules::Policy::default(),
            };
            let guard = crate::plan_cmds::apply_guard(&policy);
            let dry = a.get("dry_run").and_then(Value::as_bool) == Some(true);
            let mut events = Vec::new();
            let r = tdy_plan::sync::apply(&mut plan, &ApplyOptions { dry_run: dry, only: Some(only), verify: true }, &guard, &mut |ev| {
                events.push(match ev {
                    tdy_plan::Event::Ok(e, m) => json!({"entry": e.id, "result": "ok", "detail": m}),
                    tdy_plan::Event::Skip(e, m) => json!({"entry": e.id, "result": "skipped", "detail": m}),
                    tdy_plan::Event::Fail(e, m) => json!({"entry": e.id, "result": "failed", "detail": m}),
                    tdy_plan::Event::WouldDo(e, m) => json!({"entry": e.id, "result": "would", "detail": m}),
                    tdy_plan::Event::Note(m) => json!({"note": m}),
                })
            })?;
            Ok(json!({"plan": plan.header.id, "dry_run": dry, "report": r, "events": events, "not_approved": refused}))
        }
        "tdy_undo" => {
            let (mut plan, _) = store::load(plan_id(a)?)?;
            let ids = strs(a, "entries");
            let only: Option<HashSet<String>> = if ids.is_empty() {
                None
            } else {
                Some(plan.entries.iter().filter(|e| ids.iter().any(|w| e.id.starts_with(w.as_str()))).map(|e| e.id.clone()).collect())
            };
            let mut events = Vec::new();
            let r = tdy_plan::sync::undo(&mut plan, only.as_ref(), &mut |ev| {
                if let tdy_plan::Event::Ok(e, m) | tdy_plan::Event::Skip(e, m) | tdy_plan::Event::Fail(e, m) = ev {
                    events.push(json!({"entry": e.id, "detail": m}));
                }
            })?;
            Ok(json!({"plan": plan.header.id, "report": r, "events": events}))
        }
        "tdy_policy_test" => {
            let out = std::process::Command::new(std::env::current_exe()?).args(["policy", "test"]).env("NO_COLOR", "1").output()?;
            Ok(json!({"passed": out.status.success(), "output": String::from_utf8_lossy(&out.stdout)}))
        }
        "tdy_dupes" => {
            let paths = agent_paths(a)?;
            let min = s(a, "min_size").unwrap_or("1MB");
            if tdy_scan::fmt::parse_bytes(min).is_none() {
                anyhow::bail!("invalid min_size `{min}`");
            }
            // `--min-size=X` binds the value, so it can't be read as a flag.
            let mut args = vec!["dupes".to_string(), "--json".to_string(), format!("--min-size={min}"), "--".to_string()];
            args.extend(paths);
            run_self(&args)
        }
        _ => anyhow::bail!("unknown tool `{name}`"),
    }
}

fn review_next(plan: &Plan) -> String {
    let id = &plan.header.id;
    let mut next = format!("Ask the user to review and approve: `tdy review {id}` (agents cannot approve or override)");
    if plan.entries.iter().any(|e| e.refusal.is_some()) {
        next.push_str(&format!(
            ". To explicitly override refused items: `tdy review {id} --override-refusals` (add `--entry ID` to select items)"
        ));
    }
    next
}

fn run_self(args: &[String]) -> Result<Value> {
    let out = std::process::Command::new(std::env::current_exe()?).args(args).env("NO_COLOR", "1").output()?;
    if !out.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let last = text.lines().rev().find(|l| l.trim_start().starts_with('{')).unwrap_or("{}");
    Ok(serde_json::from_str(last)?)
}

fn propose(a: &Value) -> Result<Value> {
    use crate::propose::{By, Proposal};
    let actions = a.get("actions").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut items = Vec::new();
    let mut rejected = Vec::new();
    for act in actions {
        let path_s = s(&act, "path").unwrap_or("");
        let Ok(path) = canon(path_s) else {
            rejected.push(json!({"path": path_s, "why": "does not exist"}));
            continue;
        };
        let action: Action = match s(&act, "action").unwrap_or("").parse() {
            Ok(x) => x,
            Err(e) => {
                rejected.push(json!({"path": path, "why": e}));
                continue;
            }
        };
        items.push(Proposal {
            action,
            path,
            dest: s(&act, "dest").map(expand_tilde),
            tags: strs(&act, "tags"),
            reason: s(&act, "reason").unwrap_or("proposed by agent").to_string(),
            bytes: None,
        });
    }
    let out = crate::propose::propose(items, By::Agent, "proposed by agent")?;
    rejected.extend(out.rejected.into_iter().map(|(p, why)| json!({"path": p, "why": why})));
    let Some(plan) = out.plan else {
        return Ok(json!({"plan": null, "rejected": rejected}));
    };
    Ok(json!({"plan": plan.header.id, "entries": plan.entries.iter().map(compact).collect::<Vec<_>>(), "rejected": rejected,
              "next": review_next(&plan), "reclaimable": fmt::bytes(plan.counts().reclaimable)}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_cannot_apply_unapproved_entries() {
        let _lock = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let t = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("TDY_HOME", t.path().join("state"));
            std::env::set_var("XDG_DATA_HOME", t.path().join("share"));
            std::env::set_var("TDY_CONFIG_DIR", t.path().join("cfg"));
            std::env::set_var("TDY_CACHE_DIR", t.path().join("cache"));
            std::env::set_var("HOME", t.path());
        }
        let f = t.path().join("victim.bin");
        std::fs::write(&f, vec![0u8; 1000]).unwrap();
        // make it old enough for the removal guard
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86_400))
            .unwrap();
        let v = call("tdy_propose", &json!({"actions": [{"action": "trash", "path": f, "reason": "test"}]})).unwrap();
        let plan = v["plan"].as_str().unwrap().to_string();
        // pending: refused
        let r = call("tdy_apply", &json!({"plan": plan})).unwrap();
        assert_eq!(r["applied"], 0);
        assert!(f.exists());
        // an agent (or anything) flipping the status without human approval: refused
        let (mut p, _) = store::load(Some(&plan)).unwrap();
        p.entries[0].status = Status::Approved;
        p.entries[0].approved_by = Some("agent".into());
        store::save(&p).unwrap();
        let r = call("tdy_apply", &json!({"plan": plan})).unwrap();
        assert_eq!(r["applied"], 0, "{r}");
        assert!(f.exists());
        // the agent can reject
        let r = call("tdy_reject", &json!({"plan": plan, "all": true, "entries": [p.entries[0].id]})).unwrap();
        assert_eq!(r["changed"], 1);
        // a human approves via the review path → applies
        let (mut p, _) = store::load(Some(&plan)).unwrap();
        review::set_status(&mut p, &review::Filter::default(), Status::Approved, "human");
        store::save(&p).unwrap();
        let r = call("tdy_apply", &json!({"plan": plan})).unwrap();
        assert_eq!(r["report"]["applied"], 1, "{r}");
        assert!(!f.exists());
        let r = call("tdy_undo", &json!({"plan": plan})).unwrap();
        assert_eq!(r["report"]["undone"], 1);
        assert!(f.exists());
        // flags can't be smuggled in as paths
        assert!(call("tdy_plan", &json!({"paths": ["--autonomous", "/tmp"]})).is_err());
        assert!(call("tdy_dupes", &json!({"paths": ["--plan"]})).unwrap_err().to_string().contains("invalid path"));
        assert!(call("tdy_dupes", &json!({"paths": ["/tmp"], "min_size": "--plan"})).unwrap_err().to_string().contains("invalid min_size"));
        assert!(call("tdy_apply", &json!({"plan": "/tmp/evil.jsonl"})).is_err());
        // guards apply to proposals too
        // (/etc/passwd exists everywhere, including the Nix build sandbox)
        let v = call("tdy_propose", &json!({"actions": [{"action": "delete", "path": "/etc/passwd", "reason": "x"}]})).unwrap();
        assert!(v["plan"].is_string());
        assert_eq!(v["entries"][0]["status"], "rejected");
        assert!(v["entries"][0]["refusal"].as_str().unwrap().contains("system"));
        assert!(v["rejected"][0]["why"].as_str().unwrap().contains("system"));
        assert!(v["next"].as_str().unwrap().contains("--override-refusals"));

        // A recent file can be overridden by a human and applied through MCP.
        let recent = t.path().join("recent.bin");
        std::fs::write(&recent, b"recent data").unwrap();
        let v = call("tdy_propose", &json!({"actions": [{"action": "quarantine", "path": recent, "reason": "cleanup"}]})).unwrap();
        let id = v["plan"].as_str().unwrap();
        assert_eq!(v["entries"][0]["status"], "rejected");
        assert!(v["entries"][0]["refusal"].as_str().unwrap().contains("recent"));
        let (mut p, _) = store::load(Some(id)).unwrap();
        assert_eq!(review::set_status(&mut p, &review::Filter::default(), Status::Approved, "human"), 0);
        assert_eq!(review::override_refusals(&mut p, &review::Filter::default()), 1);
        store::save(&p).unwrap();
        let shown = call("tdy_show_plan", &json!({"plan": id})).unwrap();
        assert_eq!(shown["entries"][0]["override_refusal"], true);
        let r = call("tdy_apply", &json!({"plan": id, "dry_run": true})).unwrap();
        assert_eq!(r["report"]["would"], 1, "{r}");
        assert!(recent.exists());
        let r = call("tdy_apply", &json!({"plan": id})).unwrap();
        assert_eq!(r["report"]["applied"], 1, "{r}");
        assert!(!recent.exists());
        let r = call("tdy_undo", &json!({"plan": id})).unwrap();
        assert_eq!(r["report"]["undone"], 1, "{r}");
        assert!(recent.exists());
    }
}
