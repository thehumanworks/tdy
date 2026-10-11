//! Reversible removal: the OS trash, and Tidy's own quarantine.
//!
//! Both are same-volume renames, so they are instant regardless of size and
//! cannot fail half-way. Undo is the reverse rename.

use crate::fsops::{create_private_dir, mount_root, rename_noreplace};
#[cfg(target_os = "macos")]
use crate::fsops::unique_in;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum TrashError {
    #[error("no trash on the same volume as {0} (use quarantine instead)")]
    NoTrashOnVolume(PathBuf),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// A reserved destination in the trash. On XDG systems the `.trashinfo`
/// companion is created up front (O_EXCL) to reserve the name.
#[derive(Debug, Clone)]
pub struct Slot {
    pub dst: PathBuf,
    pub info: Option<PathBuf>,
}

/// Move `p` into a previously reserved slot.
pub fn commit(p: &Path, slot: &Slot) -> io::Result<()> {
    rename_noreplace(p, &slot.dst).inspect_err(|_| {
        if let Some(i) = &slot.info {
            let _ = std::fs::remove_file(i);
        }
    })
}

/// Move `p` to the OS trash. Returns the path it now lives at.
pub fn trash(p: &Path) -> Result<PathBuf, TrashError> {
    let slot = trash_slot(p, true)?;
    commit(p, &slot)?;
    Ok(slot.dst)
}

/// Reserve a trash destination for `p` without moving it. With
/// `reserve = false` nothing is created (dry runs): the returned slot is the
/// destination that would be used.
pub fn trash_slot(p: &Path, reserve: bool) -> Result<Slot, TrashError> {
    let dev = std::fs::symlink_metadata(p)?.dev();
    let name = p.file_name().ok_or_else(|| io::Error::other("cannot trash a root"))?;
    #[cfg(target_os = "macos")]
    {
        let home = tidy_scan::path::home_dir();
        let home_trash = home.join(".Trash");
        let dir = if std::fs::metadata(&home).map(|m| m.dev()).ok() == Some(dev) {
            if reserve {
                create_private_dir(&home_trash)?;
            }
            home_trash
        } else {
            let root = mount_root(p.parent().unwrap_or(p))?;
            let uid = unsafe { libc::getuid() };
            let d = root.join(".Trashes").join(uid.to_string());
            if reserve {
                create_private_dir(&d).map_err(|_| TrashError::NoTrashOnVolume(p.to_path_buf()))?;
            }
            d
        };
        return Ok(Slot { dst: unique_in(&dir, name), info: None });
    }
    #[cfg(not(target_os = "macos"))]
    {
        let (files, info) = xdg_trash_dirs(p, dev, reserve)?;
        // Reserve a unique name by creating the .trashinfo with O_EXCL.
        let mut n = 1u32;
        loop {
            let cand = if n == 1 {
                name.to_os_string()
            } else {
                let pp = Path::new(name);
                let stem = pp.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let ext = pp.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
                format!("{stem}.{n}{ext}").into()
            };
            let info_path = info.join(format!("{}.trashinfo", cand.to_string_lossy()));
            if !reserve {
                if info_path.exists() || files.join(&cand).symlink_metadata().is_ok() {
                    n += 1;
                    continue;
                }
                return Ok(Slot { dst: files.join(&cand), info: None });
            }
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&info_path) {
                Ok(mut f) => {
                    use std::io::Write;
                    let abs = std::fs::canonicalize(p.parent().unwrap_or(Path::new("/")))?.join(name);
                    write!(f, "[Trash Info]\nPath={}\nDeletionDate={}\n", percent_encode(&abs), iso_now())?;
                    return Ok(Slot { dst: files.join(&cand), info: Some(info_path) });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => n += 1,
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// Remove the `.trashinfo` companion after restoring an XDG-trashed item.
pub fn forget_trashinfo(trashed: &Path) {
    if cfg!(target_os = "macos") {
        return;
    }
    if let (Some(files_dir), Some(name)) = (trashed.parent(), trashed.file_name()) {
        if let Some(trash_root) = files_dir.parent() {
            let info = trash_root.join("info").join(format!("{}.trashinfo", name.to_string_lossy()));
            let _ = std::fs::remove_file(info);
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn xdg_trash_dirs(p: &Path, dev: u64, create: bool) -> Result<(PathBuf, PathBuf), TrashError> {
    let home_trash = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| tidy_scan::path::home_dir().join(".local/share"))
        .join("Trash");
    let uid = unsafe { libc::getuid() };
    let mut candidates = vec![home_trash];
    if let Ok(root) = mount_root(p.parent().unwrap_or(p)) {
        candidates.push(root.join(".Trash").join(uid.to_string()));
        candidates.push(root.join(format!(".Trash-{uid}")));
    }
    for t in candidates {
        let base = t.parent().unwrap_or(&t);
        let base_dev = std::fs::metadata(base).or_else(|_| std::fs::metadata(base.parent().unwrap_or(base))).map(|m| m.dev());
        if base_dev.ok() != Some(dev) {
            continue;
        }
        let files = t.join("files");
        let info = t.join("info");
        if !create {
            return Ok((files, info));
        }
        if create_private_dir(&files).is_ok() && create_private_dir(&info).is_ok() {
            if std::fs::metadata(&files).map(|m| m.dev()).ok() == Some(dev) {
                return Ok((files, info));
            }
        }
    }
    Err(TrashError::NoTrashOnVolume(p.to_path_buf()))
}

#[cfg(not(target_os = "macos"))]
fn percent_encode(p: &Path) -> String {
    let b = tidy_scan::path::path_bytes(p);
    let mut s = String::with_capacity(b.len());
    for &c in b {
        if c.is_ascii_alphanumeric() || b"/-_.~".contains(&c) {
            s.push(c as char);
        } else {
            s.push_str(&format!("%{c:02X}"));
        }
    }
    s
}

#[cfg(not(target_os = "macos"))]
fn iso_now() -> String {
    let t = tidy_scan::fmt::now_secs();
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let tt = t as libc::time_t;
    unsafe { libc::localtime_r(&tt, &mut tm) };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// Move `p` into Tidy's quarantine on the same volume:
/// `<data>/quarantine/<plan>/<entry>/<name>` when the data dir shares the
/// volume, else `<mount>/.tidy-quarantine/<uid>/<plan>/<entry>/<name>`.
pub fn quarantine(p: &Path, plan: &str, entry: &str) -> io::Result<PathBuf> {
    let dst = quarantine_slot(p, plan, entry, true)?;
    rename_noreplace(p, &dst)?;
    Ok(dst)
}

/// Compute (and, with `create`, make the parent of) the quarantine
/// destination for `p`.
pub fn quarantine_slot(p: &Path, plan: &str, entry: &str, create: bool) -> io::Result<PathBuf> {
    let dev = std::fs::symlink_metadata(p)?.dev();
    let name = p.file_name().ok_or_else(|| io::Error::other("cannot quarantine a root"))?;
    let qd = crate::dirs::quarantine_dir();
    if create {
        create_private_dir(&qd)?;
    }
    let qd_dev = std::fs::metadata(&qd).or_else(|_| std::fs::metadata(crate::dirs::data_dir())).or_else(|_| std::fs::metadata(tidy_scan::path::home_dir()))?.dev();
    let base = if qd_dev == dev {
        qd
    } else {
        let root = mount_root(p.parent().unwrap_or(p))?;
        let uid = unsafe { libc::getuid() };
        root.join(".tidy-quarantine").join(uid.to_string())
    };
    let dir = base.join(plan).join(entry);
    if create {
        create_private_dir(&dir)?;
    }
    Ok(dir.join(name))
}

/// Quarantine buckets (plan dirs) older than `max_age_secs`, with sizes.
pub fn quarantine_buckets() -> io::Result<Vec<(PathBuf, i64)>> {
    let qd = crate::dirs::quarantine_dir();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&qd) else { return Ok(out) };
    for e in rd.flatten() {
        if let Ok(md) = e.metadata() {
            out.push((e.path(), md.mtime()));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quarantine_roundtrip() {
        let t = tempfile::tempdir().unwrap();
        // SAFETY: test-only env mutation; tests in this module are single-threaded w.r.t. TIDY_HOME.
        unsafe { std::env::set_var("TIDY_HOME", t.path().join("home")) };
        let f = t.path().join("victim.txt");
        std::fs::write(&f, b"data").unwrap();
        let q = quarantine(&f, "plan1", "e1").unwrap();
        assert!(!f.exists());
        assert_eq!(std::fs::read(&q).unwrap(), b"data");
        rename_noreplace(&q, &f).unwrap();
        assert!(f.exists());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn xdg_trash_roundtrip() {
        let t = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_DATA_HOME", t.path().join("share")) };
        std::fs::create_dir_all(t.path().join("share")).unwrap();
        let f = t.path().join("doc.txt");
        std::fs::write(&f, b"x").unwrap();
        let dst = trash(&f).unwrap();
        assert!(dst.starts_with(t.path().join("share/Trash/files")));
        let info = std::fs::read_to_string(t.path().join("share/Trash/info/doc.txt.trashinfo")).unwrap();
        assert!(info.contains("Path=/") && info.contains("doc.txt"));
        // second item with same name gets a distinct slot
        std::fs::write(&f, b"y").unwrap();
        let dst2 = trash(&f).unwrap();
        assert_ne!(dst, dst2);
        forget_trashinfo(&dst);
        assert!(!t.path().join("share/Trash/info/doc.txt.trashinfo").exists());
    }
}
