//! Built-in rule packs. All are off until listed in `presets = [...]`.
//! Each ships with `policy test` cases that run whenever it is enabled.

use crate::policy::Rule;
use anyhow::Result;

pub struct Preset {
    pub name: &'static str,
    pub description: &'static str,
    /// Whether any rule needs a decision model.
    pub needs_judge: bool,
    rules: &'static str,
    /// `[[case]]` entries for `tdy policy test` (TOML).
    pub tests: &'static str,
}

#[derive(serde::Deserialize)]
struct Rules {
    #[serde(rename = "rule")]
    rules: Vec<Rule>,
}

impl Preset {
    pub fn rules_toml(&self) -> &'static str {
        self.rules
    }
    pub fn rules(&self) -> Result<Vec<Rule>> {
        Ok(toml::from_str::<Rules>(self.rules)?.rules)
    }
}

pub fn get(name: &str) -> Option<&'static Preset> {
    ALL.iter().find(|p| p.name == name)
}

pub const ALL: &[Preset] = &[
    Preset {
        name: "protect.personal",
        description: "Never remove (delete/trash/quarantine/archive) finance, legal and identity papers, medical records, personal photos and videos, or your own work. Organising rules can still move and tag them, and removals proven lossless (archive-leftovers) still go ahead.",
        needs_judge: true,
        rules: r#"
[[rule]]
id = "protect.personal"
action = "keep"
protects = "removal"
when.any = [
  "Finance, tax, legal or employment paperwork belonging to the owner: tax returns, invoices, receipts, contracts, payslips, bank or pension statements.",
  "Identity or official documents: passport or ID scans, visas, certificates, insurance policies, medical or health records.",
  "Personal photos or videos the owner or their family took: people, family, pets, homes, trips or events — not screenshots, memes or stock images.",
  "Original work the owner made themselves — their writing, artwork, design files, source code, research, slides or recordings — not an app, an installer or data exported from a service.",
]
"#,
        tests: r#"
[[case]]
name = "protect.personal: payslip is protected"
item = { name = "payslip_march_2026.pdf", folder = "~/Downloads", size = "120KB", age = "40d" }
expect = "keep"

[[case]]
name = "protect.personal: family video is protected"
item = { name = "family-reunion-2024.mov", folder = "~/Downloads", size = "900MB", age = "400d" }
expect = "keep"
"#,
    },
    Preset {
        name: "installers",
        description: "Trash application installers and disk images in ~/Downloads older than 30 days (re-downloadable).",
        needs_judge: true,
        rules: r#"
[[rule]]
id = "installers"
action = "trash"
scope = ["~/Downloads"]
filter = { ext = ["dmg", "pkg", "mpkg", "iso", "msi", "exe", "deb", "rpm", "appimage", "xip", "zip"], older_than = "30d", min_size = "5MB", depth = 2 }
when.any = ["An application installer or disk image (such as a .dmg, .pkg, .exe or a zipped app) that can simply be downloaded again — not an archive of personal files, documents or photos."]
"#,
        tests: r#"
[[case]]
name = "installers: old Figma installer goes"
item = { name = "Figma-124.3.dmg", folder = "~/Downloads", size = "410MB", age = "160d", where_from = "https://desktop.figma.com/mac/Figma-124.3.dmg" }
expect = "installers"

[[case]]
name = "installers: personal photo archive stays"
item = { name = "holiday-photos-2019.zip", folder = "~/Downloads", size = "800MB", age = "300d" }
expect = ["none", "keep"]
"#,
    },
    Preset {
        name: "screenshots",
        description: "Move screenshots from ~/Desktop and ~/Downloads into ~/Pictures/Screenshots/<year>-<month>. Deterministic (no model).",
        needs_judge: false,
        rules: r#"
[[rule]]
id = "screenshots"
action = "move"
scope = ["~/Desktop", "~/Downloads"]
filter = { kind = "image", name = ["Screenshot *", "Screen Shot *", "Screenshot_*", "Screenshot from *", "CleanShot *"], depth = 1 }
to = { path = "~/Pictures/Screenshots/{mtime:%Y-%m}", create_if_missing = true }
on_conflict = "dedupe-else-suffix"
"#,
        tests: r#"
[[case]]
name = "screenshots: macOS screenshot on the Desktop is filed"
item = { name = "Screenshot 2026-05-02 at 10.11.12.png", folder = "~/Desktop", size = "400KB", age = "3d" }
expect = "screenshots"
"#,
    },
    Preset {
        name: "stale-downloads",
        description: "Quarantine things in ~/Downloads untouched for a year that look like one-off downloads. Always marked for careful review, never auto-approved.",
        needs_judge: true,
        rules: r#"
[[rule]]
id = "stale-downloads"
action = "quarantine"
scope = ["~/Downloads"]
filter = { kind = "any", older_than = "365d", depth = 1 }
always_review = true
when.any = ["Something the owner probably will not need again — a one-off download, a temporary export, or an old copy of something they can get again — rather than an important document, a personal photo or an ongoing project."]
"#,
        tests: r#"
[[case]]
name = "stale-downloads: old one-off export is proposed"
item = { name = "export-2023-03-01.csv", folder = "~/Downloads", size = "2MB", age = "500d" }
expect = "stale-downloads"

[[case]]
name = "stale-downloads: old tax return is not"
item = { name = "2023-tax-return.pdf", folder = "~/Downloads", size = "2MB", age = "500d" }
expect = ["none", "keep"]
"#,
    },
    Preset {
        name: "archive-leftovers",
        description: "Trash a .zip in ~/Downloads or ~/Desktop when the folder extracted next to it holds every file with identical content (size and CRC-32 checked). Deterministic (no model).",
        needs_judge: false,
        rules: r#"
[[rule]]
id = "archive-leftovers"
action = "trash"
scope = ["~/Downloads", "~/Desktop"]
filter = { ext = ["zip"], depth = 2 }
check = "extracted-sibling"
"#,
        tests: "",
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn presets_parse_and_validate() {
        for p in ALL {
            let rules = p.rules().unwrap_or_else(|e| panic!("{}: {e}", p.name));
            assert!(!rules.is_empty());
            let needs = rules.iter().any(|r| !r.when.is_empty());
            assert_eq!(needs, p.needs_judge, "{}", p.name);
        }
        // all presets together form a valid policy
        let mut pol = crate::policy::Policy { presets: ALL.iter().map(|p| p.name.to_string()).collect(), ..Default::default() };
        pol.expand_presets().unwrap();
        pol.validate().unwrap();
        assert_eq!(pol.rules.len(), ALL.len());
        // a user rule with a preset's id overrides it
        let mut pol: crate::policy::Policy = toml::from_str(
            r#"
presets = ["installers", "screenshots"]
[[rule]]
id = "installers"
action = "quarantine"
scope = ["~/Downloads"]
filter = { ext = ["dmg"] }
"#,
        )
        .unwrap();
        pol.expand_presets().unwrap();
        assert_eq!(pol.rules.len(), 2);
        assert_eq!(pol.rules[0].action, "quarantine");
        assert!(pol.rules[0].preset.is_none());
        assert_eq!(pol.rules[1].preset.as_deref(), Some("screenshots"));
        // unknown preset names fail loudly
        let mut bad = crate::policy::Policy { presets: vec!["nope".into()], ..Default::default() };
        assert!(bad.expand_presets().unwrap_err().to_string().contains("available"));
    }
}
