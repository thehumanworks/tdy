use std::fs;
use std::io::Write;
use std::path::Path;
use tidy_scan::{ScanOptions, SizeKind, scan};

fn write(p: &Path, n: usize) {
    let mut f = fs::File::create(p).unwrap();
    f.write_all(&vec![b'x'; n]).unwrap();
}

/// Reference totals using std::fs (no hardlink dedupe across the tree unless tracked).
fn reference(root: &Path) -> (u64, u64, u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let mut files = 0;
    let mut dirs = 0;
    let mut logical = 0;
    let mut alloc = 0;
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            let md = fs::symlink_metadata(e.path()).unwrap();
            if md.is_dir() {
                dirs += 1;
                logical += md.len();
                alloc += md.blocks() * 512;
                stack.push(e.path());
            } else {
                if md.nlink() > 1 && md.is_file() && !seen.insert((md.dev(), md.ino())) {
                    continue;
                }
                files += 1;
                logical += md.len();
                alloc += md.blocks() * 512;
            }
        }
    }
    (files, dirs, logical, alloc)
}

fn fixture() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    fs::create_dir_all(r.join("a/b/c")).unwrap();
    fs::create_dir_all(r.join("a/d")).unwrap();
    fs::create_dir_all(r.join("e")).unwrap();
    write(&r.join("a/one.bin"), 10_000);
    write(&r.join("a/b/two.bin"), 200_000);
    write(&r.join("a/b/c/three.bin"), 3_000_000);
    write(&r.join("a/d/empty"), 0);
    write(&r.join("e/big.bin"), 5_000_000);
    fs::hard_link(r.join("e/big.bin"), r.join("a/d/big-link.bin")).unwrap();
    std::os::unix::fs::symlink(r.join("e"), r.join("a/link-to-e")).unwrap();
    // non-UTF-8 name
    {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
        write(&r.join("e").join(name), 123);
    }
    t
}

#[test]
fn totals_match_reference() {
    let t = fixture();
    for threads in [1, 2, 8] {
        let idx = scan(t.path(), &ScanOptions { threads, ..Default::default() }).unwrap();
        let (files, dirs, logical, alloc) = reference(&fs::canonicalize(t.path()).unwrap());
        let root = idx.root_node();
        assert_eq!(root.files, files, "files (threads={threads})");
        assert_eq!(root.dirs as u64, dirs, "dirs");
        assert_eq!(root.logical, logical, "logical");
        assert_eq!(root.alloc, alloc, "alloc");
        assert_eq!(idx.stats.hardlinks_deduped, 1);
        assert_eq!(idx.stats.symlinks, 1);
        assert_eq!(idx.nodes.len() as u64, dirs + 1);
    }
}

#[test]
fn matches_du_allocated() {
    let t = fixture();
    let idx = scan(t.path(), &ScanOptions::default()).unwrap();
    let out = std::process::Command::new("du").arg("-sB1").arg(&idx.root).output();
    let Ok(out) = out else { return };
    if !out.status.success() {
        return;
    }
    let du: u64 = String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap().parse().unwrap();
    use std::os::unix::fs::MetadataExt;
    let root_own = fs::metadata(&idx.root).unwrap().blocks() * 512;
    assert_eq!(idx.root_node().alloc + root_own, du);
}

#[test]
fn paths_lookup_children_and_top_files() {
    let t = fixture();
    let idx = scan(t.path(), &ScanOptions { top_k: 3, ..Default::default() }).unwrap();
    // every dir path round-trips through lookup
    for id in 0..idx.nodes.len() as u32 {
        let p = idx.path(id);
        assert!(p.is_dir(), "{}", p.display());
        assert_eq!(idx.lookup(&p), Some(id));
    }
    let names: Vec<String> = idx.top_files.iter().map(|f| String::from_utf8_lossy(&f.name).into_owned()).collect();
    assert_eq!(idx.top_files.len(), 3);
    assert!(names[0] == "big.bin" || names[0] == "big-link.bin");
    assert_eq!(names[1], "three.bin");
    assert_eq!(names[2], "two.bin");
    for f in &idx.top_files {
        assert!(idx.file_path(f).exists());
    }
    // tree contains root first and is depth-consistent
    let rows = idx.tree(0, 10, SizeKind::Alloc, None);
    assert_eq!(rows[0].depth, 0);
    assert!(rows.iter().any(|r| r.name == "three.bin"));
    let a = idx.lookup(&idx.root.join("a")).unwrap();
    let sub = idx.tree(a, 10, SizeKind::Logical, None);
    assert!(sub.iter().all(|r| r.path.starts_with(idx.root.join("a"))));
}

#[test]
fn snapshot_roundtrip_and_diff() {
    let t = fixture();
    let idx = scan(t.path(), &ScanOptions::default()).unwrap();
    let snap = t.path().join("snap.idx");
    tidy_scan::snapshot::save(&idx, &snap).unwrap();
    let loaded = tidy_scan::snapshot::load(&snap).unwrap().index;
    assert_eq!(loaded.nodes.len(), idx.nodes.len());
    assert_eq!(loaded.root, idx.root);
    for i in 0..idx.nodes.len() as u32 {
        assert_eq!(loaded.path(i), idx.path(i));
        assert_eq!(loaded.nodes[i as usize].alloc, idx.nodes[i as usize].alloc);
    }
    fs::remove_file(&snap).unwrap();
    write(&t.path().join("a/b/new.bin"), 1_000_000);
    let idx2 = scan(t.path(), &ScanOptions::default()).unwrap();
    let d = tidy_scan::snapshot::diff(&loaded, &idx2, SizeKind::Logical, 500_000);
    assert!(d.iter().any(|(p, delta)| p.ends_with("a/b") && *delta >= 1_000_000));
}

#[test]
fn file_root_is_rejected() {
    let t = fixture();
    let e = scan(&t.path().join("a/one.bin"), &ScanOptions::default());
    assert!(e.is_err());
}
