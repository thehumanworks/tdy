//! M1 exit criteria: plan → apply → undo restores fixture trees byte-for-byte;
//! stale entries are skipped; an interrupted apply is recovered.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tdy_plan::journal::{Journal, Phase};
use tdy_plan::model::*;
use tdy_plan::{ApplyOptions, Event, apply, store, undo};

fn home() -> &'static Path {
    static H: OnceLock<tempfile::TempDir> = OnceLock::new();
    H.get_or_init(|| {
        let t = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("TDY_HOME", t.path().join("tdy"));
            std::env::set_var("XDG_DATA_HOME", t.path().join("share"));
            // Keep the OS trash (~/.Trash on macOS) inside the sandbox too.
            std::env::set_var("HOME", t.path());
            std::env::set_var("TDY_CACHE_DIR", t.path().join("cache"));
        }
        fs::create_dir_all(t.path().join("share")).unwrap();
        t
    })
    .path()
}

/// (relative path → (kind, mode, content hash)) for a tree.
fn manifest(root: &Path) -> BTreeMap<PathBuf, (char, u32, String)> {
    let mut m = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            let md = fs::symlink_metadata(&p).unwrap();
            let rel = p.strip_prefix(root).unwrap().to_path_buf();
            let mode = md.permissions().mode() & 0o7777;
            if md.is_dir() {
                m.insert(rel, ('d', mode, String::new()));
                stack.push(p);
            } else if md.file_type().is_symlink() {
                m.insert(rel, ('l', 0, fs::read_link(&p).unwrap().display().to_string()));
            } else {
                m.insert(rel, ('f', mode, blake3::hash(&fs::read(&p).unwrap()).to_hex().to_string()));
            }
        }
    }
    m
}

fn fixture(root: &Path) {
    fs::create_dir_all(root.join("proj/target/debug/deps")).unwrap();
    fs::write(root.join("proj/Cargo.toml"), "[package]\nname='x'\n").unwrap();
    fs::write(root.join("proj/target/debug/deps/libx.rlib"), vec![7u8; 50_000]).unwrap();
    fs::create_dir_all(root.join("downloads")).unwrap();
    fs::write(root.join("downloads/Figma.dmg"), vec![1u8; 120_000]).unwrap();
    fs::write(root.join("downloads/notes.txt"), "keep").unwrap();
    fs::set_permissions(root.join("downloads/notes.txt"), fs::Permissions::from_mode(0o640)).unwrap();
    fs::create_dir_all(root.join("photos")).unwrap();
    fs::write(root.join("photos/IMG_1.jpg"), vec![9u8; 30_000]).unwrap();
    std::os::unix::fs::symlink("../downloads/notes.txt", root.join("photos/link")).unwrap();
}

fn entry(path: &Path, action: Action, dest: Option<PathBuf>, taken: &mut HashSet<String>) -> Entry {
    let id = entry_id(path, action, taken);
    taken.insert(id.clone());
    let fp = tdy_ops::fingerprint(path, None, false).unwrap();
    Entry {
        id,
        status: Status::Approved,
        approved_by: Some("human".into()),
        action,
        path: path.to_path_buf(),
        dest,
        create_dest: true,
        on_conflict: OnConflict::Skip,
        tags: vec![],
        bytes: Bytes { logical: fp.size, alloc: fp.size, private: fp.size },
        fingerprint: fp,
        source: Source { layer: Layer::User, rule: "test".into(), p: None },
        reason: "test".into(),
        undo: if action == Action::Delete { Undo::None } else { Undo::Restore },
        alternatives: vec![],
        evidence: None,
        outcome: None,
    }
}

fn new_plan(root: &Path, entries: Vec<Entry>) -> Plan {
    let p = Plan {
        header: Header {
            tdy_plan: PLAN_VERSION,
            id: store::new_id(),
            created: tdy_scan::fmt::now_secs(),
            roots: vec![root.to_path_buf()],
            mode: Mode::Manual,
            policy: None,
            policy_hash: None,
            judge: None,
            note: None,
        },
        entries,
    };
    store::save(&p).unwrap();
    p
}

fn no_guard(_: &Entry) -> Result<(), String> {
    Ok(())
}

/// Tests that check the shared trash is left empty, or trash and restore
/// themselves, take turns.
static TRASH: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn apply_then_undo_restores_byte_for_byte() {
    let _t = TRASH.lock().unwrap_or_else(|e| e.into_inner());
    let _ = home();
    let t = tempfile::tempdir_in(home()).unwrap();
    let root = t.path().join("tree");
    fixture(&root);
    let before = manifest(&root);
    let mut taken = HashSet::new();
    let entries = vec![
        entry(&root.join("downloads/Figma.dmg"), Action::Trash, None, &mut taken),
        entry(&root.join("photos/IMG_1.jpg"), Action::Move, Some(root.join("sorted/dogs/IMG_1.jpg")), &mut taken),
        entry(&root.join("downloads/notes.txt"), Action::Quarantine, None, &mut taken),
        entry(&root.join("proj"), Action::Copy, Some(root.join("proj-copy")), &mut taken),
    ];
    let mut plan = new_plan(&root, entries);
    let mut log = Vec::new();
    let r = apply(&mut plan, &ApplyOptions { verify: true, ..Default::default() }, &no_guard, &mut |e| log.push(format!("{e:?}"))).unwrap();
    assert_eq!(r.applied, 4, "{log:#?}");
    assert!(!root.join("downloads/Figma.dmg").exists());
    assert!(root.join("sorted/dogs/IMG_1.jpg").exists());
    assert!(root.join("proj-copy/target/debug/deps/libx.rlib").exists());
    // persisted
    let (reloaded, _) = store::load(Some(&plan.header.id)).unwrap();
    assert!(reloaded.entries.iter().all(|e| e.status == Status::Applied));

    let r = undo(&mut plan, None, &mut |_| {}).unwrap();
    assert_eq!(r.undone, 4);
    // the move created sorted/dogs; undo leaves empty dirs behind — remove them for comparison
    let _ = fs::remove_dir(root.join("sorted/dogs"));
    let _ = fs::remove_dir(root.join("sorted"));
    assert_eq!(manifest(&root), before);
    // trashinfo cleaned up
    let info = home().join("share/Trash/info");
    assert!(fs::read_dir(&info).map(|d| d.count()).unwrap_or(0) == 0);
}

#[test]
fn delete_is_permanent_and_not_undoable() {
    let t = tempfile::tempdir_in(home()).unwrap();
    let root = t.path().join("tree");
    fixture(&root);
    let mut taken = HashSet::new();
    let mut plan = new_plan(&root, vec![entry(&root.join("proj/target"), Action::Delete, None, &mut taken)]);
    let r = apply(&mut plan, &ApplyOptions { verify: true, ..Default::default() }, &no_guard, &mut |_| {}).unwrap();
    assert_eq!(r.applied, 1);
    assert!(r.freed >= 50_000);
    assert!(!root.join("proj/target").exists());
    let r = undo(&mut plan, None, &mut |_| {}).unwrap();
    assert_eq!(r.undone, 0);
    assert_eq!(r.skipped, 1);
}

#[test]
fn stale_and_pending_entries_are_not_applied() {
    let t = tempfile::tempdir_in(home()).unwrap();
    let root = t.path().join("tree");
    fixture(&root);
    let mut taken = HashSet::new();
    let mut e1 = entry(&root.join("proj/target"), Action::Delete, None, &mut taken);
    let e2 = entry(&root.join("downloads/Figma.dmg"), Action::Trash, None, &mut taken);
    e1.status = Status::Approved;
    let mut e3 = entry(&root.join("photos/IMG_1.jpg"), Action::Trash, None, &mut taken);
    e3.status = Status::Pending;
    let mut plan = new_plan(&root, vec![e1, e2, e3]);
    // drift: add a file inside target, rewrite the dmg
    fs::write(root.join("proj/target/new.o"), b"x").unwrap();
    fs::remove_file(root.join("downloads/Figma.dmg")).unwrap();
    fs::write(root.join("downloads/Figma.dmg"), b"different").unwrap();
    let mut skipped = Vec::new();
    let r = apply(&mut plan, &ApplyOptions { verify: true, ..Default::default() }, &no_guard, &mut |e| {
        if let Event::Skip(e, why) = e {
            skipped.push((e.id.clone(), why));
        }
    })
    .unwrap();
    assert_eq!(r.applied, 0);
    assert_eq!(r.skipped, 2, "{skipped:?}");
    assert!(root.join("proj/target").exists());
    assert!(root.join("downloads/Figma.dmg").exists());
    assert!(root.join("photos/IMG_1.jpg").exists());
    assert_eq!(plan.entries[2].status, Status::Pending);
}

#[test]
fn dry_run_changes_nothing() {
    let t = tempfile::tempdir_in(home()).unwrap();
    let root = t.path().join("tree");
    fixture(&root);
    let before = manifest(&root);
    let mut taken = HashSet::new();
    let mut plan = new_plan(
        &root,
        vec![
            entry(&root.join("downloads"), Action::Trash, None, &mut taken),
            entry(&root.join("proj/target"), Action::Delete, None, &mut taken),
        ],
    );
    let r = apply(&mut plan, &ApplyOptions { verify: true, dry_run: true, ..Default::default() }, &no_guard, &mut |_| {}).unwrap();
    assert_eq!(r.would, 2);
    assert_eq!(manifest(&root), before);
    assert!(plan.entries.iter().all(|e| e.status == Status::Approved));
}

#[test]
fn guard_refusal_skips() {
    let t = tempfile::tempdir_in(home()).unwrap();
    let root = t.path().join("tree");
    fixture(&root);
    let mut taken = HashSet::new();
    let mut plan = new_plan(&root, vec![entry(&root.join("downloads/notes.txt"), Action::Trash, None, &mut taken)]);
    let guard = |_: &Entry| -> Result<(), String> { Err("file is open in another process".into()) };
    let r = apply(&mut plan, &ApplyOptions { verify: true, ..Default::default() }, &guard, &mut |_| {}).unwrap();
    assert_eq!(r.skipped, 1);
    assert!(root.join("downloads/notes.txt").exists());
}

#[test]
fn interrupted_apply_is_recovered() {
    let t = tempfile::tempdir_in(home()).unwrap();
    let root = t.path().join("tree");
    fixture(&root);
    let mut taken = HashSet::new();
    let a = entry(&root.join("downloads/Figma.dmg"), Action::Quarantine, None, &mut taken);
    let b = entry(&root.join("photos/IMG_1.jpg"), Action::Quarantine, None, &mut taken);
    let c = entry(&root.join("proj/target"), Action::Delete, None, &mut taken);
    let mut plan = new_plan(&root, vec![a, b, c]);
    let id = plan.header.id.clone();
    // Simulate a crash: entry a fully renamed but only `begin` journalled;
    // entry b journalled `begin` but rename never happened; entry c delete
    // began and the tree is gone.
    let qa = tdy_ops::trash::quarantine_slot(&plan.entries[0].path, &id, &plan.entries[0].id, true).unwrap();
    let qb = tdy_ops::trash::quarantine_slot(&plan.entries[1].path, &id, &plan.entries[1].id, true).unwrap();
    {
        let mut j = Journal::open(&id).unwrap();
        j.log(&plan.entries[0].id, Phase::Begin, "quarantine", &plan.entries[0].path, Some(&qa), None).unwrap();
        fs::rename(&plan.entries[0].path, &qa).unwrap();
        j.log(&plan.entries[1].id, Phase::Begin, "quarantine", &plan.entries[1].path, Some(&qb), None).unwrap();
        j.log(&plan.entries[2].id, Phase::Begin, "delete", &plan.entries[2].path, None, None).unwrap();
        tdy_ops::fsops::remove_tree(&plan.entries[2].path).unwrap();
    }
    let mut notes = Vec::new();
    let r = apply(&mut plan, &ApplyOptions { verify: true, ..Default::default() }, &no_guard, &mut |e| {
        if let Event::Note(n) = e {
            notes.push(n);
        }
    })
    .unwrap();
    assert_eq!(notes.len(), 3, "{notes:?}");
    assert_eq!(plan.entries[0].status, Status::Applied);
    assert_eq!(plan.entries[0].outcome.as_ref().unwrap().now_at.as_deref(), Some(qa.as_path()));
    // b was reset to approved and then applied in the same run
    assert_eq!(plan.entries[1].status, Status::Applied);
    assert_eq!(r.applied, 1);
    assert_eq!(plan.entries[2].status, Status::Applied);
    // and everything that can be undone, is
    let r = undo(&mut plan, None, &mut |_| {}).unwrap();
    assert_eq!(r.undone, 2);
    assert!(root.join("downloads/Figma.dmg").exists() && root.join("photos/IMG_1.jpg").exists());
}

#[test]
fn editor_roundtrip() {
    let t = tempfile::tempdir_in(home()).unwrap();
    let root = t.path().join("tree");
    fixture(&root);
    let mut taken = HashSet::new();
    let mut plan = new_plan(
        &root,
        vec![
            entry(&root.join("downloads/Figma.dmg"), Action::Trash, None, &mut taken),
            entry(&root.join("photos/IMG_1.jpg"), Action::Trash, None, &mut taken),
            entry(&root.join("proj/target"), Action::Delete, None, &mut taken),
        ],
    );
    for e in plan.entries.iter_mut() {
        e.status = Status::Pending;
    }
    let text = tdy_plan::review::render_editor(&plan);
    let lines: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
    assert_eq!(lines.len(), 3);
    // approve first, reject second explicitly, delete third line
    let edited = format!("{}\n{}\n", lines[0].replacen("pending", "a", 1), lines[1].replacen("pending", "reject", 1));
    let (a, r, p) = tdy_plan::review::parse_editor(&mut plan, &edited).unwrap();
    assert_eq!((a, r, p), (1, 2, 0));
    assert_eq!(plan.entries[0].status, Status::Approved);
    assert_eq!(plan.entries[2].status, Status::Rejected);
    assert!(tdy_plan::review::parse_editor(&mut plan, "frobnicate abcd\n").is_err());
}

#[test]
fn undo_of_copy_refuses_when_copy_changed() {
    let t = tempfile::tempdir_in(home()).unwrap();
    let root = t.path().join("tree");
    fixture(&root);
    let mut taken = HashSet::new();
    let mut plan = new_plan(&root, vec![entry(&root.join("photos"), Action::Copy, Some(root.join("backup/photos")), &mut taken)]);
    let r = apply(&mut plan, &ApplyOptions { verify: true, ..Default::default() }, &no_guard, &mut |_| {}).unwrap();
    assert_eq!(r.applied, 1);
    fs::write(root.join("backup/photos/new-work.txt"), b"precious").unwrap();
    let r = undo(&mut plan, None, &mut |_| {}).unwrap();
    assert_eq!(r.undone, 0);
    assert_eq!(r.failed, 1);
    assert!(root.join("backup/photos/new-work.txt").exists());
}

#[test]
fn interrupted_cross_volume_move_with_missing_destination_keeps_source() {
    let t = tempfile::tempdir_in(home()).unwrap();
    let root = t.path().join("tree");
    fixture(&root);
    let mut taken = HashSet::new();
    let src = root.join("downloads/Figma.dmg");
    let mut plan = new_plan(&root, vec![entry(&src, Action::Move, Some(root.join("elsewhere/Figma.dmg")), &mut taken)]);
    let id = plan.header.id.clone();
    {
        let mut j = Journal::open(&id).unwrap();
        let eid = plan.entries[0].id.clone();
        j.log(&eid, Phase::Begin, "move", &src, Some(&root.join("elsewhere/Figma.dmg")), None).unwrap();
        j.log(&eid, Phase::Copied, "move", &src, Some(&root.join("elsewhere/Figma.dmg")), None).unwrap();
        // destination vanished (unmounted disk) before the source was removed
    }
    let notes = tdy_plan::recover(&mut plan).unwrap();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(src.exists(), "source must be kept");
    assert_eq!(plan.entries[0].status, Status::Failed);
}

#[test]
fn dry_run_does_not_recover() {
    let t = tempfile::tempdir_in(home()).unwrap();
    let root = t.path().join("tree");
    fixture(&root);
    let mut taken = HashSet::new();
    let mut plan = new_plan(&root, vec![entry(&root.join("proj/target"), Action::Delete, None, &mut taken)]);
    let id = plan.header.id.clone();
    Journal::open(&id).unwrap().log(&plan.entries[0].id, Phase::Begin, "delete", &root.join("proj/target"), None, None).unwrap();
    let mut notes = Vec::new();
    apply(&mut plan, &ApplyOptions { verify: true, dry_run: true, ..Default::default() }, &no_guard, &mut |e| {
        if let Event::Note(n) = e {
            notes.push(n);
        }
    })
    .unwrap();
    assert!(notes[0].contains("interrupted"));
    assert_eq!(plan.entries[0].status, Status::Approved);
    assert!(root.join("proj/target").exists());
}

/// The cached index equals a fresh scan, folder by folder.
fn cache_matches_disk(root: &Path, ctx: &str) {
    use tdy_scan::{ScanOptions, cache, scan};
    let opts = ScanOptions { top_k: 0, ..Default::default() };
    let got = cache::load(root, &opts).expect("cached").index;
    let want = scan(root, &opts).unwrap();
    assert_eq!(got.nodes.len(), want.nodes.len(), "{ctx}: folder count");
    for i in 0..want.nodes.len() as u32 {
        let p = want.path(i);
        let j = got.lookup(&p).unwrap_or_else(|| panic!("{ctx}: {} missing", p.display()));
        let (a, b) = (&got.nodes[j as usize], &want.nodes[i as usize]);
        assert_eq!(
            (a.files, a.dirs, a.logical, a.alloc, a.flags),
            (b.files, b.dirs, b.logical, b.alloc, b.flags),
            "{ctx}: {}",
            p.display()
        );
    }
}

#[test]
fn cached_index_follows_apply_and_undo() {
    use tdy_plan::sync::{self, Op};
    let _t = TRASH.lock().unwrap_or_else(|e| e.into_inner());
    let _ = home();
    let t = tempfile::tempdir_in(home()).unwrap();
    let root = fs::canonicalize(t.path()).unwrap().join("tree");
    fixture(&root);
    tdy_scan::cache::store(&tdy_scan::scan(&root, &Default::default()).unwrap()).unwrap();
    let mut taken = HashSet::new();
    // (the copy goes first: moving a photo out changes the folder it copies)
    let entries = vec![
        entry(&root.join("photos"), Action::Copy, Some(root.join("photos-copy")), &mut taken),
        entry(&root.join("downloads/Figma.dmg"), Action::Trash, None, &mut taken),
        entry(&root.join("proj/target"), Action::Trash, None, &mut taken),
        entry(&root.join("photos/IMG_1.jpg"), Action::Move, Some(root.join("sorted/dogs/IMG_1.jpg")), &mut taken),
        entry(&root.join("downloads/notes.txt"), Action::Quarantine, None, &mut taken),
    ];
    let mut plan = new_plan(&root, entries);
    let tr = sync::watch(&plan, Op::Apply, None);
    let r = apply(&mut plan, &ApplyOptions { verify: true, ..Default::default() }, &no_guard, &mut |_| {}).unwrap();
    assert_eq!(r.applied, 5);
    let s = tr.finish(&plan);
    assert_eq!((s.patched, s.dropped), (1, 0));
    cache_matches_disk(&root, "after apply");

    let tr = sync::watch(&plan, Op::Undo, None);
    assert_eq!(undo(&mut plan, None, &mut |_| {}).unwrap().undone, 5);
    assert_eq!(tr.finish(&plan).patched, 1);
    cache_matches_disk(&root, "after undo");
}
