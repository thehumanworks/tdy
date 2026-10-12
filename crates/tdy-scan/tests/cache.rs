//! The index cache and patching: whatever comes out of the cache, or out of
//! a patch, must equal a fresh scan of the same folder.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tdy_scan::cache;
use tdy_scan::{Index, ScanOptions, Watch, scan};

/// TDY_CACHE_DIR is process-wide.
static ENV: Mutex<()> = Mutex::new(());

fn cache_env(t: &Path) -> std::sync::MutexGuard<'static, ()> {
    let g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: serialised by ENV within this test binary.
    unsafe { std::env::set_var("TDY_CACHE_DIR", t.join("cache")) };
    g
}

fn write(p: &Path, n: usize) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, vec![b'x'; n]).unwrap();
}

/// A tree with sizes that make the largest files distinct.
fn fixture(root: &Path) {
    let mut n = 1000;
    for d in ["a/b/c", "a/m/deep", "c", "d/x/inner", "e", "f/g"] {
        for i in 0..4 {
            n += 777;
            write(&root.join(d).join(format!("file{i}.bin")), n);
        }
    }
    write(&root.join("top.bin"), 123_456);
    write(&root.join("Cargo.toml"), 10);
    fs::create_dir_all(root.join("empty")).unwrap();
}

/// Ranked by apparent size: allocated sizes round to blocks and tie.
fn opts(top_k: usize) -> ScanOptions {
    ScanOptions { top_k, threads: 2, top_by: tdy_scan::SizeKind::Logical, ..Default::default() }
}

fn canon(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap()
}

/// Every folder of `want` is in `got` with the same totals, and nothing else.
fn same(got: &Index, want: &Index, ctx: &str) {
    assert_eq!(got.root, want.root, "{ctx}: root");
    assert_eq!(got.nodes.len(), want.nodes.len(), "{ctx}: folder count");
    for i in 0..want.nodes.len() as u32 {
        let p = want.path(i);
        let j = got.lookup(&p).unwrap_or_else(|| panic!("{ctx}: {} missing", p.display()));
        let (a, b) = (&got.nodes[j as usize], &want.nodes[i as usize]);
        let show = |n: &tdy_scan::DirNode| (n.files, n.dirs, n.logical, n.alloc, n.private, n.own_alloc, n.flags);
        assert_eq!(show(a), show(b), "{ctx}: {} (files, dirs, logical, alloc, private, own_alloc, flags)", p.display());
        assert!(a.mtime_max >= b.mtime_max, "{ctx}: {} newest change went back", p.display());
    }
    assert_eq!((got.stats.files, got.stats.dirs), (want.stats.files, want.stats.dirs), "{ctx}: stats");
}

fn top(idx: &Index) -> Vec<(PathBuf, u64)> {
    idx.top_files.iter().map(|f| (idx.file_path(f), f.logical)).collect()
}

#[test]
fn extract_matches_a_fresh_scan() {
    let t = tempfile::tempdir().unwrap();
    let root = canon(t.path());
    fixture(&root);
    let full = scan(&root, &opts(6)).unwrap();
    for sub in ["a", "a/b", "d/x", "empty"] {
        let p = root.join(sub);
        let cut = full.extract(full.lookup(&p).unwrap());
        cut.validate().unwrap();
        let fresh = scan(&p, &opts(6)).unwrap();
        same(&cut, &fresh, sub);
        // the cut list is a true top list as far as it goes
        let k = cut.stats.top_k;
        assert_eq!(top(&cut)[..k.min(cut.top_files.len())], top(&fresh)[..k.min(cut.top_files.len())], "{sub}");
    }
}

#[test]
fn cache_round_trip_and_choice() {
    let t = tempfile::tempdir().unwrap();
    let _g = cache_env(t.path());
    let root = canon(&t.path().join("tree").tap_mkdir());
    fixture(&root);
    assert!(cache::load(&root, &opts(0)).is_none(), "empty cache");

    let full = scan(&root, &opts(8)).unwrap();
    cache::store(&full).unwrap();
    let c = cache::load(&root, &opts(8)).expect("cached");
    assert_eq!(c.from, root);
    assert_eq!(c.index.stats.backend, "cache");
    assert_eq!(c.index.stats.started_ms, full.stats.started_ms);
    same(&c.index, &full, "exact");
    assert_eq!(top(&c.index), top(&full));

    // a folder inside comes out of the parent's index
    let sub = root.join("a");
    let c = cache::load(&sub, &opts(0)).expect("cut from the parent");
    assert_eq!(c.from, root);
    same(&c.index, &scan(&sub, &opts(0)).unwrap(), "cut");

    // options the cached scan can't answer
    assert!(cache::load(&root, &ScanOptions { cross_mounts: true, ..opts(0) }).is_none());
    assert!(cache::load(&root, &opts(9)).is_none(), "8 largest files cannot answer 9");
    if !full.stats.private_measured {
        assert!(cache::load(&root, &ScanOptions { private: true, ..opts(0) }).is_none());
    }

    // a newer scan of the folder itself wins over the parent's
    write(&sub.join("later.bin"), 5000);
    let newer = scan(&sub, &opts(8)).unwrap();
    cache::store(&newer).unwrap();
    let c = cache::load(&sub, &opts(0)).unwrap();
    assert_eq!(c.from, sub);
    same(&c.index, &newer, "newer");

    // a corrupt file is ignored, not trusted
    fs::write(cache::path_for(&sub), b"TIDYIDX\x01garbage").unwrap();
    assert_eq!(cache::load(&sub, &opts(0)).unwrap().from, root);
}

trait Mk {
    fn tap_mkdir(self) -> Self;
}
impl Mk for PathBuf {
    fn tap_mkdir(self) -> Self {
        fs::create_dir_all(&self).unwrap();
        self
    }
}

#[test]
fn patch_matches_a_rescan() {
    let t = tempfile::tempdir().unwrap();
    let root = canon(t.path());
    fixture(&root);
    fs::create_dir_all(root.join("bin")).unwrap();
    let mut idx = scan(&root, &opts(16)).unwrap();

    let mut w = Watch::new(false);
    for p in ["a/b", "c/file1.bin", "c/new.bin", "d/x", "d/y", "e/n1/n2/file", "a/m", "Cargo.toml"] {
        w.add(&root.join(p));
    }
    let rec = w.start();
    fs::remove_dir_all(root.join("a/b")).unwrap();
    fs::remove_file(root.join("c/file1.bin")).unwrap();
    write(&root.join("c/new.bin"), 9999);
    fs::rename(root.join("d/x"), root.join("d/y")).unwrap();
    write(&root.join("e/n1/n2/file"), 4321);
    fs::rename(root.join("a/m"), root.join("bin/m")).unwrap();
    fs::remove_file(root.join("Cargo.toml")).unwrap();
    let changes = rec.finish([root.join("bin/m")]);

    let r = idx.patch(&changes).unwrap();
    assert_eq!((r.removed, r.grafted), (3, 3), "a/b, d/x, a/m gone; d/y, e/n1, bin/m new: {r:?}");
    idx.validate().unwrap();
    same(&idx, &scan(&root, &opts(16)).unwrap(), "patched");
    assert_eq!(idx.stats.top_k, 0, "files changed: the largest-files list is dropped");
    // comparing up-to-date folders with the index changes nothing
    let again = idx.patch(&rec_again(&root)).unwrap();
    assert_eq!(again, Default::default());
    same(&idx, &scan(&root, &opts(16)).unwrap(), "patched twice");
}

/// Compare-with-the-index observations of a few folders, as after a change.
fn rec_again(root: &Path) -> tdy_scan::Changes {
    Watch::new(false).start().finish([root.join("c/x"), root.join("d/y/x")])
}

#[test]
fn folder_removals_and_restores_keep_the_largest_files_list() {
    let t = tempfile::tempdir().unwrap();
    let root = canon(t.path());
    fixture(&root);
    let mut idx = scan(&root, &opts(5)).unwrap();
    let trash = root.join("trash");
    fs::create_dir_all(&trash).unwrap();
    // trash a folder that holds some of the largest files
    let mut w = Watch::new(false);
    w.add(&root.join("f/g"));
    let rec = w.start();
    fs::rename(root.join("f/g"), trash.join("g")).unwrap();
    let ch = rec.finish([trash.join("g")]);
    idx.patch(&ch).unwrap();
    let fresh = scan(&root, &opts(5)).unwrap();
    same(&idx, &fresh, "after trash");
    assert!(idx.stats.top_k > 0, "no file changed outside the moved folders");
    let k = idx.stats.top_k;
    assert_eq!(top(&idx)[..k], top(&fresh)[..k]);

    // and back again: the restored folder is scanned and grafted
    let mut w = Watch::new(false);
    w.add(&root.join("f/g"));
    w.add(&trash.join("g"));
    let rec = w.start();
    fs::rename(trash.join("g"), root.join("f/g")).unwrap();
    idx.patch(&rec.finish([])).unwrap();
    let fresh = scan(&root, &opts(idx.stats.top_k)).unwrap();
    same(&idx, &fresh, "after restore");
    assert_eq!(top(&idx), top(&fresh));
}

#[test]
fn sync_patches_the_cache() {
    let t = tempfile::tempdir().unwrap();
    let _g = cache_env(t.path());
    let root = canon(&t.path().join("tree").tap_mkdir());
    fixture(&root);
    cache::store(&scan(&root, &opts(8)).unwrap()).unwrap();
    cache::store(&scan(&root.join("d/x"), &opts(8)).unwrap()).unwrap();
    let gone = cache::path_for(&root.join("d/x"));
    assert!(gone.exists());

    let mut w = Watch::new(false);
    w.add(&root.join("a/b"));
    w.add(&root.join("d/x"));
    let rec = w.start();
    fs::remove_dir_all(root.join("a/b")).unwrap();
    fs::remove_dir_all(root.join("d/x")).unwrap();
    let ch = rec.finish([]);
    // scanned after the change: it already shows the change, and patching
    // it again would count the change twice
    std::thread::sleep(std::time::Duration::from_millis(3));
    cache::store(&scan(&root.join("a"), &opts(8)).unwrap()).unwrap();
    let s = cache::sync(&ch);
    assert_eq!((s.patched, s.dropped), (1, 1), "{s:?}");
    assert!(!gone.exists(), "the removed folder's index is dropped");
    for (p, ctx) in [(root.clone(), "root"), (root.join("a"), "a")] {
        let c = cache::load(&p, &opts(0)).unwrap();
        assert_eq!(c.from, p, "{ctx}");
        same(&c.index, &scan(&p, &opts(0)).unwrap(), ctx);
    }
}

#[test]
fn a_scan_during_the_change_is_dropped() {
    let t = tempfile::tempdir().unwrap();
    let _g = cache_env(t.path());
    let root = canon(&t.path().join("tree").tap_mkdir());
    fixture(&root);
    let mut w = Watch::new(false);
    w.add(&root.join("c/file1.bin"));
    let rec = w.start();
    // started after watching began, finished before the change was over
    cache::store(&scan(&root, &opts(8)).unwrap()).unwrap();
    fs::remove_file(root.join("c/file1.bin")).unwrap();
    let s = cache::sync(&rec.finish([]));
    assert_eq!((s.patched, s.dropped), (0, 1), "{s:?}");
    assert!(cache::load(&root, &opts(0)).is_none());
}

#[test]
fn folder_rewritten_in_place_is_rescanned() {
    let t = tempfile::tempdir().unwrap();
    let root = canon(t.path());
    fixture(&root);
    let mut idx = scan(&root, &opts(8)).unwrap();
    let mut w = Watch::new(false);
    w.rescan(&root.join("a"));
    let rec = w.start();
    // like compression: same name, new contents
    fs::rename(root.join("a"), root.join("a.old")).unwrap();
    write(&root.join("a/packed.bin"), 10);
    fs::remove_dir_all(root.join("a.old")).unwrap();
    let r = idx.patch(&rec.finish([])).unwrap();
    assert_eq!((r.removed, r.grafted), (1, 1));
    same(&idx, &scan(&root, &opts(8)).unwrap(), "rewritten");
    // the root itself rewritten: only a scan will do
    let mut w = Watch::new(false);
    w.rescan(&root);
    assert!(idx.patch(&w.start().finish([])).is_none());
}

#[test]
fn a_folder_the_index_fell_behind_on_is_resynced() {
    let t = tempfile::tempdir().unwrap();
    let root = canon(t.path());
    fixture(&root);
    let mut idx = scan(&root, &opts(8)).unwrap();
    // after the scan: a new download, and a new folder
    write(&root.join("c/download.bin"), 104_000);
    fs::create_dir_all(root.join("c/new/deeper")).unwrap();
    // then a change that trashes the download
    let mut w = Watch::new(false);
    w.add(&root.join("c/download.bin"));
    let rec = w.start();
    fs::rename(root.join("c/download.bin"), root.join("e/download.bin")).unwrap();
    idx.patch(&rec.finish([root.join("e/download.bin")])).unwrap();
    same(&idx, &scan(&root, &opts(8)).unwrap(), "fell behind");
}

#[test]
fn landing_in_folders_the_change_created() {
    let t = tempfile::tempdir().unwrap();
    let root = canon(t.path());
    fixture(&root);
    fs::create_dir_all(root.join("q")).unwrap();
    let mut idx = scan(&root, &opts(8)).unwrap();
    let mut w = Watch::new(false).within(vec![root.clone()]);
    w.add(&root.join("c/file1.bin"));
    let rec = w.start();
    // like quarantine: <q>/<plan>/<entry>/<name>, both folders new
    let to = root.join("q/plan/entry/file1.bin");
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    fs::rename(root.join("c/file1.bin"), &to).unwrap();
    let r = idx.patch(&rec.finish([to])).unwrap();
    assert_eq!(r.grafted, 1, "q/plan, with everything in it");
    same(&idx, &scan(&root, &opts(8)).unwrap(), "quarantined");
}

#[test]
fn hard_links_make_the_patch_refuse() {
    let t = tempfile::tempdir().unwrap();
    let root = canon(t.path());
    fixture(&root);
    fs::hard_link(root.join("c/file1.bin"), root.join("e/link.bin")).unwrap();
    let mut idx = scan(&root, &opts(8)).unwrap();
    let mut w = Watch::new(false);
    w.add(&root.join("c/file1.bin"));
    let rec = w.start();
    fs::remove_file(root.join("c/file1.bin")).unwrap();
    let before = (idx.nodes.len(), idx.root_node().alloc);
    assert!(idx.patch(&rec.finish([])).is_none(), "where the other link counts is a scan's call");
    assert_eq!((idx.nodes.len(), idx.root_node().alloc), before, "a refused patch changes nothing");

    // a restored folder holding a hard link can't be grafted either
    fs::hard_link(root.join("c/file2.bin"), root.join("e/link2.bin")).unwrap();
    let mut idx = scan(&root, &opts(8)).unwrap();
    let mut w = Watch::new(false);
    w.add(&root.join("e"));
    let rec = w.start();
    fs::rename(root.join("e"), root.join("e2")).unwrap();
    assert!(idx.patch(&rec.finish([root.join("e2")])).is_none());
}

#[test]
fn a_folder_replaced_under_the_same_name_is_rescanned() {
    let t = tempfile::tempdir().unwrap();
    let root = canon(t.path());
    fixture(&root);
    let mut idx = scan(&root, &opts(8)).unwrap();
    let mut w = Watch::new(false);
    w.add(&root.join("d/x"));
    w.add(&root.join("e"));
    let rec = w.start();
    // trash d/x, then move e into its place
    fs::rename(root.join("d/x"), root.join("bin")).unwrap();
    fs::rename(root.join("e"), root.join("d/x")).unwrap();
    idx.patch(&rec.finish([root.join("bin")])).unwrap();
    same(&idx, &scan(&root, &opts(8)).unwrap(), "replaced");
}
