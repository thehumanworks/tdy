//! The policy file (`~/.config/tidy/policy.toml`): mode, guard knobs,
//! detector settings, judge backends, subjects and natural-language rules.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tidy_plan::{Mode, OnConflict};
use tidy_scan::fmt::{parse_bytes, parse_duration};
use tidy_scan::path::expand_tilde;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default)]
    pub mode: Mode,
    /// Guard: minimum age of the newest file under a path before any
    /// destructive action ("7d").
    #[serde(default)]
    pub min_age: Option<String>,
    #[serde(default)]
    pub allow_cloud: bool,
    /// Paths that are never touched.
    #[serde(default)]
    pub protect: Vec<String>,
    #[serde(default)]
    pub detectors: Detectors,
    #[serde(default)]
    pub judge: JudgeConfig,
    #[serde(default)]
    pub subjects: BTreeMap<String, Subject>,
    #[serde(default, rename = "rule")]
    pub rules: Vec<Rule>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Detectors {
    #[serde(default)]
    pub min_size: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub disabled: Vec<String>,
    /// Turn L1 detectors off entirely.
    #[serde(default)]
    pub off: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeConfig {
    /// Backend name used by rules that don't name one.
    #[serde(default)]
    pub default: Option<String>,
    /// Cheap → expensive. Items whose answers fall in the uncertain band of
    /// a stage are re-asked at the next stage.
    #[serde(default)]
    pub cascade: Vec<String>,
    /// Probability band treated as "uncertain" for the cascade.
    #[serde(default)]
    pub uncertain: Option<[f64; 2]>,
    #[serde(default)]
    pub concurrency: Option<usize>,
    #[serde(default)]
    pub backends: BTreeMap<String, Backend>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    /// `openai-decisions` or `systemone` (TypeSafe Jev, Cloudflare Clef).
    pub kind: String,
    pub model: String,
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Environment variable holding the API key/token.
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub images: Option<bool>,
    #[serde(default)]
    pub max_questions: Option<usize>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subject {
    pub refs: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    /// keep | delete | trash | quarantine | move | copy | tag | rename | archive | compress
    pub action: String,
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default)]
    pub filter: Filter,
    #[serde(default)]
    pub when: When,
    #[serde(default)]
    pub to: Option<To>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub on_conflict: Option<OnConflict>,
    /// Probability needed for the rule to fire (calibrated by `tidy calibrate`).
    #[serde(default)]
    pub threshold: Option<f64>,
    #[serde(default)]
    pub mode: Option<Mode>,
    #[serde(default)]
    pub judge: Option<String>,
    /// Send file content (text head / image thumbnail) to the judge.
    #[serde(default)]
    pub content: Option<bool>,
    /// Template for `rename` (e.g. "{exif.date:%Y-%m-%d} {stem}.{ext}").
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    /// file | dir | image | video | audio | document | archive | installer | code
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub ext: Vec<String>,
    /// Glob on the file name (`*`, `?`).
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub min_size: Option<String>,
    #[serde(default)]
    pub max_size: Option<String>,
    #[serde(default)]
    pub older_than: Option<String>,
    #[serde(default)]
    pub newer_than: Option<String>,
    /// Only consider entries this many levels below each scope root (1 = direct children).
    #[serde(default)]
    pub depth: Option<usize>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct When {
    #[serde(default)]
    pub any: Vec<String>,
    #[serde(default)]
    pub all: Vec<String>,
    #[serde(default)]
    pub none: Vec<String>,
}

impl When {
    pub fn is_empty(&self) -> bool {
        self.any.is_empty() && self.all.is_empty() && self.none.is_empty()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct To {
    /// Destination template: `~/photos/dogs/{exif.date:%Y}`.
    #[serde(default)]
    pub path: Option<String>,
    /// Let the judge pick one existing folder matching this glob.
    #[serde(default)]
    pub pick_from: Option<String>,
    #[serde(default)]
    pub create_if_missing: bool,
    /// What to do when no folder fits: "ask" (default) or "skip".
    #[serde(default)]
    pub fallback: Option<String>,
}

pub fn default_path() -> PathBuf {
    if let Some(p) = std::env::var_os("TIDY_POLICY") {
        return PathBuf::from(p);
    }
    tidy_ops::dirs::config_dir().join("policy.toml")
}

impl Policy {
    pub fn load(path: &Path) -> Result<(Policy, String)> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading policy {}", path.display()))?;
        let p: Policy = toml::from_str(&text).with_context(|| format!("parsing policy {}", path.display()))?;
        p.validate()?;
        let hash = blake3::hash(text.as_bytes()).to_hex()[..12].to_string();
        Ok((p, hash))
    }

    /// The default policy when no file exists: detectors only.
    pub fn load_or_default(path: Option<&Path>) -> Result<(Policy, Option<String>, Option<PathBuf>)> {
        let p = path.map(Path::to_path_buf).unwrap_or_else(default_path);
        if p.exists() {
            let (pol, h) = Policy::load(&p)?;
            Ok((pol, Some(h), Some(p)))
        } else if path.is_some() {
            bail!("policy {} not found", p.display())
        } else {
            Ok((Policy::default(), None, None))
        }
    }

    pub fn validate(&self) -> Result<()> {
        let mut ids = std::collections::HashSet::new();
        for r in &self.rules {
            if !ids.insert(&r.id) {
                bail!("duplicate rule id `{}`", r.id);
            }
            let a = r.action.as_str();
            if !["keep", "delete", "trash", "quarantine", "move", "copy", "tag", "rename", "archive", "compress"].contains(&a) {
                bail!("rule `{}`: unknown action `{a}`", r.id);
            }
            if matches!(a, "move" | "copy") && r.to.as_ref().is_none_or(|t| t.path.is_none() && t.pick_from.is_none()) {
                bail!("rule `{}`: `{a}` needs `to = {{ path = … }}` or `to = {{ pick_from = … }}`", r.id);
            }
            if a == "rename" && r.name.is_none() {
                bail!("rule `{}`: `rename` needs a `name` template", r.id);
            }
            if a == "tag" && r.tags.is_empty() {
                bail!("rule `{}`: `tag` needs `tags = [...]`", r.id);
            }
            if let Some(t) = r.threshold {
                if !(0.0..=1.0).contains(&t) {
                    bail!("rule `{}`: threshold must be within 0..1", r.id);
                }
            }
            for s in [&r.filter.min_size, &r.filter.max_size].into_iter().flatten() {
                parse_bytes(s).with_context(|| format!("rule `{}`: bad size `{s}`", r.id))?;
            }
            for s in [&r.filter.older_than, &r.filter.newer_than].into_iter().flatten() {
                parse_duration(s).with_context(|| format!("rule `{}`: bad duration `{s}`", r.id))?;
            }
            for text in r.when.any.iter().chain(&r.when.all).chain(&r.when.none) {
                for name in subject_refs(text) {
                    if !self.subjects.contains_key(&name) {
                        bail!("rule `{}`: unknown subject `{{{name}}}`", r.id);
                    }
                }
            }
            if let Some(j) = &r.judge {
                if !self.judge.backends.contains_key(j) {
                    bail!("rule `{}`: unknown judge `{j}`", r.id);
                }
            }
        }
        for j in self.judge.cascade.iter().chain(self.judge.default.iter()) {
            if !self.judge.backends.contains_key(j) {
                bail!("judge `{j}` is not defined under [judge.backends]");
            }
        }
        for (name, b) in &self.judge.backends {
            if !["openai-decisions", "systemone", "mock"].contains(&b.kind.as_str()) {
                bail!("judge backend `{name}`: unknown kind `{}` (openai-decisions | systemone)", b.kind);
            }
        }
        if let Some(m) = &self.min_age {
            parse_duration(m).with_context(|| format!("bad min_age `{m}`"))?;
        }
        Ok(())
    }

    pub fn guard_config(&self) -> crate::guards::GuardConfig {
        let mut g = crate::guards::GuardConfig { allow_cloud: self.allow_cloud, ..Default::default() };
        if let Some(m) = self.min_age.as_deref().and_then(parse_duration) {
            g.min_age = m;
        }
        g.protect = self.protect.iter().map(|p| expand_tilde(p)).collect();
        for r in &self.rules {
            if let Some(to) = &r.to {
                if let Some(p) = &to.path {
                    g.destinations.push(template_root(p));
                }
                if let Some(p) = &to.pick_from {
                    g.destinations.push(template_root(p));
                }
            }
        }
        g
    }

    pub fn detect_config(&self) -> crate::detect::DetectConfig {
        let mut d = crate::detect::DetectConfig { disabled: self.detectors.disabled.clone(), ..Default::default() };
        if let Some(s) = self.detectors.min_size.as_deref().and_then(parse_bytes) {
            d.min_size = s;
        }
        d.action = self.detectors.action.as_deref().and_then(|a| a.parse().ok());
        d
    }
}

/// `{name}` references to subjects in a criterion (excludes `{exif.…}`-style
/// template fields, which never appear in criteria).
pub fn subject_refs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find('{') {
        let after = &rest[i + 1..];
        if let Some(j) = after.find('}') {
            let name = &after[..j];
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                out.push(name.to_string());
            }
            rest = &after[j + 1..];
        } else {
            break;
        }
    }
    out
}

/// The fixed (non-template) prefix of a destination template.
pub fn template_root(t: &str) -> PathBuf {
    let cut = t.find(['{', '*', '?']).unwrap_or(t.len());
    let fixed = &t[..cut];
    let fixed = fixed.rsplit_once('/').map(|(a, _)| a).unwrap_or(fixed);
    let p = expand_tilde(if cut == t.len() { t } else { fixed });
    if p.as_os_str().is_empty() { PathBuf::from("/") } else { p }
}

pub const EXAMPLE: &str = r#"# Tidy policy. Rules are checked top to bottom; for moves/deletes the first
# rule whose answer clears its threshold wins. Tags accumulate.
mode = "manual"          # "autonomous" writes plan entries pre-approved
min_age = "7d"           # never touch anything modified more recently
protect = ["~/Documents/Taxes"]

[detectors]              # deterministic build-artifact/cache cleanup (L1)
min_size = "10MB"
action = "delete"        # or "trash"
disabled = []

[judge]
default = "luna"

[judge.backends.luna]
kind = "openai-decisions"
model = "gpt-6-luna"
api_key_env = "OPENAI_API_KEY"

# [judge.backends.clef]
# kind = "systemone"
# endpoint = "https://api.cloudflare.com/client/v4/accounts/<ACCOUNT_ID>/ai/run/@cf/cloudflare/clef"
# model = "@cf/cloudflare/clef"
# api_key_env = "CLOUDFLARE_API_TOKEN"

# [subjects]
# me = { refs = ["~/.config/tidy/subjects/me-1.jpg"] }

[[rule]]
id = "protect.finance"
action = "keep"
scope = ["~/Downloads", "~/Desktop"]
when.any = ["Tax returns, invoices, contracts, payslips, bank statements or other finance/legal documents."]

[[rule]]
id = "installers"
action = "trash"
scope = ["~/Downloads"]
filter = { ext = ["dmg", "pkg", "iso", "msi", "exe", "zip"], older_than = "30d", min_size = "5MB" }
when.any = ["An application installer or disk image (such as a .dmg, .pkg, .exe or a zipped app) that can simply be downloaded again — not an archive of personal files."]

[[rule]]
id = "photos.dogs"
action = "move"
scope = ["~/Downloads", "~/Desktop"]
filter = { kind = "image", min_size = "50KB" }
when.any = ["a picture of a dog", "a picture of a walk in the park"]
to = { path = "~/Pictures/Dogs/{mtime:%Y}", create_if_missing = true }
on_conflict = "dedupe-else-suffix"
"#;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn example_parses_and_validates() {
        let p: Policy = toml::from_str(EXAMPLE).unwrap();
        p.validate().unwrap();
        assert_eq!(p.rules.len(), 3);
        assert_eq!(p.rules[2].to.as_ref().unwrap().create_if_missing, true);
        let g = p.guard_config();
        assert!(g.destinations.iter().any(|d| d.ends_with("Pictures/Dogs")));
        assert_eq!(subject_refs("picture of {me} next to a dog"), vec!["me"]);
    }
    #[test]
    fn rejects_bad_rules() {
        let bad = "[[rule]]\nid='x'\naction='move'\nwhen.any=['a']\n";
        let p: Policy = toml::from_str(bad).unwrap();
        assert!(p.validate().is_err());
        let bad2 = "[[rule]]\nid='x'\naction='tag'\ntags=['a']\nwhen.any=['{ghost} dog']\n";
        let p: Policy = toml::from_str(bad2).unwrap();
        assert!(p.validate().unwrap_err().to_string().contains("ghost"));
    }
}
