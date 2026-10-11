//! Primitive filesystem operations with the safety properties Tidy needs:
//! renames that never clobber, deletes that never follow symlinks and cope
//! with read-only trees, and helpers for volumes.

use rayon::prelude::*;
use std::ffi::CString;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tidy_scan::path::path_to_cstring;

/// `rename(src, dst)` that fails with `AlreadyExists` instead of replacing `dst`.
pub fn rename_noreplace(src: &Path, dst: &Path) -> io::Result<()> {
    let s = path_to_cstring(src);
    let d = path_to_cstring(dst);
    #[cfg(target_os = "linux")]
    {
        let rc = unsafe { libc::renameat2(libc::AT_FDCWD, s.as_ptr(), libc::AT_FDCWD, d.as_ptr(), libc::RENAME_NOREPLACE) };
        if rc == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINVAL) && e.raw_os_error() != Some(libc::ENOSYS) {
            return Err(e);
        }
        // Filesystem without RENAME_NOREPLACE support: best-effort check.
        return fallback_rename(&s, &d, dst);
    }
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn renamex_np(from: *const libc::c_char, to: *const libc::c_char, flags: libc::c_uint) -> libc::c_int;
        }
        const RENAME_EXCL: libc::c_uint = 0x0000_0004;
        let rc = unsafe { renamex_np(s.as_ptr(), d.as_ptr(), RENAME_EXCL) };
        if rc == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ENOTSUP) {
            return Err(e);
        }
        return fallback_rename(&s, &d, dst);
    }
    #[allow(unreachable_code)]
    fallback_rename(&s, &d, dst)
}

fn fallback_rename(s: &CString, d: &CString, dst: &Path) -> io::Result<()> {
    if std::fs::symlink_metadata(dst).is_ok() {
        return Err(io::Error::from(io::ErrorKind::AlreadyExists));
    }
    if unsafe { libc::rename(s.as_ptr(), d.as_ptr()) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Remove a file, symlink or directory tree. Never follows symlinks.
/// Read-only directories (e.g. Go's module cache) are made writable as
/// needed. Large trees are removed in parallel.
pub fn remove_tree(p: &Path) -> io::Result<()> {
    let md = std::fs::symlink_metadata(p)?;
    if !md.is_dir() {
        return std::fs::remove_file(p);
    }
    remove_dir_inner(p)
}

fn remove_dir_inner(dir: &Path) -> io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            make_writable(dir)?;
            std::fs::read_dir(dir)?
        }
        Err(e) => return Err(e),
    };
    let mut subdirs = Vec::new();
    let mut need_chmod = false;
    for ent in entries {
        let ent = ent?;
        let ft = ent.file_type()?;
        let path = ent.path();
        if ft.is_dir() {
            subdirs.push(path);
        } else {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied && !need_chmod => {
                    need_chmod = true;
                    make_writable(dir)?;
                    std::fs::remove_file(&path)?;
                }
                Err(e) => return Err(e),
            }
        }
    }
    if subdirs.len() > 1 {
        subdirs.par_iter().try_for_each(|d| remove_dir_inner(d))?;
    } else {
        for d in &subdirs {
            remove_dir_inner(d)?;
        }
    }
    match std::fs::remove_dir(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            if let Some(parent) = dir.parent() {
                make_writable(parent)?;
            }
            std::fs::remove_dir(dir)
        }
        Err(e) => Err(e),
    }
}

fn make_writable(dir: &Path) -> io::Result<()> {
    let md = std::fs::symlink_metadata(dir)?;
    let mut perm = md.permissions();
    perm.set_mode(perm.mode() | 0o700);
    std::fs::set_permissions(dir, perm)
}

/// Whether two paths live on the same device.
pub fn same_device(a: &Path, b: &Path) -> io::Result<bool> {
    Ok(std::fs::symlink_metadata(a)?.dev() == std::fs::symlink_metadata(b)?.dev())
}

/// The mount point containing `p` (walk up until the device changes).
pub fn mount_root(p: &Path) -> io::Result<PathBuf> {
    let p = std::fs::canonicalize(p)?;
    let dev = std::fs::metadata(&p)?.dev();
    let mut cur = p.as_path();
    while let Some(parent) = cur.parent() {
        if std::fs::metadata(parent)?.dev() != dev {
            break;
        }
        cur = parent;
    }
    Ok(cur.to_path_buf())
}

/// A name inside `dir` that does not exist yet: `name`, `name 2`, `name 3`…
/// (the extension is kept: `a.txt` → `a 2.txt`).
pub fn unique_in(dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let first = dir.join(name);
    if std::fs::symlink_metadata(&first).is_err() {
        return first;
    }
    let p = Path::new(name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    for i in 2.. {
        let cand = dir.join(format!("{stem} {i}{ext}"));
        if std::fs::symlink_metadata(&cand).is_err() {
            return cand;
        }
    }
    unreachable!()
}

/// Recursively copy `src` to `dst` (which must not exist), preserving
/// permissions, modification times and symlinks (never following them).
/// Uses the platform copy (clonefile/copy_file_range) for file data.
pub fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let md = std::fs::symlink_metadata(src)?;
    let ft = md.file_type();
    if ft.is_symlink() {
        let target = std::fs::read_link(src)?;
        std::os::unix::fs::symlink(target, dst)?;
    } else if ft.is_dir() {
        std::fs::create_dir(dst)?;
        for ent in std::fs::read_dir(src)? {
            let ent = ent?;
            copy_tree(&ent.path(), &dst.join(ent.file_name()))?;
        }
        std::fs::set_permissions(dst, md.permissions())?;
    } else if ft.is_file() {
        if std::fs::symlink_metadata(dst).is_ok() {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        std::fs::copy(src, dst)?;
    } else {
        return Err(io::Error::other(format!("cannot copy special file {}", src.display())));
    }
    if !ft.is_symlink() {
        set_mtime(dst, md.mtime(), md.mtime_nsec())?;
    }
    Ok(())
}

pub fn set_mtime(p: &Path, secs: i64, nsec: i64) -> io::Result<()> {
    let c = path_to_cstring(p);
    let times = [
        libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT },
        libc::timespec { tv_sec: secs as libc::time_t, tv_nsec: nsec as _ },
    ];
    if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Whether two regular files have identical content (size check, then BLAKE3).
pub fn same_content(a: &Path, b: &Path) -> io::Result<bool> {
    let (ma, mb) = (std::fs::metadata(a)?, std::fs::metadata(b)?);
    if !ma.is_file() || !mb.is_file() || ma.len() != mb.len() {
        return Ok(false);
    }
    Ok(crate::fingerprint::hash_file(a)? == crate::fingerprint::hash_file(b)?)
}

/// Free bytes available to the user on the filesystem holding `p`.
pub fn free_bytes(p: &Path) -> io::Result<u64> {
    let c = path_to_cstring(p);
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(s.f_bavail as u64 * s.f_frsize as u64)
}

/// Create `p` and parents with mode 0700.
pub fn create_private_dir(p: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn noreplace_and_remove() {
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("a");
        let b = t.path().join("b");
        std::fs::write(&a, b"1").unwrap();
        std::fs::write(&b, b"2").unwrap();
        assert_eq!(rename_noreplace(&a, &b).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        std::fs::remove_file(&b).unwrap();
        rename_noreplace(&a, &b).unwrap();
        assert!(!a.exists() && b.exists());

        // read-only tree with a symlink pointing outside
        let outside = t.path().join("outside");
        std::fs::write(&outside, b"keep me").unwrap();
        let tree = t.path().join("tree");
        std::fs::create_dir_all(tree.join("x/y")).unwrap();
        std::fs::write(tree.join("x/y/f"), b"z").unwrap();
        std::os::unix::fs::symlink(&outside, tree.join("x/link")).unwrap();
        for d in [tree.join("x/y"), tree.join("x")] {
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        remove_tree(&tree).unwrap();
        assert!(!tree.exists());
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep me");

        assert_eq!(unique_in(t.path(), "b".as_ref()), t.path().join("b 2"));
    }
}
