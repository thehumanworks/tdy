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
        std::env::set_var("TIDY_HOME", t.path().join("state"));
        std::env::set_var("TIDY_CONFIG_DIR", t.path().join("cfg"));
        std::env::set_var("TIDY_CACHE_DIR", t.path().join("cache"));
        std::env::set_var("XDG_DATA_HOME", t.path().join("share"));
        std::env::set_var("TIDY_NO_FNOX", "1");
        std::env::remove_var("TIDY_POLICY");
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
fn explore_mark_plan_apply_undo() {
    let (_lock, t) = sandbox();
    let root = t.path().join("work");
    write(&root.join("big/movie.bin"), 300_000);
    write(&root.join("small/note.txt"), 10_000);
    write(&root.join("loose.bin"), 50_000);

    let mut app = App::new(root.clone());
    app.arm = Duration::ZERO;
    pump(&mut app, "the scan", |a| a.explore.index.is_some());
    let s = screen(&mut app);
    assert!(s.contains("big/") && s.contains("small/") && s.contains("loose.bin"), "{s}");
    assert!(s.contains("1 Explore") && s.contains("2 Plans") && s.contains("3 Cleanup"));

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
    assert_eq!(plan.entries[0].status, tidy_plan::Status::Approved);
    assert_eq!(plan.entries[0].approved_by.as_deref(), Some("human"));
    assert_eq!(plan.entries[0].path, root.join("big"));
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
    assert!(t.path().join("share/Trash/files/big").exists());
    assert_eq!(app.plans.review.as_ref().unwrap().plan.entries[0].status, tidy_plan::Status::Applied);
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
    pump(&mut app, "the cleanup", |a| a.cleanup.result.is_some());
    let r = app.cleanup.result.clone().unwrap().unwrap();
    assert_eq!(r.actions, 1, "{r:?}");
    assert!(screen(&mut app).contains(&format!("plan {}", r.plan)));
    ch(&mut app, 'o');
    assert_eq!(app.tab, Tab::Plans);
    let v = app.plans.review.as_ref().unwrap();
    assert_eq!(v.plan.entries[0].path, proj.join("target"));
    assert_eq!(v.plan.entries[0].status, tidy_plan::Status::Pending);
}

#[test]
fn starts_from_the_cached_index() {
    let (_lock, t) = sandbox();
    let root = t.path().join("work");
    write(&root.join("a/one.bin"), 10_000);
    tidy_scan::cache::store(&tidy_scan::scan(&root, &Default::default()).unwrap()).unwrap();
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
    assert_eq!(app.tab, Tab::Cleanup);
    assert!(app.cleanup.editing);
    ch(&mut app, 'p');
    assert!(app.cleanup.path.ends_with('p'), "typed into the folder field");

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
