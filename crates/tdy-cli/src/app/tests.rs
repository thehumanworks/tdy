//! Headless tests for the TUI: the real app, real background jobs and a
//! sandboxed HOME, drawn into ratatui's test backend.

use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

fn sandbox() -> (std::sync::MutexGuard<'static, ()>, tempfile::TempDir) {
    let lock = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    // SAFETY: env changes are serialised by TEST_ENV across this test binary.
    unsafe {
        std::env::set_var("HOME", t.path());
        std::env::set_var("TDY_HOME", t.path().join("state"));
        std::env::set_var("TDY_CONFIG_DIR", t.path().join("cfg"));
        std::env::set_var("TDY_CACHE_DIR", t.path().join("cache"));
        std::env::set_var("XDG_DATA_HOME", t.path().join("share"));
        std::env::set_var("TDY_NO_FNOX", "1");
        std::env::remove_var("TDY_POLICY");
    }
    fs::create_dir_all(t.path().join("share")).unwrap();
    (lock, t)
}

fn old(p: &Path) {
    let when = SystemTime::now() - Duration::from_secs(40 * 86_400);
    fs::File::options().write(true).open(p).unwrap().set_modified(when).unwrap();
}

fn write(p: &Path, n: usize) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, vec![7u8; n]).unwrap();
    old(p);
}

fn key(app: &mut App, c: KeyCode) {
    app.on_key(KeyEvent::new(c, KeyModifiers::NONE));
}

fn ch(app: &mut App, c: char) {
    key(app, KeyCode::Char(c));
}

/// Feed background messages to the app until `done` holds (or fail after 30 s).
fn pump(app: &mut App, what: &str, done: impl Fn(&App) -> bool) {
    let t = Instant::now();
    while !done(app) {
        assert!(t.elapsed() < Duration::from_secs(30), "timed out waiting for {what}");
        if let Ok(m) = app.rx.recv_timeout(Duration::from_millis(50)) {
            app.on_msg(m);
        }
    }
}

fn screen(app: &mut App) -> String {
    let mut term = Terminal::new(TestBackend::new(140, 36)).unwrap();
    term.draw(|f| app.draw(f)).unwrap();
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn refused_explore_items_require_confirmation_and_apply_from_the_same_plan() {
    let (_lock, t) = sandbox();
    for mixed in [false, true] {
        let root = t.path().join(if mixed { "mixed" } else { "refused" });
        let recent = root.join("recent.bin");
        write(&recent, 500);
        fs::File::options().write(true).open(&recent).unwrap().set_modified(SystemTime::now()).unwrap();
        let old_path = root.join("old.bin");
        if mixed {
            write(&old_path, 500);
        }
        let mut app = App::new(root);
        app.arm = Duration::ZERO;
        pump(&mut app, "scan", |a| a.explore.index.is_some());
        let mut paths = vec![recent.clone()];
        if mixed {
            paths.push(old_path.clone());
        }
        for p in &paths {
            app.explore.marks.insert(p.clone(), Bytes::default());
        }
        app.confirmed(Confirm::Propose(Action::Quarantine, paths.iter().map(|p| (p.clone(), Bytes::default())).collect()));
        pump(&mut app, "refusal dialog", |a| a.dialog.is_some());
        let id = match &app.dialog.as_ref().unwrap().confirm {
            Some(Confirm::OverrideRefusals(id)) => id.clone(),
            _ => panic!("missing refusal override confirmation"),
        };
        assert!(screen(&mut app).contains("Override the retained refusals"));
        let (before, _) = tdy_plan::store::load(Some(&id)).unwrap();
        assert_eq!(before.entries.len(), paths.len());
        assert_eq!(before.entries.iter().filter(|e| e.status == tdy_plan::Status::Approved).count(), usize::from(mixed));
        assert_eq!(app.explore.marks.len(), 1, "only the refused mark remains");
        ch(&mut app, 'n');
        let (cancelled, _) = tdy_plan::store::load(Some(&id)).unwrap();
        assert!(cancelled.entries.iter().all(|e| !e.override_refusal));
        assert!(recent.exists(), "cancel never changes the file");

        app.confirmed(Confirm::OverrideRefusals(id.clone()));
        assert_eq!(app.tab, Tab::Plans);
        assert!(app.explore.marks.is_empty(), "overridden marks are now accepted too");
        let plan = &app.plans.review.as_ref().unwrap().plan;
        assert_eq!(plan.header.id, id);
        assert_eq!(plan.entries.len(), paths.len(), "the original plan is edited, never reproposed");
        assert!(plan.entries.iter().all(|e| e.status == tdy_plan::Status::Approved));
        assert!(plan.entries.iter().find(|e| e.path == recent).unwrap().override_refusal);
        ch(&mut app, 'X');
        pump(&mut app, "dry run", |a| a.dialog.is_some());
        assert!(matches!(app.dialog.as_ref().unwrap().confirm, Some(Confirm::Apply(_))));
        assert!(recent.exists());
        ch(&mut app, 'y');
        pump(&mut app, "apply", |a| a.plans.busy.is_none());
        assert!(!recent.exists());
        if mixed {
            assert!(!old_path.exists());
        }
        let (applied, _) = tdy_plan::store::load(Some(&id)).unwrap();
        assert_eq!(applied.entries.len(), paths.len());
        assert!(applied.entries.iter().all(|e| e.status == tdy_plan::Status::Applied));
    }
}

#[test]
fn standalone_review_requires_override_and_respects_visible_filter() {
    let (_lock, t) = sandbox();
    let paths = [t.path().join("one.bin"), t.path().join("two.bin")];
    for path in &paths {
        write(path, 100);
        fs::File::options().write(true).open(path).unwrap().set_modified(SystemTime::now()).unwrap();
    }
    let proposed = crate::propose::propose(
        paths
            .iter()
            .map(|path| crate::propose::Proposal {
                action: Action::Quarantine,
                path: path.clone(),
                dest: None,
                tags: vec![],
                reason: "review fixture".into(),
                bytes: None,
            })
            .collect(),
        crate::propose::By::Human,
        "review fixture",
    )
    .unwrap();
    let mut view = crate::tui::ReviewView::new(proposed.plan.unwrap(), tdy_plan::review::Filter::default());
    let press = |view: &mut crate::tui::ReviewView, code| view.handle(KeyEvent::new(code, KeyModifiers::NONE));
    press(&mut view, KeyCode::Char('A'));
    assert!(view.plan.entries.iter().all(|e| e.status == tdy_plan::Status::Rejected));
    assert!(!view.dirty);
    press(&mut view, KeyCode::Char('o'));
    assert!(view.typing(), "confirmation captures global keys in either host");
    press(&mut view, KeyCode::Char('n'));
    assert!(!view.dirty);
    press(&mut view, KeyCode::Char('/'));
    for c in "one.bin".chars() {
        press(&mut view, KeyCode::Char(c));
    }
    press(&mut view, KeyCode::Enter);
    press(&mut view, KeyCode::Char('O'));
    let mut terminal = Terminal::new(TestBackend::new(120, 25)).unwrap();
    terminal.draw(|f| view.draw(f, f.area(), "")).unwrap();
    let rendered: String = terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect();
    assert!(rendered.contains("Override refusals?") && rendered.contains("recent"), "{rendered}");
    press(&mut view, KeyCode::Char('y'));
    assert!(view.dirty);
    assert!(view.plan.entries[0].override_refusal);
    assert_eq!(view.plan.entries[0].status, tdy_plan::Status::Approved);
    assert!(!view.plan.entries[1].override_refusal);
    assert_eq!(view.plan.entries[1].status, tdy_plan::Status::Rejected);
}

#[test]
fn new_plan_opens_all_refused_cleanup_and_ask_results() {
    let (_lock, t) = sandbox();
    let root = t.path().join("fixtures");
    write(&root.join("project/Cargo.toml"), 10);
    write(&root.join("project/target/debug/app"), 20_000);
    write(&root.join("setup.dmg"), 20_000);
    fs::create_dir_all(t.path().join("cfg")).unwrap();
    fs::write(t.path().join("cfg/policy.toml"), format!("protect = [\"{}\"]\n[detectors]\nmin_size = \"1KB\"\n", root.display())).unwrap();
    for kind in [newplan::Kind::Cleanup, newplan::Kind::Ask] {
        let mut app = App::new(root.clone());
        app.arm = Duration::ZERO;
        pump(&mut app, "scan", |a| a.explore.index.is_some());
        app.switch(Tab::NewPlan);
        app.newplan.kind = kind;
        if kind == newplan::Kind::Ask {
            app.newplan.request = "trash dmg files older than 30 days".into();
        }
        key(&mut app, KeyCode::Enter);
        pump(&mut app, "all-refused plan", |a| a.newplan.result.is_some());
        let result = app.newplan.result.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(result.actions, 0);
        assert!(result.retained > 0);
        assert_eq!(app.tab, Tab::Plans);
        let view = app.plans.review.as_ref().unwrap();
        assert_eq!(view.plan.header.id, result.plan);
        assert!(view.plan.entries.iter().all(|e| e.refusal.is_some() && e.status == tdy_plan::Status::Rejected));
        ch(&mut app, 'O');
        assert!(screen(&mut app).contains("Override refusals?"));
        ch(&mut app, 'n');
        assert!(app.plans.review.as_ref().unwrap().plan.entries.iter().all(|e| !e.override_refusal));
    }
}

#[test]
fn explore_git_refusal_displays_local_work_and_can_be_overridden() {
    let (_lock, t) = sandbox();
    let root = t.path().join("fixtures");
    let repo = root.join("repository");
    write(&repo.join("tracked.txt"), 100);
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(&repo)
            .args(["-c", "user.name=Tidy Test", "-c", "user.email=tidy@example.invalid", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "--quiet"]);
    git(&["add", "tracked.txt"]);
    git(&["commit", "--quiet", "-m", "fixture"]);
    write(&repo.join("tracked.txt"), 200);
    git(&["stash", "push", "--quiet"]);
    write(&repo.join("dirty.txt"), 100);
    fs::create_dir_all(t.path().join("cfg")).unwrap();
    fs::write(t.path().join("cfg/policy.toml"), "min_age = \"0s\"\n").unwrap();
    let mut app = App::new(root);
    app.arm = Duration::ZERO;
    pump(&mut app, "scan", |a| a.explore.index.is_some());
    app.confirmed(Confirm::Propose(Action::Quarantine, vec![(repo.clone(), Bytes::default())]));
    pump(&mut app, "git refusal", |a| a.dialog.is_some());
    let rendered = screen(&mut app);
    assert!(rendered.contains("stash") && rendered.contains("no upstream") && rendered.contains("uncommitted"), "{rendered}");
    ch(&mut app, 'y');
    let view = app.plans.review.as_ref().unwrap();
    assert_eq!(view.plan.entries.len(), 1);
    assert!(view.plan.entries[0].override_refusal);
    ch(&mut app, 'X');
    pump(&mut app, "git dry run", |a| a.dialog.is_some());
    ch(&mut app, 'y');
    pump(&mut app, "git apply", |a| a.plans.busy.is_none());
    assert!(!repo.exists(), "{:?} {:?}", app.plans.review.as_ref().unwrap().plan.entries[0].outcome, app.plans.log);
    assert_eq!(app.plans.review.as_ref().unwrap().plan.entries[0].status, tdy_plan::Status::Applied);
}

#[test]
fn explore_mark_plan_apply_undo() {
    let (_lock, t) = sandbox();
    let root = t.path().join("work");
    write(&root.join("big/movie.bin"), 300_000);
    write(&root.join("small/note.txt"), 10_000);
    write(&root.join("loose.bin"), 50_000);
    let root = fs::canonicalize(root).unwrap();

    let mut app = App::new(root.clone());
    app.arm = Duration::ZERO;
    pump(&mut app, "the scan", |a| a.explore.index.is_some());
    let s = screen(&mut app);
    assert!(s.contains("big/") && s.contains("small/") && s.contains("loose.bin"), "{s}");
    assert!(s.contains("1 Explore") && s.contains("2 Plans") && s.contains("3 New plan"));

    // largest first; open it, see its file, come back to the same row
    assert_eq!(app.explore.rows[0].name, "big");
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.explore.rows[0].name, "movie.bin");
    key(&mut app, KeyCode::Backspace);
    assert_eq!(app.explore.selected().unwrap().name, "big");
    // at the top, going up only explains why it can't
    key(&mut app, KeyCode::Backspace);
    assert!(app.toast.as_ref().is_some_and(|(t, _)| t.contains("top of the scan")));

    // sort by name: folders first, alphabetical
    ch(&mut app, 's');
    let names: Vec<&str> = app.explore.rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["big", "small", "loose.bin"]);
    ch(&mut app, 's');
    ch(&mut app, 's');
    ch(&mut app, 's');
    assert_eq!(app.explore.sort, explore::Sort::Size);

    // filter
    ch(&mut app, '/');
    for c in "sma".chars() {
        ch(&mut app, c);
    }
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.explore.rows.len(), 1);
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.explore.rows.len(), 3);

    // details panel: guards are checked in the background
    ch(&mut app, 'i');
    pump(&mut app, "the explanation", |a| a.explore.selected().is_some_and(|r| a.explore.is_explained(&r.path)));
    let s = screen(&mut app);
    assert!(s.contains("guards") && s.contains("trash"), "{s}");
    ch(&mut app, 'i');

    // mark the big folder → trash dialog → a plan approved by the human
    ch(&mut app, ' ');
    assert_eq!(app.explore.marks.len(), 1);
    ch(&mut app, 't');
    assert!(app.dialog.as_ref().is_some_and(|d| matches!(d.confirm, Some(Confirm::Propose(Action::Trash, _)))));
    assert!(screen(&mut app).contains("trash 1 item(s)?"));
    ch(&mut app, 'y');
    pump(&mut app, "the plan", |a| a.tab == Tab::Plans && a.plans.review.is_some());
    assert!(app.explore.marks.is_empty(), "marks of accepted items are cleared");
    let plan = app.plans.review.as_ref().unwrap().plan.clone();
    assert_eq!(plan.entries.len(), 1);
    assert_eq!(plan.entries[0].status, tdy_plan::Status::Approved);
    assert_eq!(plan.entries[0].approved_by.as_deref(), Some("human"));
    assert_eq!(plan.entries[0].path, fs::canonicalize(root.join("big")).unwrap());
    let id = plan.header.id.clone();

    // apply: a dry run first, then a confirmation
    ch(&mut app, 'X');
    pump(&mut app, "the dry run", |a| a.dialog.is_some());
    assert!(root.join("big").exists(), "a dry run changes nothing");
    let s = screen(&mut app);
    assert!(s.contains(&format!("Apply plan {id}?")) && s.contains("1 entry would apply"), "{s}");
    ch(&mut app, 'y');
    pump(&mut app, "the apply", |a| a.plans.busy.is_none() && a.toast.as_ref().is_some_and(|(t, _)| t.contains("applied")));
    assert!(!root.join("big").exists(), "moved to the trash");
    #[cfg(target_os = "macos")]
    assert!(t.path().join(".Trash/big").exists());
    #[cfg(not(target_os = "macos"))]
    assert!(t.path().join("share/Trash/files/big").exists());
    assert_eq!(app.plans.review.as_ref().unwrap().plan.entries[0].status, tdy_plan::Status::Applied);
    // Explore shows the change without a rescan: the cached index was patched
    pump(&mut app, "the patched index", |a| a.explore.patched);
    assert!(app.explore.scanning.is_none() && !app.explore.stale);
    let idx = app.explore.index.as_ref().unwrap();
    assert!(idx.lookup(&root.join("big")).is_none());
    assert_eq!(idx.root_node().files, 2);

    // undo, confirmed
    ch(&mut app, 'U');
    assert!(app.dialog.as_ref().is_some_and(|d| matches!(d.confirm, Some(Confirm::Undo(_)))));
    ch(&mut app, 'y');
    pump(&mut app, "the undo", |a| a.plans.busy.is_none() && a.toast.as_ref().is_some_and(|(t, _)| t.contains("restored")));
    assert!(root.join("big/movie.bin").exists());
    pump(&mut app, "the restored folder", |a| a.explore.index.as_ref().is_some_and(|i| i.lookup(&root.join("big")).is_some()));
    assert!(app.explore.patched && app.explore.scanning.is_none());
    assert_eq!(app.explore.index.as_ref().unwrap().root_node().files, 3);

    // back to the list; the plan is there; help draws
    key(&mut app, KeyCode::Esc); // hides the log
    key(&mut app, KeyCode::Esc); // leaves the plan
    assert!(app.plans.review.is_none());
    assert!(screen(&mut app).contains(&id));
    ch(&mut app, '?');
    assert!(screen(&mut app).contains("Plans — keys"));
    ch(&mut app, 'x'); // any key closes help
    // rescan after the changes
    ch(&mut app, '1');
    ch(&mut app, 'r');
    pump(&mut app, "the rescan", |a| a.explore.scanning.is_none());
    assert!(!app.explore.stale && !app.explore.patched && app.explore.cached.is_none());
    ch(&mut app, 'q');
    assert!(app.quit);
}

#[test]
fn cleanup_makes_a_plan() {
    let (_lock, t) = sandbox();
    fs::create_dir_all(t.path().join("cfg")).unwrap();
    fs::write(t.path().join("cfg/policy.toml"), "[detectors]\nmin_size = \"1KB\"\n").unwrap();
    let proj = t.path().join("code/app");
    write(&proj.join("Cargo.toml"), 10);
    write(&proj.join("target/debug/app"), 20_000);
    write(&proj.join("src/main.rs"), 100);

    let mut app = App::new(t.path().join("code"));
    app.arm = Duration::ZERO;
    pump(&mut app, "the scan", |a| a.explore.index.is_some());
    // the detector tags the target folder while browsing
    key(&mut app, KeyCode::Enter);
    let s = screen(&mut app);
    assert!(s.contains("target/") && s.contains("cargo-target"), "{s}");

    ch(&mut app, '3');
    assert!(screen(&mut app).contains("build artefacts and caches"));
    // edit the folder: replace it with the project path
    ch(&mut app, 'e');
    for _ in 0..200 {
        key(&mut app, KeyCode::Backspace);
    }
    for c in proj.display().to_string().chars() {
        ch(&mut app, c);
    }
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Enter); // run
    // the finished plan opens on the Plans screen
    pump(&mut app, "the cleanup", |a| a.newplan.result.is_some() && a.plans.review.is_some());
    let r = app.newplan.result.clone().unwrap().unwrap();
    assert_eq!(r.actions, 1, "{r:?}");
    assert_eq!(app.tab, Tab::Plans);
    let v = app.plans.review.as_ref().unwrap();
    assert_eq!(v.plan.header.id, r.plan);
    assert_eq!(v.plan.entries[0].path, fs::canonicalize(proj.join("target")).unwrap());
    assert_eq!(v.plan.entries[0].status, tdy_plan::Status::Pending);
    // n on the list starts another
    key(&mut app, KeyCode::Esc);
    ch(&mut app, 'n');
    assert_eq!(app.tab, Tab::NewPlan);
}

#[test]
fn p_in_explore_plans_the_folder_on_screen() {
    let (_lock, t) = sandbox();
    fs::create_dir_all(t.path().join("cfg")).unwrap();
    fs::write(t.path().join("cfg/policy.toml"), "[detectors]\nmin_size = \"1KB\"\n").unwrap();
    let code = t.path().join("code");
    let proj = code.join("app");
    write(&proj.join("Cargo.toml"), 10);
    write(&proj.join("target/debug/app"), 20_000);
    write(&code.join("other/node_modules/x/index.js"), 20_000);
    write(&code.join("other/package.json"), 10);
    let mut app = App::new(code.clone());
    app.arm = Duration::ZERO;
    pump(&mut app, "the scan", |a| a.explore.index.is_some());
    select(&mut app, "app");
    key(&mut app, KeyCode::Enter);
    ch(&mut app, 'p');
    assert_eq!(app.tab, Tab::NewPlan);
    assert!(app.newplan.running.is_some(), "starts right away");
    pump(&mut app, "the plan", |a| a.plans.review.is_some());
    let v = app.plans.review.as_ref().unwrap();
    // only the folder on screen: app/target, not other/node_modules
    let paths: Vec<_> = v.plan.entries.iter().map(|e| e.path.clone()).collect();
    assert_eq!(paths, [fs::canonicalize(&proj).unwrap().join("target")]);
}

#[test]
fn ask_in_plain_english() {
    let (_lock, t) = sandbox();
    let dl = t.path().join("work/Downloads");
    for n in ["Figma.dmg", "setup.pkg", "notes.txt"] {
        write(&dl.join(n), 30_000);
    }
    let mut app = App::new(t.path().join("work"));
    app.arm = Duration::ZERO;
    pump(&mut app, "the scan", |a| a.explore.index.is_some());
    ch(&mut app, 'a');
    assert_eq!((app.tab, app.newplan.kind), (Tab::NewPlan, newplan::Kind::Ask));
    assert_eq!(app.newplan.editing, Some(newplan::Field::Request), "ready to type");
    for c in "trash installers in Downloads older than 30 days".chars() {
        ch(&mut app, c);
    }
    let s = screen(&mut app);
    assert!(s.contains("installers and disk images · older than 30 days"), "{s}");
    assert!(s.contains("not needed: the filters say it all"), "{s}");
    key(&mut app, KeyCode::Enter);
    pump(&mut app, "the plan", |a| a.plans.review.is_some());
    let v = app.plans.review.as_ref().unwrap();
    let mut names: Vec<_> = v.plan.entries.iter().map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names, ["Figma.dmg", "setup.pkg"]);
    assert!(v.plan.entries.iter().all(|e| e.action == Action::Trash && e.status == tdy_plan::Status::Pending));
    assert!(v.plan.header.note.as_deref().unwrap().starts_with("asked: trash installers"));

    // a request that needs the judge says so before running
    ch(&mut app, '3');
    ch(&mut app, 'w');
    for _ in 0..100 {
        key(&mut app, KeyCode::Backspace);
    }
    for c in "quarantine screenshots in Downloads".chars() {
        ch(&mut app, c);
    }
    assert!(screen(&mut app).contains("but no judge is configured"));
    key(&mut app, KeyCode::Enter);
    pump(&mut app, "the refusal", |a| a.dialog.is_some());
    let d = app.dialog.as_ref().unwrap();
    assert!(d.lines.iter().any(|l| l.to_string().contains("needs a judge")));
}

#[test]
fn duplicates_and_archives() {
    let (_lock, t) = sandbox();
    let root = t.path().join("work");
    write(&root.join("a/report.bin"), 50_000);
    write(&root.join("b/report-copy.bin"), 50_000);
    write(&root.join("cold/old/data.bin"), 400_000);
    let mut app = App::new(root.clone());
    app.arm = Duration::ZERO;
    pump(&mut app, "the scan", |a| a.explore.index.is_some());
    ch(&mut app, '3');
    key(&mut app, KeyCode::Right);
    key(&mut app, KeyCode::Right);
    assert_eq!(app.newplan.kind, newplan::Kind::Dupes);
    ch(&mut app, 'm');
    for _ in 0..5 {
        key(&mut app, KeyCode::Backspace);
    }
    for c in "10KB".chars() {
        ch(&mut app, c);
    }
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Enter);
    pump(&mut app, "the duplicates plan", |a| a.plans.review.is_some());
    let v = app.plans.review.as_ref().unwrap();
    assert_eq!(v.plan.entries.len(), 1, "one copy of the pair");
    assert!(matches!(v.plan.entries[0].action, Action::Dedupe | Action::Trash));
    assert_eq!(app.newplan.result.as_ref().unwrap().as_ref().unwrap().found.as_deref(), Some("1 duplicate group(s)"));

    // archives: folders untouched for 30 days, from 100 KB
    key(&mut app, KeyCode::Esc);
    ch(&mut app, '3');
    key(&mut app, KeyCode::Right);
    assert_eq!(app.newplan.kind, newplan::Kind::Archive);
    ch(&mut app, 'g');
    for _ in 0..5 {
        key(&mut app, KeyCode::Backspace);
    }
    for c in "30d".chars() {
        ch(&mut app, c);
    }
    key(&mut app, KeyCode::Enter);
    ch(&mut app, 'm');
    for _ in 0..6 {
        key(&mut app, KeyCode::Backspace);
    }
    for c in "100KB".chars() {
        ch(&mut app, c);
    }
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Enter);
    pump(&mut app, "the archive plan", |a| a.newplan.result.is_some() && a.newplan.running.is_none());
    let r = app.newplan.result.clone().unwrap().unwrap();
    assert!(r.actions >= 1, "{r:?}");
    let v = app.plans.review.as_ref().unwrap();
    assert_eq!(v.plan.header.id, r.plan);
    assert!(v.plan.entries.iter().all(|e| e.action == Action::Archive));
    assert!(v.plan.entries.iter().any(|e| e.path.ends_with("cold")));
}

#[test]
fn starts_from_the_cached_index() {
    let (_lock, t) = sandbox();
    let root = t.path().join("work");
    write(&root.join("a/one.bin"), 10_000);
    tdy_scan::cache::store(&tdy_scan::scan(&root, &Default::default()).unwrap()).unwrap();
    // changed since that scan
    write(&root.join("b/two.bin"), 20_000);

    let mut app = App::new(root.clone());
    pump(&mut app, "the cached index", |a| a.explore.index.is_some());
    let root = fs::canonicalize(&root).unwrap();
    assert_eq!(app.explore.cached.as_deref(), Some(root.as_path()));
    assert!(app.explore.scanning.is_some(), "a fresh scan runs meanwhile");
    let names: Vec<&str> = app.explore.rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["a"], "the cached folders, before the refresh");
    assert!(screen(&mut app).contains("refreshing the cached index"));

    pump(&mut app, "the refresh", |a| a.explore.scanning.is_none());
    assert!(app.explore.cached.is_none());
    let names: Vec<&str> = app.explore.rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["b", "a"]);
    assert!(screen(&mut app).contains("scanned"));

    // a folder inside a cached one starts from the parent's index
    let mut app = App::new(root.join("b"));
    pump(&mut app, "the cut-out index", |a| a.explore.index.is_some());
    assert_eq!(app.explore.cached.as_deref(), Some(root.as_path()));
    assert_eq!(app.explore.rows[0].name, "two.bin");
    // as after Esc stopped the refresh: the header says where it came from
    app.explore.scanning = None;
    assert!(screen(&mut app).contains(&format!("cached index of {}, scanned", display_path(&root))));
}

#[test]
fn bar_widths() {
    assert_eq!(explore::bar(0.0, 4), "    ");
    assert_eq!(explore::bar(1.0, 4), "████");
    assert_eq!(explore::bar(0.5, 4), "██  ");
    assert_eq!(explore::bar(0.0625, 4).chars().count(), 4);
}

fn select(app: &mut App, name: &str) {
    let i = app.explore.rows.iter().position(|r| r.name == name).unwrap_or_else(|| panic!("no row {name}"));
    app.explore.state.select(Some(i));
}

#[test]
fn background_results_never_hijack_input() {
    let (_lock, t) = sandbox();
    let root = t.path().join("work");
    for n in ["a.bin", "b.bin", "c.bin"] {
        write(&root.join(n), 20_000);
    }
    let mut app = App::new(root.clone());
    app.arm = Duration::ZERO;
    pump(&mut app, "the scan", |a| a.explore.index.is_some());

    // plan A: trash a.bin, approved
    select(&mut app, "a.bin");
    ch(&mut app, 't');
    ch(&mut app, 'y');
    pump(&mut app, "plan A", |a| a.plans.review.is_some());
    let plan_a = app.plans.review.as_ref().unwrap().plan.header.id.clone();

    // dry run of A starts; before it reports, the user opens another confirmation
    ch(&mut app, 'X');
    ch(&mut app, '1');
    select(&mut app, "b.bin");
    ch(&mut app, 't');
    assert!(matches!(app.dialog.as_ref().unwrap().confirm, Some(Confirm::Propose(..))));
    pump(&mut app, "the dry run", |a| a.plans.busy.is_none());
    // the dry-run result waits its turn instead of replacing the open dialog
    assert!(matches!(app.dialog.as_ref().unwrap().confirm, Some(Confirm::Propose(..))));
    assert_eq!(app.queue.len(), 1);
    ch(&mut app, 'y'); // answers the trash question, not "apply A?"
    assert!(matches!(&app.dialog.as_ref().unwrap().confirm, Some(Confirm::Apply(id)) if *id == plan_a));
    // the propose result arrives while a dialog is open: no tab jump
    pump(&mut app, "plan B", |a| a.toast.as_ref().is_some_and(|(t, _)| t.contains("is ready")));
    assert_eq!(app.tab, Tab::Explore);
    ch(&mut app, 'n');
    assert!(root.join("a.bin").exists(), "A was not applied");
    assert!(root.join("b.bin").exists(), "B is only planned");

    // a result arriving while the user types elsewhere doesn't steal the keys
    select(&mut app, "c.bin");
    ch(&mut app, 't');
    ch(&mut app, 'y');
    ch(&mut app, '3');
    ch(&mut app, 'e');
    pump(&mut app, "plan C", |a| a.plans.rows.len() == 3);
    assert_eq!(app.tab, Tab::NewPlan);
    assert_eq!(app.newplan.editing, Some(newplan::Field::Folder));
    ch(&mut app, 'p');
    assert!(app.newplan.path.ends_with('p'), "typed into the folder field");

    // background dialogs ignore keys for a moment after they appear
    key(&mut app, KeyCode::Enter); // stop editing
    app.arm = Duration::from_millis(300);
    app.act(Act::Dialog(Dialog { title: "from a job".into(), lines: vec![], confirm: Some(Confirm::Undo(plan_a.clone())) }), true);
    ch(&mut app, 'n');
    assert!(app.dialog.is_some(), "an immediate key is ignored");
    std::thread::sleep(Duration::from_millis(350));
    ch(&mut app, 'n');
    assert!(app.dialog.is_none());
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_names_keep_their_bytes() {
    use std::os::unix::ffi::OsStrExt;
    let (_lock, t) = sandbox();
    let root = t.path().join("work");
    let name = std::ffi::OsStr::from_bytes(b"caf\xe9.bin");
    write(&root.join(name), 1000);
    fs::create_dir_all(root.join(std::ffi::OsStr::from_bytes(b"dir\xff"))).unwrap();
    let mut app = App::new(root.clone());
    pump(&mut app, "the scan", |a| a.explore.index.is_some());
    for r in &app.explore.rows {
        assert!(r.path.symlink_metadata().is_ok(), "{:?} must point at the real entry", r.path);
    }
    assert_eq!(app.explore.rows.len(), 2);
}

#[test]
fn asks_the_filters_describe_need_no_judge_key() {
    let (_lock, t) = sandbox();
    fs::create_dir_all(t.path().join("cfg")).unwrap();
    // a judge is configured, but its key isn't set
    fs::write(
        t.path().join("cfg/policy.toml"),
        "[judge]\ndefault = \"luna\"\n[judge.backends.luna]\nkind = \"openai-decisions\"\nmodel = \"gpt-6-luna\"\napi_key_env = \"TDY_TEST_UNSET_KEY\"\n",
    )
    .unwrap();
    let dl = t.path().join("Downloads");
    write(&dl.join("Figma.dmg"), 30_000);
    let req = tdy_rules::ask::parse("trash dmg files older than 30 days", &dl).unwrap();
    let a = crate::ask_cmds::ask_plan(&req, &|_, _| {}).unwrap();
    assert_eq!(a.plan.entries.len(), 1);
    // one that needs the judge says why it can't run
    let req = tdy_rules::ask::parse("trash screenshots", &dl).unwrap();
    let e = crate::ask_cmds::ask_plan(&req, &|_, _| {}).err().unwrap();
    assert!(format!("{e:#}").contains("TDY_TEST_UNSET_KEY"), "{e:#}");
}
