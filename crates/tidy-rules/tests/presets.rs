//! Built-in presets end to end through L2, with a scripted judge. Its own test
//! binary because it points $HOME at a sandbox (preset scopes use `~`).

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use tidy_judge::{Answer, Caps, Judge, JudgeError, QKind, Request, Response};
use tidy_plan::Action;
use tidy_rules::Policy;
use tidy_rules::calib::Calibration;
use tidy_rules::guards::{GuardConfig, Guards};
use tidy_rules::l2::{self, OutcomeKind};

/// (question-name substring, item-name substring) → p; everything else 0.01.
struct Script(Vec<(&'static str, &'static str, f64)>);

impl Judge for Script {
    fn id(&self) -> String {
        "script".into()
    }
    fn caps(&self) -> Caps {
        Caps { images: false, max_images: 0, max_questions: 64 }
    }
    fn decide(&self, req: &Request) -> Result<Response, JudgeError> {
        let name = req.text.lines().find(|l| l.starts_with("name: ")).unwrap_or("").to_string();
        let mut r = Response::default();
        for q in &req.questions {
            let a = match &q.kind {
                QKind::Predicate => Answer::Predicate {
                    p: self.0.iter().find(|(qn, it, _)| q.name.contains(qn) && name.contains(it)).map(|x| x.2).unwrap_or(0.01),
                },
                QKind::Score { levels } => {
                    let mut probs = vec![0.0; levels.len()];
                    probs[0] = 1.0;
                    Answer::Score { score: 0.0, probs, confidence: Some(0.9) }
                }
                QKind::Choice { .. } => Answer::Choice { probs: vec![], choice: "none".into(), confidence: None },
            };
            r.answers.insert(q.name.clone(), a);
        }
        Ok(r)
    }
}

fn age(p: &Path, days: u64) {
    let t = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
    fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
}

/// Minimal stored zip.
fn write_zip(path: &Path, files: &[(&str, &[u8])]) {
    let (mut out, mut cd) = (Vec::new(), Vec::new());
    for (name, data) in files {
        let crc = crc32fast::hash(data);
        let off = out.len() as u32;
        let len = (data.len() as u32).to_le_bytes();
        out.extend_from_slice(b"PK\x03\x04\x14\0\0\0\0\0\0\0\0\0");
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&len);
        out.extend_from_slice(&len);
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        cd.extend_from_slice(b"PK\x01\x02\x14\0\x14\0\0\0\0\0\0\0\0\0");
        cd.extend_from_slice(&crc.to_le_bytes());
        cd.extend_from_slice(&len);
        cd.extend_from_slice(&len);
        cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
        cd.extend_from_slice(&[0; 12]);
        cd.extend_from_slice(&off.to_le_bytes());
        cd.extend_from_slice(name.as_bytes());
    }
    let cd_off = out.len() as u32;
    out.extend_from_slice(&cd);
    out.extend_from_slice(b"PK\x05\x06\0\0\0\0");
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(cd.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_off.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    fs::File::create(path).unwrap().write_all(&out).unwrap();
}

fn verdicts(run: &l2::L2Run) -> HashMap<String, String> {
    run.outcomes
        .iter()
        .map(|o| {
            let name = o.item.path.file_name().unwrap().to_string_lossy().into_owned();
            let v = match &o.kind {
                OutcomeKind::Kept { rule, p } => format!("keep:{rule}:{}", if p.is_some() { "answered" } else { "unanswered" }),
                OutcomeKind::Propose { entries } => entries
                    .iter()
                    .map(|e| format!("{}:{}{}", e.action.as_str(), e.rule, if e.uncertain { ":uncertain" } else { "" }))
                    .collect::<Vec<_>>()
                    .join(","),
                OutcomeKind::Blocked { rule, why } => format!("blocked:{rule}:{why}"),
                OutcomeKind::NoRule => "none".into(),
                OutcomeKind::Error(e) => format!("error:{e}"),
            };
            (name, v)
        })
        .collect()
}

#[test]
fn presets_end_to_end() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().canonicalize().unwrap();
    // SAFETY: this test binary has a single test, so nothing reads the environment concurrently.
    unsafe { std::env::set_var("HOME", &home) };
    let (dl, desk) = (home.join("Downloads"), home.join("Desktop"));
    fs::create_dir_all(&dl).unwrap();
    fs::create_dir_all(&desk).unwrap();

    let shot = desk.join("Screenshot 2026-05-02 at 10.11.12.png");
    fs::write(&shot, b"png").unwrap();
    age(&shot, 3);
    for (n, d) in [("Figma-124.3.dmg", 90), ("holiday-photos.zip", 90)] {
        fs::write(dl.join(n), vec![0u8; 6_000_000]).unwrap();
        age(&dl.join(n), d);
    }
    fs::write(dl.join("export-2023-03-01.csv"), b"a,b\n").unwrap();
    age(&dl.join("export-2023-03-01.csv"), 500);
    // project.zip, already extracted next to itself
    write_zip(&dl.join("project.zip"), &[("project/a.txt", b"alpha"), ("project/b.txt", b"beta")]);
    fs::create_dir_all(dl.join("project")).unwrap();
    fs::write(dl.join("project/a.txt"), b"alpha").unwrap();
    fs::write(dl.join("project/b.txt"), b"beta").unwrap();
    age(&dl.join("project.zip"), 40);

    let mut policy: Policy =
        toml::from_str(r#"presets = ["protect.personal", "installers", "screenshots", "stale-downloads", "archive-leftovers"]"#).unwrap();
    policy.expand_presets().unwrap();
    policy.validate().unwrap();
    let guards = Guards::new(GuardConfig { min_age: 0, settle: 0, ..Default::default() });
    let rules = l2::compile(&policy, std::slice::from_ref(&home), Some("script"), &Calibration::default());
    let judge = Script(vec![
        ("protect.personal", "holiday", 0.9),
        ("installers", "Figma", 0.95),
        ("installers", "holiday", 0.9),
        ("stale-downloads", "export", 0.9),
    ]);
    let run = l2::run(&policy, &rules, Some(&judge), &guards, 2, &|_, _| {}).unwrap();
    let v = verdicts(&run);
    assert_eq!(v["Screenshot 2026-05-02 at 10.11.12.png"], "move:screenshots");
    assert_eq!(v["Figma-124.3.dmg"], "trash:installers");
    assert_eq!(v["holiday-photos.zip"], "keep:protect.personal:answered", "removal-only keep vetoes the installer rule");
    assert_eq!(v["export-2023-03-01.csv"], "quarantine:stale-downloads:uncertain", "always_review");
    assert_eq!(v["project.zip"], "trash:archive-leftovers");
    // The screenshot is organising-only and project.zip a lossless removal:
    // protect.personal is pruned for both and no call is made for them.
    assert_eq!(run.requests, 3, "{v:?}");
    let dest = run
        .outcomes
        .iter()
        .find_map(|o| match &o.kind {
            OutcomeKind::Propose { entries } if entries[0].action == Action::Move => entries[0].dest.clone(),
            _ => None,
        })
        .unwrap();
    let month = dest.parent().unwrap().file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(dest.parent().unwrap().parent().unwrap(), home.join("Pictures/Screenshots"));
    assert!(month.len() == 7 && month.as_bytes()[4] == b'-', "{month}");
    assert_eq!(dest.file_name().unwrap(), shot.file_name().unwrap());

    // A rule check gates the rule: once the extracted copy differs, the zip stays.
    fs::write(dl.join("project/b.txt"), b"BETA").unwrap();
    let run = l2::run(&policy, &rules, Some(&judge), &guards, 2, &|_, _| {}).unwrap();
    assert!(!verdicts(&run).contains_key("project.zip"));
    fs::write(dl.join("project/b.txt"), b"beta").unwrap();

    // Without a judge: no error; deterministic organising still runs, judged
    // rules are skipped, and the unanswered removal-only keep rule protects.
    let rules = l2::compile(&policy, std::slice::from_ref(&home), None, &Calibration::default());
    let run = l2::run(&policy, &rules, None, &guards, 2, &|_, _| {}).unwrap();
    let v = verdicts(&run);
    assert_eq!(run.requests, 0);
    assert_eq!(run.unjudged, 3);
    assert_eq!(v["Screenshot 2026-05-02 at 10.11.12.png"], "move:screenshots");
    assert_eq!(v["project.zip"], "trash:archive-leftovers", "lossless removals aren't vetoed by removal-only keep rules");
    assert_eq!(v["Figma-124.3.dmg"], "keep:protect.personal:unanswered");

    // Presets alone, without protect.personal and without a judge: the
    // deterministic removal goes ahead, judged rules stay silent.
    let mut policy: Policy = toml::from_str(r#"presets = ["installers", "archive-leftovers"]"#).unwrap();
    policy.expand_presets().unwrap();
    let rules = l2::compile(&policy, std::slice::from_ref(&home), None, &Calibration::default());
    let run = l2::run(&policy, &rules, None, &guards, 2, &|_, _| {}).unwrap();
    let v = verdicts(&run);
    assert_eq!(v["project.zip"], "trash:archive-leftovers");
    assert_eq!(v.get("Figma-124.3.dmg").map(String::as_str).unwrap_or("none"), "none");
}
