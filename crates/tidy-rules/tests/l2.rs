//! L2 decision logic with a scripted judge: keep veto, first-match-wins,
//! tags accumulate, safety downgrade, uncertainty, destinations.

use std::fs;
use std::path::Path;
use tidy_judge::{Answer, Caps, Judge, JudgeError, QKind, Request, Response};
use tidy_plan::{Action, Mode};
use tidy_rules::calib::Calibration;
use tidy_rules::guards::{GuardConfig, Guards};
use tidy_rules::l2::{self, OutcomeKind};
use tidy_rules::Policy;

/// Answers predicates from (question-name substring, item-name substring) → p.
struct Script(Vec<(&'static str, &'static str, f64)>);

impl Judge for Script {
    fn id(&self) -> String {
        "script".into()
    }
    fn caps(&self) -> Caps {
        Caps { images: false, max_images: 0, max_questions: 64 }
    }
    fn decide(&self, req: &Request) -> Result<Response, JudgeError> {
        let name_line = req.text.lines().find(|l| l.starts_with("name: ")).unwrap_or("").to_string();
        let mut r = Response::default();
        for q in &req.questions {
            let a = match &q.kind {
                QKind::Predicate => {
                    let p = self.0.iter().find(|(qn, item, _)| q.name.contains(qn) && name_line.contains(item)).map(|x| x.2).unwrap_or(0.01);
                    Answer::Predicate { p }
                }
                QKind::Score { levels } => {
                    let costly = name_line.contains("wedding");
                    let mut probs = vec![0.0; levels.len()];
                    let idx = if costly { 3 } else { 0 };
                    probs[idx] = 1.0;
                    Answer::Score { score: idx as f64, probs, confidence: Some(0.9) }
                }
                QKind::Choice { options } => {
                    let pick = options.iter().find(|(v, _)| name_line.to_lowercase().contains(&v.to_lowercase())).map(|o| o.0.clone()).unwrap_or("none".into());
                    Answer::Choice { probs: vec![(pick.clone(), 0.9)], choice: pick, confidence: Some(0.9) }
                }
            };
            r.answers.insert(q.name.clone(), a);
        }
        Ok(r)
    }
}

fn old(p: &Path) {
    let t = std::time::SystemTime::now() - std::time::Duration::from_secs(90 * 86_400);
    let f = fs::File::options().write(true).open(p).unwrap();
    f.set_modified(t).unwrap();
}

#[test]
fn decisions_follow_policy_semantics() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path();
    let dl = home.join("Downloads");
    fs::create_dir_all(&dl).unwrap();
    fs::create_dir_all(home.join("Documents/Invoices")).unwrap();
    fs::create_dir_all(home.join("Documents/Recipes")).unwrap();
    let files = [
        ("Figma-124.3.dmg", 6_000_000usize),
        ("2024-tax-return.pdf", 200_000),
        ("dog-at-the-park.jpg", 90_000),
        ("wedding-dog.jpg", 90_000),
        ("invoices-acme.pdf", 50_000),
        ("unclear.dmg", 6_000_000),
        ("random.txt", 100),
    ];
    for (n, sz) in files {
        fs::write(dl.join(n), vec![1u8; sz]).unwrap();
        old(&dl.join(n));
    }
    let policy_text = format!(
        r#"
[judge]
default = "s"
[judge.backends.s]
kind = "mock"
model = "x"

[[rule]]
id = "protect.finance"
action = "keep"
scope = ["{dl}"]
when.any = ["tax return or invoice"]

[[rule]]
id = "installers"
action = "delete"
scope = ["{dl}"]
filter = {{ ext = ["dmg"] }}
when.any = ["installer that can be re-downloaded"]

[[rule]]
id = "photos.dogs"
action = "move"
scope = ["{dl}"]
filter = {{ kind = "image" }}
when.any = ["a picture of a dog"]
to = {{ path = "{home}/Pictures/Dogs/{{mtime:%Y}}", create_if_missing = true }}

[[rule]]
id = "photos.any"
action = "tag"
scope = ["{dl}"]
filter = {{ kind = "image" }}
tags = ["Photo"]
when.any = ["a photograph"]

[[rule]]
id = "file.docs"
action = "move"
scope = ["{dl}"]
filter = {{ ext = ["pdf"] }}
when.any = ["a business document"]
to = {{ pick_from = "{home}/Documents/*" }}

[[rule]]
id = "trash.everything-old"
action = "trash"
scope = ["{dl}"]
filter = {{ ext = ["txt"], older_than = "30d" }}
"#,
        dl = dl.display(),
        home = home.display()
    );
    let policy: Policy = toml::from_str(&policy_text).unwrap();
    policy.validate().unwrap();
    let judge = Script(vec![
        ("protect", "tax-return", 0.97),
        ("protect", "invoices", 0.10),
        ("installers", "Figma", 0.96),
        ("regenerable", "Figma", 0.95),
        ("installers", "unclear", 0.62),
        ("regenerable", "unclear", 0.4),
        ("photos.dogs", "dog", 0.93),
        ("photos.any", "dog", 0.99),
        ("file.docs", "invoices", 0.9),
    ]);
    let guards = Guards::new(GuardConfig { min_age: 30 * 86_400, ..Default::default() });
    let rules = l2::compile(&policy, &[home.to_path_buf()], Some("script"), &Calibration::default());
    let run = l2::run(&policy, &rules, Some(&judge), &guards, 4, &|_, _| {}).unwrap();
    let get = |n: &str| run.outcomes.iter().find(|o| o.item.name() == n).map(|o| &o.kind).unwrap_or_else(|| panic!("no outcome for {n}"));

    // keep veto wins over everything
    assert!(matches!(get("2024-tax-return.pdf"), OutcomeKind::Kept { rule, .. } if rule == "protect.finance"));
    // installer: delete stays delete (regenerable, low loss)
    match get("Figma-124.3.dmg") {
        OutcomeKind::Propose { entries } => {
            assert_eq!(entries[0].action, Action::Delete);
            assert!(!entries[0].uncertain);
        }
        o => panic!("{o:?}"),
    }
    // unclear installer: uncertain band, downgraded to trash
    match get("unclear.dmg") {
        OutcomeKind::Propose { entries } => {
            assert_eq!(entries[0].action, Action::Trash);
            assert!(entries[0].uncertain);
            assert!(entries[0].note.as_ref().unwrap().contains("downgraded"));
        }
        o => panic!("{o:?}"),
    }
    // dog photo: move to templated folder + tag accumulates
    match get("dog-at-the-park.jpg") {
        OutcomeKind::Propose { entries } => {
            let mv = entries.iter().find(|e| e.action == Action::Move).unwrap();
            let year = &tidy_rules::items::ymd(tidy_scan::fmt::now_secs() - 90 * 86_400)[..4];
            assert_eq!(mv.dest.as_ref().unwrap(), &home.join(format!("Pictures/Dogs/{year}/dog-at-the-park.jpg")));
            assert!(mv.create_dest);
            assert!(entries.iter().any(|e| e.action == Action::Tag && e.tags == vec!["Photo".to_string()]));
        }
        o => panic!("{o:?}"),
    }
    // pick_from: judge picks an existing folder by name
    match get("invoices-acme.pdf") {
        OutcomeKind::Propose { entries } => {
            assert_eq!(entries[0].dest.as_ref().unwrap(), &home.join("Documents/Invoices/invoices-acme.pdf"));
        }
        o => panic!("{o:?}"),
    }
    // deterministic rule (no `when`): no judge needed
    match get("random.txt") {
        OutcomeKind::Propose { entries } => assert_eq!(entries[0].action, Action::Trash),
        o => panic!("{o:?}"),
    }
    // entries: uncertain ones stay pending even in autonomous mode
    let mut taken = Default::default();
    let es = l2::entries(&run, Mode::Autonomous, Some("script"), &mut taken);
    let unclear = es.iter().find(|e| e.path.ends_with("unclear.dmg")).unwrap();
    assert_eq!(unclear.status, tidy_plan::Status::Pending);
    let figma = es.iter().find(|e| e.path.ends_with("Figma-124.3.dmg")).unwrap();
    assert_eq!(figma.status, tidy_plan::Status::Approved);
    assert!(figma.evidence.as_ref().unwrap()["rules"]["installers"].as_f64().unwrap() > 0.9);
}

#[test]
fn templates() {
    let it = tidy_rules::items::Item {
        path: "/x/IMG_0001.HEIC".into(),
        is_dir: false,
        logical: 1,
        alloc: 1,
        mtime: tidy_rules::media::parse_exif_datetime("2021:03:04 05:06:07").unwrap(),
        flags: 0,
        depth: 1,
        dir: None,
    };
    assert_eq!(l2::expand_template("~/Pictures/{mtime:%Y}/{mtime:%m - %B}", &it, None), "~/Pictures/2021/03 - March");
    assert_eq!(l2::expand_template("{stem}.{ext}", &it, None), "IMG_0001.heic");
}
