//! Index cache benchmark: how long a cached start and a patch after a
//! change take, against a full rescan of the same tree.
//!
//! `tidy-cachebench <tree> [folders-to-move]` moves that many folders out
//! of the tree and back (like trash and undo), patching the index each
//! time, and checks the patched totals against a rescan. The tree is
//! restored afterwards.

use std::path::{Path, PathBuf};
use std::time::Instant;
use tidy_scan::{Index, ScanOptions, Watch, cache, scan};

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn check(idx: &Index, root: &Path, what: &str) {
    let fresh = scan(root, &ScanOptions { top_k: 0, ..Default::default() }).unwrap();
    let (a, b) = (idx.root_node(), fresh.root_node());
    assert_eq!((a.files, a.dirs, a.alloc, a.logical), (b.files, b.dirs, b.alloc, b.logical), "{what}: patched totals differ from a rescan");
    assert_eq!(idx.nodes.len(), fresh.nodes.len(), "{what}: folder count");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = std::fs::canonicalize(args.get(1).expect("usage: tidy-cachebench <tree> [folders]")).unwrap();
    let n: usize = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(100);
    let aside = root.with_file_name(format!("{}-aside", root.file_name().unwrap().to_string_lossy()));
    std::fs::create_dir_all(&aside).unwrap();
    // SAFETY: single-threaded at this point.
    unsafe { std::env::set_var("TIDY_CACHE_DIR", aside.join(".cache")) };
    let opts = ScanOptions { top_k: 64, ..Default::default() };

    let t = Instant::now();
    let idx = scan(&root, &opts).unwrap();
    let t_scan = ms(t);
    let t = Instant::now();
    cache::store(&idx).unwrap();
    let t_store = ms(t);
    let t = Instant::now();
    let c = cache::load(&root, &ScanOptions { top_k: 0, ..Default::default() }).unwrap();
    let t_load = ms(t);
    // a folder about a tenth of the tree, cut out of the cached index
    let big = (1..idx.nodes.len() as u32)
        .filter(|&i| idx.nodes[i as usize].files * 10 <= idx.root_node().files)
        .max_by_key(|&i| idx.nodes[i as usize].files)
        .unwrap();
    let sub = idx.path(big);
    let t = Instant::now();
    let s = cache::load(&sub, &ScanOptions { top_k: 0, ..Default::default() }).unwrap();
    let t_cut = ms(t);
    let t = Instant::now();
    let _ = scan(&sub, &opts).unwrap();
    let t_sub_scan = ms(t);

    // n folders at depth 2–4, spread over the tree, none inside another
    let eligible: Vec<u32> = (1..idx.nodes.len() as u32).filter(|&i| (3..=5).contains(&idx.depth(i))).collect();
    let mut picked: Vec<PathBuf> = vec![];
    let step = (eligible.len() / (n * 2)).max(1);
    for &i in eligible.iter().step_by(step) {
        let p = idx.path(i);
        if !picked.iter().any(|q| p.starts_with(q) || q.starts_with(&p)) {
            picked.push(p);
        }
        if picked.len() == n {
            break;
        }
    }
    let moved_files: u64 = picked.iter().map(|p| idx.nodes[idx.lookup(p).unwrap() as usize].files).sum();
    let mut index = c.index;

    // "trash": move them aside
    let t = Instant::now();
    let mut w = Watch::new(false);
    picked.iter().for_each(|p| w.add(p));
    let rec = w.start();
    let t_before = ms(t);
    let dests: Vec<PathBuf> = (0..picked.len()).map(|i| aside.join(format!("{i}"))).collect();
    for (p, d) in picked.iter().zip(&dests) {
        std::fs::rename(p, d).unwrap();
    }
    let t = Instant::now();
    let ch = rec.finish(dests.clone());
    let r = index.patch(&ch).unwrap();
    let t_patch_out = ms(t) + t_before;
    check(&index, &root, "after moving out");

    // "undo": move them back (the folders are scanned and grafted)
    let t = Instant::now();
    let mut w = Watch::new(false);
    picked.iter().chain(&dests).for_each(|p| w.add(p));
    let rec = w.start();
    let t_before = ms(t);
    for (p, d) in picked.iter().zip(&dests) {
        std::fs::rename(d, p).unwrap();
    }
    let t = Instant::now();
    let ch = rec.finish([]);
    let r2 = index.patch(&ch).unwrap();
    let t_patch_back = ms(t) + t_before;
    check(&index, &root, "after moving back");
    let t = Instant::now();
    let _ = scan(&root, &opts).unwrap();
    let t_rescan = ms(t);
    let _ = std::fs::remove_dir_all(&aside);

    println!("tree: {} files in {} folders; cut-out folder: {} files", idx.stats.files, idx.stats.dirs, s.index.stats.files);
    println!("{:<44} {:>10}", "", "ms");
    println!("{:<44} {:>10.1}", "full scan", t_scan);
    println!("{:<44} {:>10.1}", "  save to the cache", t_store);
    println!("{:<44} {:>10.1}", "start from the cache", t_load);
    println!("{:<44} {:>10.1}", "subfolder: scan", t_sub_scan);
    println!("{:<44} {:>10.1}", "subfolder: cut out of the cached index", t_cut);
    println!("{:<44} {:>10.1}", format!("patch: {} folders moved out ({} files)", picked.len(), moved_files), t_patch_out);
    println!("{:<44} {:>10.1}", "patch: moved back (scanned and grafted)", t_patch_back);
    println!("{:<44} {:>10.1}", "rescan instead", t_rescan);
    eprintln!("{r:?} {r2:?}");
}
