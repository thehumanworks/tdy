//! Space actions: duplicate finding and copy-on-write dedupe, cold-folder
//! archiving, transparent compression, and compressibility sampling.

use rayon::prelude::*;
use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use tidy_scan::visit::{VisitOptions, entry_path, visit_files};

#[derive(Clone, Debug, serde::Serialize)]
pub struct FileRec {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub alloc: u64,
    /// Bytes not already shared with another file (clone/reflink), if known.
    pub private: Option<u64>,
    pub mtime: i64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct DupGroup {
    pub size: u64,
    pub hash: String,
    /// Sorted so that `files[0]` is the suggested keeper.
    pub files: Vec<FileRec>,
}

impl DupGroup {
    /// Bytes reclaimable by deduplicating all but the keeper (ignoring
    /// copies that already share storage).
    pub fn reclaimable(&self) -> u64 {
        self.files[1..].iter().map(|f| f.private.unwrap_or(f.alloc).min(f.alloc)).sum()
    }
}

const SAMPLE: u64 = 16 * 1024;

/// Head + middle + tail sample hash: cheap, and different files almost
/// always differ in one of them.
fn sample_hash(p: &Path, size: u64) -> io::Result<[u8; 32]> {
    let mut f = std::fs::File::open(p)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; SAMPLE as usize];
    let mut take = |f: &mut std::fs::File, off: u64, h: &mut blake3::Hasher| -> io::Result<()> {
        f.seek(SeekFrom::Start(off))?;
        let n = f.read(&mut buf)?;
        h.update(&buf[..n]);
        Ok(())
    };
    take(&mut f, 0, &mut h)?;
    if size > 2 * SAMPLE {
        take(&mut f, size / 2 - SAMPLE / 2, &mut h)?;
        take(&mut f, size - SAMPLE, &mut h)?;
    }
    Ok(*h.finalize().as_bytes())
}

/// Find byte-identical files of at least `min_size` bytes under `roots`.
/// Staged: size → sample hash → full BLAKE3, each stage in parallel, so
/// memory and I/O are spent only on real candidates. Hard links count once.
pub fn find_duplicates(roots: &[PathBuf], min_size: u64, threads: usize) -> io::Result<Vec<DupGroup>> {
    let recs = parking_lot::Mutex::new(Vec::<FileRec>::new());
    let opts = VisitOptions { threads, private: true, skip_hidden_dirs: false, ..Default::default() };
    for r in roots {
        visit_files(r, &opts, |dir, e| {
            if e.kind == tidy_scan::Kind::File && e.logical >= min_size && e.flags & tidy_scan::entry::flags::DATALESS == 0 {
                let rec = FileRec {
                    path: entry_path(dir, e.name),
                    dev: e.dev,
                    ino: e.ino,
                    size: e.logical,
                    alloc: e.alloc,
                    private: e.private,
                    mtime: e.mtime,
                };
                recs.lock().push(rec);
            }
        })?;
    }
    let mut recs = recs.into_inner();
    // one record per inode
    recs.sort_by(|a, b| (a.dev, a.ino, &a.path).cmp(&(b.dev, b.ino, &b.path)));
    recs.dedup_by(|a, b| a.dev == b.dev && a.ino == b.ino);
    // stage 1: (dev, size)
    let mut by_size: HashMap<(u64, u64), Vec<FileRec>> = HashMap::new();
    for r in recs {
        by_size.entry((r.dev, r.size)).or_default().push(r);
    }
    let candidates: Vec<FileRec> = by_size.into_values().filter(|v| v.len() > 1).flatten().collect();
    // stage 2: sample hash
    let sampled: Vec<(FileRec, [u8; 32])> =
        candidates.into_par_iter().filter_map(|r| sample_hash(&r.path, r.size).ok().map(|h| (r, h))).collect();
    let mut by_sample: HashMap<(u64, u64, [u8; 32]), Vec<FileRec>> = HashMap::new();
    for (r, h) in sampled {
        by_sample.entry((r.dev, r.size, h)).or_default().push(r);
    }
    let candidates: Vec<FileRec> = by_sample.into_values().filter(|v| v.len() > 1).flatten().collect();
    // stage 3: full hash
    let full: Vec<(FileRec, String)> =
        candidates.into_par_iter().filter_map(|r| crate::fingerprint::hash_file(&r.path).ok().map(|h| (r, h))).collect();
    let mut by_hash: HashMap<(u64, String), Vec<FileRec>> = HashMap::new();
    for (r, h) in full {
        by_hash.entry((r.dev, h)).or_default().push(r);
    }
    let mut groups: Vec<DupGroup> = by_hash
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|((_, hash), mut files)| {
            files.sort_by_key(keeper_rank);
            DupGroup { size: files[0].size, hash, files }
        })
        .collect();
    groups.sort_by_key(|g| std::cmp::Reverse(g.reclaimable()));
    Ok(groups)
}

/// Lower is a better keeper: not in transient folders, oldest, shortest path.
fn keeper_rank(f: &FileRec) -> (u8, i64, usize) {
    let s = f.path.to_string_lossy();
    let transient =
        ["/Downloads/", "/Desktop/", "/tmp/", "/Trash/", "/.Trash/", "/Caches/", " copy", "(1)", "(2)"].iter().any(|t| s.contains(t));
    (transient as u8, f.mtime, s.len())
}

/// Whether files on the volume holding `p` can share extents.
pub fn clone_supported(p: &Path) -> bool {
    let c = tidy_scan::path::path_to_cstring(p);
    tidy_scan::platform::supports_sharing(&c)
}

/// Make `dup` share storage with `keeper` (identical content required).
/// macOS: `clonefile` into a temp sibling, carry over the duplicate's
/// metadata, then atomically rename over it. Linux: `FIDEDUPERANGE`, where
/// the kernel itself verifies the bytes are equal and shares extents in place.
pub fn dedupe(keeper: &Path, dup: &Path) -> io::Result<()> {
    // Both must be distinct regular files: never follow a symlink, never
    // "dedupe" a file against itself.
    let (mk, md0) = (std::fs::symlink_metadata(keeper)?, std::fs::symlink_metadata(dup)?);
    if !mk.file_type().is_file() || !md0.file_type().is_file() {
        return Err(io::Error::other("dedupe needs two regular files (no symlinks)"));
    }
    if mk.dev() == md0.dev() && mk.ino() == md0.ino() {
        return Err(io::Error::other("keeper and duplicate are the same file"));
    }
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32) -> libc::c_int;
        }
        const CLONE_NOFOLLOW: u32 = 0x0001;
        let md = std::fs::symlink_metadata(dup)?;
        let tmp = dup.with_file_name(format!(".{}.tidy-clone", dup.file_name().unwrap_or_default().to_string_lossy()));
        let _ = std::fs::remove_file(&tmp);
        let (s, d) = (tidy_scan::path::path_to_cstring(keeper), tidy_scan::path::path_to_cstring(&tmp));
        if unsafe { clonefile(s.as_ptr(), d.as_ptr(), CLONE_NOFOLLOW) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let res = (|| {
            std::fs::set_permissions(&tmp, md.permissions())?;
            crate::fsops::set_mtime(&tmp, md.mtime(), md.mtime_nsec())?;
            if crate::fingerprint::hash_file(&tmp)? != crate::fingerprint::hash_file(dup)? {
                return Err(io::Error::other("content differs; not deduplicating"));
            }
            std::fs::rename(&tmp, dup)
        })();
        if res.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        return res;
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        const FIDEDUPERANGE: libc::c_ulong = 0xC018_9436;
        const CHUNK: u64 = 16 * 1024 * 1024;
        let src = std::fs::File::open(keeper)?;
        let dst = std::fs::OpenOptions::new().write(true).open(dup)?;
        let len = src.metadata()?.len();
        if dst.metadata()?.len() != len {
            return Err(io::Error::other("sizes differ"));
        }
        let mut off = 0u64;
        while off < len {
            let n = CHUNK.min(len - off);
            // struct file_dedupe_range (24 bytes) + one file_dedupe_range_info (32 bytes)
            let mut buf = [0u64; 7];
            buf[0] = off; // src_offset
            buf[1] = n; // src_length
            buf[2] = 1; // dest_count (u16) | reserved1 | reserved2
            buf[3] = dst.as_raw_fd() as i64 as u64; // dest_fd
            buf[4] = off; // dest_offset
            if unsafe { libc::ioctl(src.as_raw_fd(), FIDEDUPERANGE as _, buf.as_mut_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let deduped = buf[5];
            let status = (buf[6] & 0xFFFF_FFFF) as u32 as i32;
            if status == 1 {
                return Err(io::Error::other("content differs; not deduplicating"));
            }
            if status < 0 {
                return Err(io::Error::from_raw_os_error(-status));
            }
            if deduped == 0 {
                return Err(io::Error::other("kernel deduplicated 0 bytes"));
            }
            off += deduped;
        }
        return Ok(());
    }
    #[allow(unreachable_code)]
    {
        let _ = (keeper, dup);
        Err(io::Error::other("copy-on-write dedupe is not supported on this platform"))
    }
}

/// Estimated compressed/original ratio from zstd-1 over samples of up to
/// `max_files` files (64 KB each). 1.0 = incompressible.
pub fn compressibility(dir: &Path, max_files: usize) -> f64 {
    let files = parking_lot::Mutex::new(Vec::new());
    let _ = visit_files(dir, &VisitOptions { threads: 4, ..Default::default() }, |d, e| {
        if e.kind == tidy_scan::Kind::File && e.logical > 0 {
            let mut v = files.lock();
            if v.len() < max_files * 8 {
                v.push((entry_path(d, e.name), e.logical));
            }
        }
    });
    let mut files = files.into_inner();
    files.sort_by_key(|f| std::cmp::Reverse(f.1));
    files.truncate(max_files);
    let (raw, comp): (u64, u64) = files
        .par_iter()
        .filter_map(|(p, _)| {
            let mut f = std::fs::File::open(p).ok()?;
            let mut buf = vec![0u8; 64 * 1024];
            let n = f.read(&mut buf).ok()?;
            let c = zstd::bulk::compress(&buf[..n], 1).ok()?;
            Some((n as u64, c.len() as u64))
        })
        .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    if raw == 0 { 1.0 } else { comp as f64 / raw as f64 }
}

/// Pack `dir` into `dest` (`.tar.zst`), preserving symlinks, modes and
/// mtimes; then verify by reading the archive back. Returns (archive bytes,
/// entries).
pub fn archive_dir(dir: &Path, dest: &Path, level: i32) -> io::Result<(u64, u64)> {
    let partial = dest.with_extension("zst.tidy-partial");
    let res = (|| {
        let f = std::fs::File::create(&partial)?;
        let mut enc = zstd::stream::Encoder::new(io::BufWriter::with_capacity(1 << 20, f), level)?;
        let threads = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1).min(8);
        let _ = enc.multithread(threads);
        let mut tb = tar::Builder::new(enc);
        tb.follow_symlinks(false);
        let base = dir.file_name().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("archive"));
        tb.append_dir_all(&base, dir)?;
        let enc = tb.into_inner()?;
        let w = enc.finish()?;
        w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        // verify: entry count and byte total must match the source tree
        let (src_files, src_bytes) = tree_files_bytes(dir)?;
        let mut ar = tar::Archive::new(zstd::stream::Decoder::new(std::fs::File::open(&partial)?)?);
        let (mut n, mut bytes, mut files) = (0u64, 0u64, 0u64);
        for e in ar.entries()? {
            let mut e = e?;
            n += 1;
            if e.header().entry_type().is_file() {
                files += 1;
                bytes += io::copy(&mut e, &mut io::sink())?;
            }
        }
        if files != src_files || bytes != src_bytes {
            return Err(io::Error::other(format!("archive verification failed ({files}/{src_files} files, {bytes}/{src_bytes} bytes)")));
        }
        crate::fsops::rename_noreplace(&partial, dest)?;
        Ok((std::fs::metadata(dest)?.len(), n))
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    res
}

fn tree_files_bytes(dir: &Path) -> io::Result<(u64, u64)> {
    let acc = parking_lot::Mutex::new((0u64, 0u64));
    visit_files(dir, &VisitOptions { threads: 4, ..Default::default() }, |_, e| {
        if e.kind == tidy_scan::Kind::File {
            let mut a = acc.lock();
            a.0 += 1;
            a.1 += e.logical;
        }
    })?;
    Ok(acc.into_inner())
}

/// Transparent filesystem compression in place: APFS (decmpfs via `ditto
/// --hfsCompression`, swapped in atomically) or btrfs (`defragment -czstd`).
pub fn transparent_compress(p: &Path) -> io::Result<()> {
    if cfg!(target_os = "macos") {
        let tmp = p.with_file_name(format!(".{}.tidy-compress", p.file_name().unwrap_or_default().to_string_lossy()));
        let _ = crate::fsops::remove_tree(&tmp);
        let before = crate::fingerprint::fingerprint(p, None, true)?;
        let ok = std::process::Command::new("ditto").arg("--hfsCompression").arg(p).arg(&tmp).status()?.success();
        if !ok {
            let _ = crate::fsops::remove_tree(&tmp);
            return Err(io::Error::other("ditto --hfsCompression failed"));
        }
        // Abort if the original changed while ditto copied it (writes would be lost).
        let after = crate::fingerprint::fingerprint(p, None, true)?;
        let copy = crate::fingerprint::fingerprint(&tmp, None, true)?;
        if before != after || copy.size != after.size || copy.files != after.files || copy.hash != after.hash {
            let _ = crate::fsops::remove_tree(&tmp);
            return Err(io::Error::other("source changed during compression, or the compressed copy differs; aborted"));
        }
        swap(&tmp, p)?;
        return crate::fsops::remove_tree(&tmp);
    }
    if fs_is_btrfs(p) {
        let ok = std::process::Command::new("btrfs").args(["filesystem", "defragment", "-r", "-czstd"]).arg(p).status()?.success();
        return if ok { Ok(()) } else { Err(io::Error::other("btrfs defragment failed")) };
    }
    Err(io::Error::other("transparent compression needs APFS or btrfs (use archive instead)"))
}

pub fn transparent_compression_supported(p: &Path) -> bool {
    if cfg!(target_os = "macos") {
        let c = tidy_scan::path::path_to_cstring(p);
        return tidy_scan::platform::supports_sharing(&c); // APFS
    }
    fs_is_btrfs(p)
}

fn fs_is_btrfs(p: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        let c = tidy_scan::path::path_to_cstring(p);
        return tidy_scan::platform::linux::fs_type(&c) == Some("btrfs");
    }
    #[allow(unreachable_code)]
    {
        let _ = p;
        false
    }
}

/// Atomically exchange two paths (macOS `RENAME_SWAP`, Linux `RENAME_EXCHANGE`).
pub fn swap(a: &Path, b: &Path) -> io::Result<()> {
    let (ca, cb) = (tidy_scan::path::path_to_cstring(a), tidy_scan::path::path_to_cstring(b));
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn renamex_np(from: *const libc::c_char, to: *const libc::c_char, flags: libc::c_uint) -> libc::c_int;
        }
        const RENAME_SWAP: libc::c_uint = 0x0000_0002;
        if unsafe { renamex_np(ca.as_ptr(), cb.as_ptr(), RENAME_SWAP) } != 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        if unsafe { libc::renameat2(libc::AT_FDCWD, ca.as_ptr(), libc::AT_FDCWD, cb.as_ptr(), libc::RENAME_EXCHANGE) } != 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(());
    }
    #[allow(unreachable_code)]
    {
        let _ = (ca, cb);
        Err(io::Error::other("atomic swap unsupported"))
    }
}

/// Is `p` already compressed transparently (APFS `UF_COMPRESSED`)?
pub fn is_compressed(p: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt as _;
        return std::fs::symlink_metadata(p).map(|m| m.st_flags() & 0x20 != 0).unwrap_or(false);
    }
    #[allow(unreachable_code)]
    {
        let _ = p;
        false
    }
}

pub fn dev_of(p: &Path) -> u64 {
    std::fs::symlink_metadata(p).map(|m| m.dev()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finds_dupes_and_archives() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        std::fs::create_dir_all(r.join("a/b")).unwrap();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        std::fs::write(r.join("a/orig.bin"), &data).unwrap();
        std::fs::write(r.join("a/b/copy.bin"), &data).unwrap();
        let mut other = data.clone();
        other[150_000] ^= 1; // same size, same head/tail, differs in the middle only
        std::fs::write(r.join("a/near.bin"), &other).unwrap();
        std::fs::hard_link(r.join("a/orig.bin"), r.join("a/hard.bin")).unwrap();
        let g = find_duplicates(&[r.to_path_buf()], 1000, 2).unwrap();
        assert_eq!(g.len(), 1, "{g:?}");
        assert_eq!(g[0].files.len(), 2);
        // archive + verify
        let dest = r.join("a.tar.zst");
        let (bytes, n) = archive_dir(&r.join("a"), &dest, 3).unwrap();
        assert!(bytes > 0 && bytes < 900_000 && n >= 5);
        assert!(compressibility(&r.join("a"), 10) < 0.5);
    }
}
