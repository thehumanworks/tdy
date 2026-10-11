//! Generate a deterministic directory tree for benchmarks.
//!
//! Shape: a random recursive tree of `files / ratio` directories (each new
//! directory picks a uniformly random existing parent, giving realistic
//! log-depth with long tails), files assigned to random directories, and a
//! size mix of mostly-empty, small and sparse-large files so that a
//! million-file tree needs only ~1 GB of real disk.

use anyhow::Result;
use clap::Parser;
use rayon::prelude::*;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    /// Output directory (created; must not exist unless --force).
    #[arg(long)]
    out: PathBuf,
    /// Number of files.
    #[arg(long, default_value_t = 1_000_000)]
    files: usize,
    /// Files per directory on average.
    #[arg(long, default_value_t = 10)]
    ratio: usize,
    #[arg(long, default_value_t = 42)]
    seed: u64,
    /// Overwrite an existing output directory.
    #[arg(long)]
    force: bool,
    /// Write no data at all (all files empty) — for inode-heavy memory tests.
    #[arg(long)]
    empty: bool,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

const WORDS: &[&str] = &[
    "src",
    "lib",
    "assets",
    "photos",
    "2023",
    "2024",
    "2025",
    "docs",
    "build",
    "cache",
    "node_modules",
    "vendor",
    "tmp",
    "data",
    "music",
    "projects",
    "notes",
    "archive",
    "backup",
    "images",
    "video",
    "drafts",
    "config",
    "test",
    "fixtures",
];
const EXTS: &[&str] = &["txt", "jpg", "png", "rs", "js", "json", "md", "pdf", "mp4", "heic", "o", "zip", "csv", "py"];

fn main() -> Result<()> {
    let a = Args::parse();
    if a.out.exists() {
        anyhow::ensure!(a.force, "{} exists (use --force)", a.out.display());
        fs::remove_dir_all(&a.out)?;
    }
    let ndirs = (a.files / a.ratio.max(1)).max(1);
    let mut rng = Rng(a.seed);
    // Directory tree: parent index and relative path.
    let mut paths: Vec<PathBuf> = Vec::with_capacity(ndirs);
    paths.push(a.out.clone());
    for i in 1..ndirs {
        let parent = rng.below(i as u64) as usize;
        let w = WORDS[rng.below(WORDS.len() as u64) as usize];
        paths.push(paths[parent].join(format!("{w}-{i}")));
    }
    // Create dirs in index order (parents always precede children).
    for p in &paths {
        fs::create_dir(p)?;
    }
    // Files: per-dir lists, then create in parallel.
    let mut per_dir: Vec<Vec<(u32, u64)>> = vec![Vec::new(); ndirs];
    for i in 0..a.files {
        let d = rng.below(ndirs as u64) as usize;
        let roll = rng.below(1000);
        let size = if a.empty || roll < 600 {
            0
        } else if roll < 900 {
            1 + rng.below(4096)
        } else if roll < 990 {
            8192 + rng.below(256 * 1024)
        } else {
            256 * 1024 + rng.below(64 * 1024 * 1024)
        };
        per_dir[d].push((i as u32, size));
    }
    let block = vec![b'z'; 4096];
    per_dir.par_iter().enumerate().try_for_each(|(d, files)| -> Result<()> {
        for &(i, size) in files {
            let ext = EXTS[(i as usize * 7919) % EXTS.len()];
            let p = paths[d].join(format!("file_{i:07}.{ext}"));
            let mut f = fs::File::create(&p)?;
            if size == 0 {
                continue;
            }
            if size <= 4096 {
                f.write_all(&block[..size as usize])?;
            } else {
                // Sparse: one real block, logical length `size`.
                f.write_all(&block)?;
                f.set_len(size)?;
            }
        }
        Ok(())
    })?;
    println!("{}: {} dirs, {} files", a.out.display(), ndirs, a.files);
    Ok(())
}
