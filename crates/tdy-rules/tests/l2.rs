//! L2 decision logic with a scripted judge: keep veto, first-match-wins,
//! tags accumulate, safety downgrade, uncertainty, destinations.

use std::fs;
use std::path::Path;
use tdy_judge::{Answer, Caps, Judge, JudgeError, QKind, Request, Response};
use tdy_plan::{Action, Mode};
use tdy_rules::Policy;
use tdy_rules::calib::Calibration;
use tdy_rules::guards::{GuardConfig, Guards};
use tdy_rules::l2::{self, OutcomeKind};

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
                    let p =
                        self.0.iter().find(|(qn, item, _)| q.name.contains(qn) && name_line.contains(item)).map(|x| x.2).unwrap_or(0.01);
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
                    let pick = options
                        .iter()
                        .find(|(v, _)| name_line.to_lowercase().contains(&v.to_lowercase()))
                        .map(|o| o.0.clone())
                        .unwrap_or("none".into());
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
            let year = &tdy_rules::items::ymd(tdy_scan::fmt::now_secs() - 90 * 86_400)[..4];
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
    assert_eq!(unclear.status, tdy_plan::Status::Pending);
    let figma = es.iter().find(|e| e.path.ends_with("Figma-124.3.dmg")).unwrap();
    assert_eq!(figma.status, tdy_plan::Status::Approved);
    assert!(figma.evidence.as_ref().unwrap()["rules"]["installers"].as_f64().unwrap() > 0.9);
}

#[test]
fn templates() {
    let it = tdy_rules::items::Item {
        path: "/x/IMG_0001.HEIC".into(),
        is_dir: false,
        logical: 1,
        alloc: 1,
        mtime: tdy_rules::media::parse_exif_datetime("2021:03:04 05:06:07").unwrap(),
        flags: 0,
        depth: 1,
        dir: None,
    };
    assert_eq!(l2::expand_template("~/Pictures/{mtime:%Y}/{mtime:%m - %B}", &it, None), "~/Pictures/2021/03 - March");
    assert_eq!(l2::expand_template("{stem}.{ext}", &it, None), "IMG_0001.heic");
}

/// A judge that refuses every question about protection.
struct RefuseKeep;
impl Judge for RefuseKeep {
    fn id(&self) -> String {
        "refuse".into()
    }
    fn caps(&self) -> Caps {
        Caps { images: false, max_images: 0, max_questions: 64 }
    }
    fn decide(&self, req: &Request) -> Result<Response, JudgeError> {
        let mut r = Response::default();
        for q in &req.questions {
            let a = if q.name.starts_with("protect") { Answer::Refusal } else { Answer::Predicate { p: 0.99 } };
            r.answers.insert(q.name.clone(), a);
        }
        Ok(r)
    }
}

#[test]
fn unanswered_keep_rule_protects() {
    let t = tempfile::tempdir().unwrap();
    let dl = t.path().join("Downloads");
    fs::create_dir_all(&dl).unwrap();
    fs::write(dl.join("2019-tax-return.txt"), b"income").unwrap();
    old(&dl.join("2019-tax-return.txt"));
    let policy: Policy = toml::from_str(&format!(
        r#"
[[rule]]
id = "protect.tax"
action = "keep"
scope = ["{dl}"]
when.any = ["tax documents"]

[[rule]]
id = "old-text"
action = "trash"
scope = ["{dl}"]
filter = {{ ext = ["txt"] }}
"#,
        dl = dl.display()
    ))
    .unwrap();
    let guards = Guards::new(GuardConfig { min_age: 0, ..Default::default() });
    let rules = l2::compile(&policy, &[t.path().to_path_buf()], None, &Calibration::default());
    let run = l2::run(&policy, &rules, Some(&RefuseKeep), &guards, 2, &|_, _| {}).unwrap();
    assert!(matches!(&run.outcomes[0].kind, OutcomeKind::Kept { p: None, .. }), "{:?}", run.outcomes[0].kind);
    let entries = l2::entries(&run, Mode::Autonomous, None, &mut Default::default());
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, tdy_plan::Status::Rejected);
    assert!(entries[0].refusal.as_deref().unwrap().contains("keep rule protect.tax was unanswered"));
    assert!(entries[0].approved_by.is_none());
}

#[test]
fn guard_and_negative_judge_refusals_are_retained_without_unrelated_candidates() {
    let t = tempfile::tempdir().unwrap();
    for name in ["guard.txt", "negative.txt", "unrelated.bin"] {
        fs::write(t.path().join(name), b"candidate").unwrap();
        old(&t.path().join(name));
    }
    let policy: Policy = toml::from_str(&format!(
        r#"
[[rule]]
id = "remove.text"
action = "trash"
scope = ["{}"]
filter = {{ ext = ["txt"] }}
when.any = ["obsolete"]
"#,
        t.path().display()
    ))
    .unwrap();
    let guards = Guards::new(GuardConfig { min_age: 0, protect: vec![t.path().join("guard.txt")], ..Default::default() });
    let rules = l2::compile(&policy, &[t.path().to_path_buf()], None, &Calibration::default());
    let run = l2::run(&policy, &rules, Some(&Script(vec![])), &guards, 2, &|_, _| {}).unwrap();
    assert_eq!(run.requests, 1); // guard refusal does not need a paid judge call
    let entries = l2::entries(&run, Mode::Autonomous, None, &mut Default::default());
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|e| e.status == tdy_plan::Status::Rejected && !e.override_refusal && e.approved_by.is_none()));
    assert!(entries.iter().find(|e| e.path.ends_with("guard.txt")).unwrap().refusal.as_deref().unwrap().starts_with("protect:"));
    assert!(entries.iter().find(|e| e.path.ends_with("negative.txt")).unwrap().refusal.as_deref().unwrap().contains("judge did not match"));
}

#[test]
fn partial_l2_guard_refusal_is_retained_beside_allowed_tag() {
    let t = tempfile::tempdir().unwrap();
    fs::write(t.path().join("fresh.txt"), b"candidate").unwrap();
    let policy: Policy = toml::from_str(&format!(
        r#"
[[rule]]
id = "remove"
action = "trash"
scope = ["{scope}"]
[[rule]]
id = "label"
action = "tag"
scope = ["{scope}"]
tags = ["Review"]
"#,
        scope = t.path().display()
    ))
    .unwrap();
    let guards = Guards::new(GuardConfig::default());
    let rules = l2::compile(&policy, &[t.path().to_path_buf()], None, &Calibration::default());
    let run = l2::run(&policy, &rules, None, &guards, 2, &|_, _| {}).unwrap();
    let entries = l2::entries(&run, Mode::Autonomous, None, &mut Default::default());
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().any(|e| e.action == Action::Tag && e.status == tdy_plan::Status::Approved));
    assert!(entries.iter().any(|e| e.action == Action::Trash && e.status == tdy_plan::Status::Rejected && e.refusal.is_some()));
}

#[test]
fn tag_below_threshold_is_retained_as_refused() {
    let t = tempfile::tempdir().unwrap();
    fs::write(t.path().join("candidate.txt"), b"candidate").unwrap();
    let policy: Policy = toml::from_str(&format!(
        r#"
[[rule]]
id = "label"
action = "tag"
scope = ["{}"]
tags = ["Review"]
threshold = 0.8
when.any = ["relevant"]
"#,
        t.path().display()
    ))
    .unwrap();
    let guards = Guards::new(GuardConfig::default());
    let rules = l2::compile(&policy, &[t.path().to_path_buf()], None, &Calibration::default());
    let run = l2::run(&policy, &rules, Some(&Script(vec![("label", "candidate", 0.7)])), &guards, 2, &|_, _| {}).unwrap();
    let entries = l2::entries(&run, Mode::Autonomous, None, &mut Default::default());
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, tdy_plan::Status::Rejected);
    assert!(entries[0].refusal.as_deref().unwrap().contains("judge did not match"));
}

#[test]
fn refused_delete_preserves_safety_downgrade() {
    let t = tempfile::tempdir().unwrap();
    fs::write(t.path().join("wedding.txt"), b"candidate").unwrap();
    let policy: Policy = toml::from_str(&format!(
        r#"
[[rule]]
id = "remove"
action = "delete"
scope = ["{scope}"]
when.any = ["obsolete"]
[[rule]]
id = "label"
action = "tag"
scope = ["{scope}"]
tags = ["Review"]
"#,
        scope = t.path().display()
    ))
    .unwrap();
    let guards = Guards::new(GuardConfig::default());
    let rules = l2::compile(&policy, &[t.path().to_path_buf()], None, &Calibration::default());
    let run = l2::run(&policy, &rules, Some(&Script(vec![("remove", "wedding", 0.99)])), &guards, 2, &|_, _| {}).unwrap();
    assert_eq!(run.requests, 1);
    let entries = l2::entries(&run, Mode::Autonomous, None, &mut Default::default());
    let refused = entries.iter().find(|e| e.refusal.is_some()).unwrap();
    assert_eq!(refused.action, Action::Trash);
    assert_eq!(refused.undo, tdy_plan::Undo::Restore);
    assert_eq!(refused.status, tdy_plan::Status::Rejected);
    assert!(refused.refusal.as_deref().unwrap().starts_with("recent:"));
    assert!(refused.reason.contains("downgraded delete → trash"));
}
