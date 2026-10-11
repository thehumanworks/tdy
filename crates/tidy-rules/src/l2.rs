//! L2: natural-language policy rules decided by a judge.
//!
//! One request per item carries every question for every rule that applies
//! to it (plus shared safety questions when a removal is possible). Code then
//! combines the probabilities: keep rules veto at a low bar, the first
//! exclusive rule that clears its threshold wins, tags accumulate, and
//! removals must also look safe. The model never writes paths.

use crate::calib::Calibration;
use crate::guards::{Facts, Guards};
use crate::items::{self, CompiledFilter, Item};
use crate::policy::{Policy, Rule, subject_refs};
use anyhow::Result;
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use tidy_judge::{Answer, Judge, QKind, Question, Request, Response};
use tidy_plan::model::*;

pub const DEFAULT_KEEP_THRESHOLD: f64 = 0.15;
pub const DEFAULT_THRESHOLD: f64 = 0.8;

pub const REGENERABLE_Q: &str = "Could the owner get ITEM back without redoing original work — for example by downloading, reinstalling or rebuilding it again, or because another copy exists elsewhere?";
pub const LOSS_Q: &str = "How costly would it be for the owner to lose ITEM?";

pub fn loss_levels() -> Vec<(String, String)> {
    vec![
        ("None".into(), "Trivially recreated or re-downloaded".into()),
        ("Minor".into(), "Some effort to recreate or obtain again".into()),
        ("Significant".into(), "Hours of work, or hard to obtain again".into()),
        ("Irreplaceable".into(), "Unique personal data such as own photos, documents or creative work".into()),
    ]
}

#[derive(Clone, Debug)]
pub struct CompiledRule {
    pub idx: usize,
    pub rule: Rule,
    pub qid: String,
    pub filter: CompiledFilter,
    pub scopes: Vec<PathBuf>,
    pub action: Option<Action>, // None = keep
    pub threshold: f64,
    pub dest_options: Vec<(String, PathBuf)>,
}

impl CompiledRule {
    pub fn is_keep(&self) -> bool {
        self.action.is_none()
    }
    pub fn exclusive(&self) -> bool {
        self.action.is_some_and(|a| a.exclusive())
    }
    pub fn removal(&self) -> bool {
        matches!(self.action, Some(Action::Delete | Action::Trash | Action::Quarantine | Action::Archive))
    }
    pub fn deterministic(&self) -> bool {
        self.rule.when.is_empty() && self.rule.to.as_ref().is_none_or(|t| t.pick_from.is_none())
    }
    pub fn applies(&self, it: &Item, now: i64) -> bool {
        let Some(scope) = self.scopes.iter().find(|s| it.path.starts_with(s) && it.path != **s) else { return false };
        let depth = it.path.strip_prefix(scope).map(|r| r.components().count()).unwrap_or(usize::MAX);
        let mut it2 = it.clone();
        it2.depth = depth;
        self.filter.matches(&it2, now)
    }
}

fn sanitize(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' { c } else { '_' }).collect()
}

pub fn compile(policy: &Policy, roots: &[PathBuf], judge_id: Option<&str>, calib: &Calibration) -> Vec<CompiledRule> {
    policy
        .rules
        .iter()
        .enumerate()
        .map(|(idx, r)| {
            let action = if r.action == "keep" { None } else { r.action.parse::<Action>().ok() };
            let calibrated = judge_id.and_then(|j| calib.get(j, &r.id));
            let threshold = r.threshold.or(calibrated).unwrap_or(if action.is_none() { DEFAULT_KEEP_THRESHOLD } else { DEFAULT_THRESHOLD });
            let mut dest_options = Vec::new();
            if let Some(pf) = r.to.as_ref().and_then(|t| t.pick_from.as_ref()) {
                dest_options = expand_dir_glob(pf);
            }
            CompiledRule {
                idx,
                rule: r.clone(),
                qid: sanitize(&r.id),
                filter: CompiledFilter::from(&r.filter),
                scopes: items::scopes(r, roots),
                action,
                threshold,
                dest_options,
            }
        })
        .collect()
}

/// Existing directories matching a `dir/*` style glob (one level).
pub fn expand_dir_glob(pat: &str) -> Vec<(String, PathBuf)> {
    let p = tidy_scan::path::expand_tilde(pat);
    let (base, leaf) = match (p.parent(), p.file_name()) {
        (Some(b), Some(l)) => (b.to_path_buf(), l.to_string_lossy().into_owned()),
        _ => return vec![],
    };
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&base) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if items::glob_match(&leaf.to_lowercase(), &name.to_lowercase()) {
                v.push((name, e.path()));
            }
        }
    }
    v.sort();
    v.truncate(60);
    v
}

/// Probability that a rule matches, from its answered criteria.
pub fn rule_probability(r: &CompiledRule, ans: &BTreeMap<String, Answer>) -> Option<f64> {
    if r.rule.when.is_empty() {
        return Some(1.0);
    }
    let get = |kind: &str, i: usize| ans.get(&format!("{}.{kind}{i}", r.qid)).and_then(|a| a.p());
    let mut p = 1.0f64;
    if !r.rule.when.any.is_empty() {
        let v: Vec<f64> = (0..r.rule.when.any.len()).filter_map(|i| get("any", i)).collect();
        if v.is_empty() {
            return None;
        }
        p = p.min(v.iter().cloned().fold(0.0, f64::max));
    }
    if !r.rule.when.all.is_empty() {
        let v: Vec<f64> = (0..r.rule.when.all.len()).filter_map(|i| get("all", i)).collect();
        if v.len() < r.rule.when.all.len() {
            return None;
        }
        p = p.min(v.iter().cloned().fold(1.0, f64::min));
    }
    if !r.rule.when.none.is_empty() {
        let v: Vec<f64> = (0..r.rule.when.none.len()).filter_map(|i| get("none", i)).collect();
        if v.is_empty() {
            return None;
        }
        p = p.min(1.0 - v.iter().cloned().fold(0.0, f64::max));
    }
    Some(p)
}

fn criterion_text(policy: &Policy, c: &str) -> String {
    let mut out = c.to_string();
    for name in subject_refs(c) {
        let desc = policy
            .subjects
            .get(&name)
            .and_then(|s| s.description.clone())
            .unwrap_or_else(|| format!("the person called \"{name}\" (see the reference photos)"));
        out = out.replace(&format!("{{{name}}}"), &desc);
    }
    out
}

/// Images available to requests: the judge's image budget and reference
/// photos of policy subjects (thumbnailed once).
#[derive(Default)]
pub struct Media {
    pub max_images: usize,
    pub subjects: BTreeMap<String, Vec<tidy_judge::Image>>,
}

impl Media {
    pub fn load(policy: &Policy, caps: tidy_judge::Caps) -> Media {
        let max_images = if caps.images { caps.max_images } else { 0 };
        let mut subjects = BTreeMap::new();
        if max_images > 1 {
            for (name, s) in &policy.subjects {
                let imgs: Vec<tidy_judge::Image> = s
                    .refs
                    .iter()
                    .filter_map(|r| crate::media::thumbnail(&tidy_scan::path::expand_tilde(r), 384))
                    .map(|mut i| {
                        i.caption = Some(format!(
                            "Reference photo of \"{name}\"{}",
                            s.description.as_ref().map(|d| format!(" ({d})")).unwrap_or_default()
                        ));
                        i
                    })
                    .collect();
                subjects.insert(name.clone(), imgs);
            }
        }
        Media { max_images, subjects }
    }
}

/// Build the request for one item given the rules that apply to it.
pub fn build_request(policy: &Policy, it: &Item, rules: &[&CompiledRule], now: i64, redact: bool, media: Option<&Media>) -> Request {
    build_request_text(policy, items::evidence(it, now, redact), it, rules, media)
}

/// Same, with the evidence text supplied by the caller.
pub fn build_request_text(policy: &Policy, text: String, it: &Item, rules: &[&CompiledRule], media: Option<&Media>) -> Request {
    let mut text = text;
    let mut qs = Vec::new();
    let mut want_safety = false;
    let mut content = false;
    for r in rules {
        for (kind, list) in [("any", &r.rule.when.any), ("all", &r.rule.when.all), ("none", &r.rule.when.none)] {
            for (i, c) in list.iter().enumerate() {
                qs.push(Question::predicate(
                    format!("{}.{kind}{i}", r.qid),
                    format!("Is this true of ITEM? {}", criterion_text(policy, c)),
                ));
            }
        }
        if !r.dest_options.is_empty() {
            let mut opts: Vec<(String, String)> =
                r.dest_options.iter().map(|(n, p)| (n.clone(), format!("the folder {}", tidy_scan::path::display_path(p)))).collect();
            opts.push(("none".into(), "None of these folders is a good fit for ITEM".into()));
            qs.push(Question::choice(format!("{}.dest", r.qid), "Which existing folder should ITEM be filed into?", opts));
        }
        if r.removal() && !r.rule.when.is_empty() {
            want_safety = true;
        }
        content |= r.rule.content.unwrap_or(false);
    }
    if want_safety {
        qs.push(Question::predicate("safety.regenerable", REGENERABLE_Q));
        qs.push(Question::score("safety.loss", LOSS_Q, loss_levels()));
    }
    if content
        && !it.is_dir
        && matches!(it.kind(), "document" | "code" | "file")
        && let Some(ex) = items::text_excerpt(&it.path, 2048)
    {
        text.push_str("\nCONTENT (first 2 KB)\n");
        text.push_str(&ex);
    }
    let mut images = Vec::new();
    if let Some(m) = media.filter(|m| m.max_images > 0) {
        let subjects: Vec<String> = rules
            .iter()
            .flat_map(|r| r.rule.when.any.iter().chain(&r.rule.when.all).chain(&r.rule.when.none))
            .flat_map(|c| subject_refs(c))
            .collect();
        if !it.is_dir
            && it.kind() == "image"
            && (content || !subjects.is_empty())
            && let Some(mut img) = crate::media::thumbnail(&it.path, 512)
        {
            img.caption = Some("ITEM itself (the image being classified)".into());
            images.push(img);
            text.push_str("The first image shows ITEM itself.\n");
        }
        if !images.is_empty() {
            let mut seen = HashSet::new();
            for s in subjects {
                if !seen.insert(s.clone()) {
                    continue;
                }
                for r in m.subjects.get(&s).into_iter().flatten() {
                    if images.len() < m.max_images {
                        images.push(r.clone());
                    }
                }
            }
        }
    }
    Request { text, images, questions: qs }
}

#[derive(Clone, Debug)]
pub struct Outcome {
    pub item: Item,
    pub kind: OutcomeKind,
    pub rule_p: Vec<(String, f64)>,
    pub response: Option<Response>,
}

#[derive(Clone, Debug)]
pub enum OutcomeKind {
    NoRule,
    Kept { rule: String, p: f64 },
    Propose { entries: Vec<Proposal> },
    Blocked { rule: String, why: String },
    Error(String),
}

#[derive(Clone, Debug)]
pub struct Proposal {
    pub rule: String,
    pub action: Action,
    pub p: f64,
    pub dest: Option<PathBuf>,
    pub tags: Vec<String>,
    pub uncertain: bool,
    pub note: Option<String>,
    pub alternatives: Vec<Alt>,
    pub create_dest: bool,
    pub on_conflict: OnConflict,
    pub mode: Option<Mode>,
}

/// Decide one item from its answers.
pub fn decide(it: &Item, rules: &[&CompiledRule], resp: Option<&Response>, guards: &Guards, now: i64) -> (OutcomeKind, Vec<(String, f64)>) {
    let empty = BTreeMap::new();
    let ans = resp.map(|r| &r.answers).unwrap_or(&empty);
    let mut ps: Vec<(String, f64)> = Vec::new();
    let probs: Vec<Option<f64>> = rules.iter().map(|r| rule_probability(r, ans)).collect();
    for (r, p) in rules.iter().zip(&probs) {
        if let Some(p) = p {
            ps.push((r.rule.id.clone(), *p));
        }
    }
    // keep rules veto first, regardless of order
    for (r, p) in rules.iter().zip(&probs) {
        if r.is_keep() && p.is_some_and(|p| p >= r.threshold) {
            return (OutcomeKind::Kept { rule: r.rule.id.clone(), p: p.unwrap() }, ps);
        }
    }
    let regenerable = ans.get("safety.regenerable").and_then(|a| a.p());
    let loss = match ans.get("safety.loss") {
        Some(Answer::Score { score, .. }) => Some(*score),
        _ => None,
    };
    let newest = it.dir.as_ref().map(|d| d.newest.max(it.mtime)).unwrap_or(it.mtime);
    let facts = Facts { mtime_max: Some(newest), flags: it.flags, regenerable: false, min_age: None };
    let mut proposals = Vec::new();
    // exclusive: first rule clearing its threshold wins
    let mut winner: Option<(usize, f64, bool)> = None;
    let mut best_uncertain: Option<(usize, f64)> = None;
    for (i, (r, p)) in rules.iter().zip(&probs).enumerate() {
        let (Some(_), Some(p)) = (r.action, *p) else { continue };
        if !r.exclusive() {
            continue;
        }
        if p >= r.threshold {
            winner = Some((i, p, false));
            break;
        }
        let ask = (r.threshold - 0.25).max(0.5);
        if p >= ask && best_uncertain.is_none_or(|b| p > b.1) {
            best_uncertain = Some((i, p));
        }
    }
    if winner.is_none() {
        winner = best_uncertain.map(|(i, p)| (i, p, true));
    }
    let mut blocked = None;
    if let Some((wi, p, uncertain)) = winner {
        let r = rules[wi];
        let mut action = r.action.unwrap();
        let mut uncertain = uncertain;
        let mut note = None;
        if r.removal() && !r.rule.when.is_empty() {
            if action == Action::Delete && !(regenerable.unwrap_or(0.0) >= 0.8 && loss.unwrap_or(3.0) <= 1.0) {
                action = Action::Trash;
                note = Some(format!(
                    "downgraded delete → trash (regenerable {:.2}, loss {:.1}/3)",
                    regenerable.unwrap_or(0.0),
                    loss.unwrap_or(3.0)
                ));
            }
            if loss.is_some_and(|l| l >= 2.5) {
                uncertain = true;
                note = Some(format!("judge rates losing it as costly (loss {:.1}/3)", loss.unwrap()));
            }
        }
        let mut dest = None;
        let mut skip = None;
        if matches!(action, Action::Move | Action::Copy) {
            match destination(r, it, ans) {
                Ok(d) => dest = Some(d),
                Err(e) => skip = Some(e),
            }
        } else if action == Action::Rename
            && let (Some(tpl), Some(parent)) = (&r.rule.name, it.path.parent())
        {
            dest = Some(parent.join(expand_template(tpl, it, None)));
        }
        if let Some(why) = skip {
            if uncertain || why == "none" {
                // no folder fits: nothing to propose
            } else {
                blocked = Some((r.rule.id.clone(), why));
            }
        } else if dest.as_ref().is_some_and(|d| d == &it.path) {
            // already where it belongs
        } else if let Err(b) = guards.check(&it.path, action, &facts, None) {
            blocked = Some((r.rule.id.clone(), b.to_string()));
        } else {
            let alternatives = rules
                .iter()
                .zip(&probs)
                .enumerate()
                .filter(|(i, (rr, pp))| *i != wi && rr.exclusive() && pp.is_some_and(|x| x >= (rr.threshold - 0.25).max(0.5)))
                .take(2)
                .map(|(_, (rr, pp))| Alt { rule: rr.rule.id.clone(), action: rr.action.unwrap(), dest: None, p: *pp })
                .collect();
            let to = r.rule.to.as_ref();
            proposals.push(Proposal {
                rule: r.rule.id.clone(),
                action,
                p,
                dest,
                tags: vec![],
                uncertain,
                note,
                alternatives,
                create_dest: to.is_some_and(|t| t.create_if_missing),
                on_conflict: r.rule.on_conflict.unwrap_or_default(),
                mode: r.rule.mode,
            });
        }
    }
    // tags accumulate
    let mut tags = Vec::new();
    let mut tag_p = 1.0f64;
    let mut tag_rules = Vec::new();
    for (r, p) in rules.iter().zip(&probs) {
        if r.action == Some(Action::Tag) && p.is_some_and(|p| p >= r.threshold) {
            tags.extend(r.rule.tags.iter().cloned());
            tag_p = tag_p.min(p.unwrap());
            tag_rules.push(r.rule.id.clone());
        }
    }
    if !tags.is_empty() && guards.check(&it.path, Action::Tag, &facts, None).is_ok() {
        tags.sort();
        tags.dedup();
        let existing = tidy_ops::meta::get_tags(&it.path);
        tags.retain(|t| !existing.contains(t));
        if !tags.is_empty() {
            proposals.push(Proposal {
                rule: tag_rules.join("+"),
                action: Action::Tag,
                p: tag_p,
                dest: None,
                tags,
                uncertain: false,
                note: None,
                alternatives: vec![],
                create_dest: false,
                on_conflict: OnConflict::Skip,
                mode: None,
            });
        }
    }
    let _ = now;
    if !proposals.is_empty() {
        return (OutcomeKind::Propose { entries: proposals }, ps);
    }
    if let Some((rule, why)) = blocked {
        return (OutcomeKind::Blocked { rule, why }, ps);
    }
    (OutcomeKind::NoRule, ps)
}

/// Destination directory for a move/copy, joined with the item name.
fn destination(r: &CompiledRule, it: &Item, ans: &BTreeMap<String, Answer>) -> Result<PathBuf, String> {
    let to = r.rule.to.as_ref().ok_or("no destination")?;
    let mut choice = None;
    if !r.dest_options.is_empty() {
        match ans.get(&format!("{}.dest", r.qid)) {
            Some(Answer::Choice { choice: c, probs, .. }) => {
                let conf = probs.iter().find(|x| &x.0 == c).map(|x| x.1).unwrap_or(0.0);
                if c == "none" || conf < 0.5 {
                    return Err("none".into());
                }
                choice = r.dest_options.iter().find(|(n, _)| n == c).map(|(_, p)| p.clone());
                if choice.is_none() {
                    return Err(format!("judge picked unknown folder `{c}`"));
                }
            }
            _ => return Err("no folder choice".into()),
        }
    }
    let dir = match (&to.path, choice) {
        (Some(tpl), ch) => PathBuf::from(expand_template(tpl, it, ch.as_deref())),
        (None, Some(ch)) => ch,
        (None, None) => return Err("no destination".into()),
    };
    let dir = tidy_scan::path::expand_tilde(&dir.to_string_lossy());
    Ok(dir.join(it.path.file_name().unwrap_or_default()))
}

/// Expand `{mtime:%Y}`, `{exif.date:%Y-%m}`, `{name}`, `{stem}`, `{ext}`,
/// `{kind}`, `{choice}` in a template.
pub fn expand_template(tpl: &str, it: &Item, choice: Option<&Path>) -> String {
    let mut out = String::new();
    let mut rest = tpl;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(j) = after.find('}') else {
            out.push_str(&rest[i..]);
            return out;
        };
        let field = &after[..j];
        let (key, fmt) = field.split_once(':').map(|(a, b)| (a, Some(b))).unwrap_or((field, None));
        let v = match key {
            "mtime" | "date" => strftime(it.mtime, fmt.unwrap_or("%Y-%m-%d")),
            "exif.date" | "taken" => strftime(crate::media::taken_at(&it.path).unwrap_or(it.mtime), fmt.unwrap_or("%Y-%m-%d")),
            "name" => it.name(),
            "stem" => it.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            "ext" => it.ext(),
            "kind" => it.kind().to_string(),
            "choice" => choice.map(|c| c.display().to_string()).unwrap_or_default(),
            other => format!("{{{other}}}"),
        };
        out.push_str(&v);
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    out
}

pub fn strftime(t: i64, f: &str) -> String {
    const MONTHS: [&str; 12] =
        ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    let ymd = items::ymd(t);
    let (y, m, d) = (&ymd[0..4], &ymd[5..7], &ymd[8..10]);
    let mi: usize = m.parse().unwrap_or(1);
    let mut out = String::new();
    let mut it = f.chars();
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('Y') => out.push_str(y),
            Some('y') => out.push_str(&y[2..]),
            Some('m') => out.push_str(m),
            Some('d') => out.push_str(d),
            Some('B') => out.push_str(MONTHS[mi - 1]),
            Some('b') => out.push_str(&MONTHS[mi - 1][..3]),
            Some('%') => out.push('%'),
            Some(o) => {
                out.push('%');
                out.push(o);
            }
            None => out.push('%'),
        }
    }
    out
}

/// Gather items for all rules: union of each rule's scope listing.
pub fn collect(rules: &[CompiledRule], now: i64) -> Vec<(Item, Vec<usize>)> {
    let mut by_scope: HashMap<PathBuf, (usize, bool)> = HashMap::new();
    for r in rules {
        let depth = r.filter.depth.unwrap_or(if r.filter.wants_dirs() { 1 } else { 64 });
        for s in &r.scopes {
            let e = by_scope.entry(s.clone()).or_insert((0, false));
            e.0 = e.0.max(depth);
            e.1 |= r.filter.wants_dirs();
        }
    }
    let mut all: Vec<Item> = Vec::new();
    let mut scopes: Vec<_> = by_scope.into_iter().collect();
    scopes.sort_by(|a, b| a.0.cmp(&b.0));
    for (s, (depth, dirs)) in scopes {
        items::list(&s, depth, dirs, &mut all);
    }
    let mut seen = HashSet::new();
    all.retain(|i| seen.insert(i.path.clone()));
    let mut out = Vec::new();
    for mut it in all {
        if it.is_dir && it.dir.is_none() {
            // summaries are needed for filters on size/age
            if rules.iter().any(|r| r.filter.wants_dirs() && r.scopes.iter().any(|s| it.path.starts_with(s))) {
                items::summarize_dir(&mut it);
            }
        }
        let applicable: Vec<usize> = rules.iter().enumerate().filter(|(_, r)| r.applies(&it, now)).map(|(i, _)| i).collect();
        if !applicable.is_empty() {
            out.push((it, applicable));
        }
    }
    out
}

pub struct L2Run {
    pub outcomes: Vec<Outcome>,
    pub requests: usize,
    pub cache_hits: usize,
    pub input_tokens: u64,
    pub elapsed_ms: u64,
}

/// Evaluate every rule against every applicable item.
pub fn run(
    policy: &Policy,
    rules: &[CompiledRule],
    judge: Option<&dyn Judge>,
    guards: &Guards,
    concurrency: usize,
    progress: &(dyn Fn(usize, usize) + Sync),
) -> Result<L2Run> {
    let now = tidy_scan::fmt::now_secs();
    let items = collect(rules, now);
    run_on(policy, rules, items, judge, guards, concurrency, progress)
}

/// Applicable rules for specific paths (watch mode: just-arrived files).
pub fn items_for_paths(rules: &[CompiledRule], paths: &[PathBuf], now: i64) -> Vec<(Item, Vec<usize>)> {
    use std::os::unix::fs::MetadataExt;
    let mut out = Vec::new();
    for p in paths {
        let Ok(md) = std::fs::symlink_metadata(p) else { continue };
        if !(md.is_file() || md.is_dir()) {
            continue;
        }
        let mut it = Item {
            path: p.clone(),
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
        let app: Vec<usize> = rules.iter().enumerate().filter(|(_, r)| r.applies(&it, now)).map(|(i, _)| i).collect();
        if !app.is_empty() {
            out.push((it, app));
        }
    }
    out
}

/// Evaluate rules for already-collected items.
pub fn run_on(
    policy: &Policy,
    rules: &[CompiledRule],
    items: Vec<(Item, Vec<usize>)>,
    judge: Option<&dyn Judge>,
    guards: &Guards,
    concurrency: usize,
    progress: &(dyn Fn(usize, usize) + Sync),
) -> Result<L2Run> {
    let started = std::time::Instant::now();
    let now = tidy_scan::fmt::now_secs();
    let media = judge.map(|j| Media::load(policy, j.caps()));
    // Items whose only applicable actionable rules are all blocked by guards
    // are not worth a paid call.
    let mut reqs = Vec::new();
    let mut req_of: Vec<Option<usize>> = Vec::new();
    for (it, app) in &items {
        let rs: Vec<&CompiledRule> = app.iter().map(|&i| &rules[i]).collect();
        let newest = it.dir.as_ref().map(|d| d.newest.max(it.mtime)).unwrap_or(it.mtime);
        let facts = Facts { mtime_max: Some(newest), flags: it.flags, regenerable: false, min_age: None };
        let actionable = rs.iter().any(|r| r.action.is_some_and(|a| guards.check(&it.path, a, &facts, Some(&[])).is_ok()));
        let needs_judge = rs.iter().any(|r| !r.deterministic());
        if actionable && needs_judge {
            req_of.push(Some(reqs.len()));
            reqs.push(build_request(policy, it, &rs, now, true, media.as_ref()));
        } else {
            req_of.push(None);
        }
    }
    let responses: Vec<Result<Response, tidy_judge::JudgeError>> = match judge {
        Some(j) if !reqs.is_empty() => tidy_judge::decide_many(j, &reqs, concurrency, progress),
        Some(_) => vec![],
        None if !reqs.is_empty() => anyhow::bail!("policy rules need a judge: add [judge.backends.*] to the policy"),
        None => vec![],
    };
    if let Some(Err(e @ (tidy_judge::JudgeError::Auth(_) | tidy_judge::JudgeError::Config(_)))) = responses.first() {
        anyhow::bail!("{e}");
    }
    let mut outcomes = Vec::new();
    let mut hits = 0;
    let mut tokens = 0;
    for ((it, app), ri) in items.into_iter().zip(req_of) {
        let rs: Vec<&CompiledRule> = app.iter().map(|&i| &rules[i]).collect();
        let resp = match ri {
            Some(i) => match &responses[i] {
                Ok(r) => {
                    if r.cached {
                        hits += 1;
                    } else {
                        tokens += r.input_tokens;
                    }
                    Some(r.clone())
                }
                Err(e) => {
                    outcomes.push(Outcome { item: it, kind: OutcomeKind::Error(e.to_string()), rule_p: vec![], response: None });
                    continue;
                }
            },
            None => None,
        };
        // Deterministic rules decide without answers; judged rules without a
        // response (guard-skipped) yield nothing.
        let (kind, ps) = decide(&it, &rs, resp.as_ref(), guards, now);
        outcomes.push(Outcome { item: it, kind, rule_p: ps, response: resp });
    }
    Ok(L2Run { outcomes, requests: reqs.len(), cache_hits: hits, input_tokens: tokens, elapsed_ms: started.elapsed().as_millis() as u64 })
}

/// Turn proposals into plan entries.
pub fn entries(run: &L2Run, mode: Mode, judge_id: Option<&str>, taken: &mut HashSet<String>) -> Vec<Entry> {
    let mut out = Vec::new();
    for o in &run.outcomes {
        let OutcomeKind::Propose { entries: props } = &o.kind else { continue };
        for p in props {
            let Ok(fp) = tidy_ops::fingerprint(&o.item.path, None, false) else { continue };
            let id = entry_id(&o.item.path, p.action, taken);
            taken.insert(id.clone());
            let auto = p.mode.unwrap_or(mode) == Mode::Autonomous && !p.uncertain;
            let evidence = json!({
                "judge": judge_id,
                "rules": o.rule_p.iter().map(|(r, p)| (r.clone(), json!((p * 1000.0).round() / 1000.0))).collect::<serde_json::Map<_, _>>(),
                "answers": o.response.as_ref().map(|r| &r.answers),
                "cached": o.response.as_ref().map(|r| r.cached),
                "uncertain": p.uncertain,
            });
            let mut reason = match p.action {
                Action::Tag => format!("tag {}", p.tags.join(", ")),
                _ => format!("policy rule {}", p.rule),
            };
            if p.uncertain {
                reason.push_str(" — uncertain, review carefully");
            }
            if let Some(n) = &p.note {
                reason.push_str(&format!(" ({n})"));
            }
            let (logical, alloc) = (o.item.logical, o.item.alloc);
            out.push(Entry {
                id,
                status: if auto { Status::Approved } else { Status::Pending },
                approved_by: auto.then(|| "policy".to_string()),
                action: p.action,
                path: o.item.path.clone(),
                dest: p.dest.clone(),
                create_dest: p.create_dest,
                on_conflict: p.on_conflict,
                tags: p.tags.clone(),
                bytes: Bytes { logical, alloc, private: alloc },
                fingerprint: fp,
                source: Source { layer: Layer::L2, rule: p.rule.clone(), p: Some((p.p * 1000.0).round() / 1000.0) },
                reason,
                undo: if p.action == Action::Delete { Undo::None } else { Undo::Restore },
                alternatives: p.alternatives.clone(),
                evidence: Some(evidence),
                outcome: None,
            });
        }
    }
    out
}

/// Predicate for an answered question (used by tests and explain).
pub fn answered_p(ans: &BTreeMap<String, Answer>, name: &str) -> Option<f64> {
    ans.get(name).and_then(|a| a.p())
}

pub fn question_kinds(req: &Request) -> Vec<(String, &'static str)> {
    req.questions
        .iter()
        .map(|q| {
            (
                q.name.clone(),
                match q.kind {
                    QKind::Predicate => "predicate",
                    QKind::Choice { .. } => "choice",
                    QKind::Score { .. } => "score",
                },
            )
        })
        .collect()
}
