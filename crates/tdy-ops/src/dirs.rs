//! Where Tidy keeps its own state, and moving it over from the folders
//! the command used when it was called `tidy`.

use std::path::{Path, PathBuf};
use tdy_scan::path::home_dir;

/// `$TDY_HOME`, else `~/Library/Application Support/tdy` (macOS) or
/// `$XDG_DATA_HOME/tdy` / `~/.local/share/tdy`.
pub fn data_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("TDY_HOME") {
        return PathBuf::from(x);
    }
    if cfg!(target_os = "macos") {
        return home_dir().join("Library/Application Support/tdy");
    }
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(x).join("tdy");
    }
    home_dir().join(".local/share/tdy")
}

/// `$TDY_CONFIG`'s directory, else `~/.config/tdy` on both platforms
/// (dotfile-friendly; macOS users of CLI tools expect it there).
pub fn config_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("TDY_CONFIG_DIR") {
        return PathBuf::from(x);
    }
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(x).join("tdy");
    }
    home_dir().join(".config/tdy")
}

pub fn plans_dir() -> PathBuf {
    data_dir().join("plans")
}

pub fn journal_dir() -> PathBuf {
    data_dir().join("journal")
}

pub fn quarantine_dir() -> PathBuf {
    data_dir().join("quarantine")
}

pub fn decisions_cache_dir() -> PathBuf {
    data_dir().join("decisions")
}

/// The folder names before the rename to `tdy`.
const OLD: &str = "tidy";
const NEW: &str = "tdy";

/// The default data and config folders, and the old ones they replace
/// (only where no environment variable picks the folder).
fn renamed() -> Vec<(PathBuf, PathBuf)> {
    let mut v = vec![];
    if std::env::var_os("TDY_HOME").is_none() {
        let d = data_dir();
        if d.file_name().is_some_and(|n| n == NEW) {
            v.push((d.with_file_name(OLD), d));
        }
    }
    if std::env::var_os("TDY_CONFIG_DIR").is_none() {
        let d = config_dir();
        v.push((d.with_file_name(OLD), d));
    }
    if std::env::var_os("TDY_CACHE_DIR").is_none() {
        let d = tdy_scan::snapshot::cache_dir();
        v.push((d.with_file_name(OLD), d));
    }
    v
}

/// What [`migrate`] did, for a one-line notice.
#[derive(Debug, Default)]
pub struct Migrated {
    pub moved: Vec<(PathBuf, PathBuf)>,
    pub failed: Vec<(PathBuf, String)>,
}

/// Move state left by the old `tidy` command to the `tdy` folders, once.
/// The old folder becomes a link to the new one, so absolute paths stored
/// in old plans (quarantined items, the policy file) keep working.
pub fn migrate() -> Migrated {
    let mut out = Migrated::default();
    for (old, new) in renamed() {
        match move_over(&old, &new) {
            Ok(true) => out.moved.push((old, new)),
            Ok(false) => {}
            Err(e) => out.failed.push((old, e.to_string())),
        }
    }
    out
}

/// `Ok(true)` when it moved something.
fn move_over(old: &Path, new: &Path) -> std::io::Result<bool> {
    // nothing to move: never used, already moved (a link), or both exist
    let Ok(md) = std::fs::symlink_metadata(old) else { return Ok(false) };
    if !md.is_dir() || new.symlink_metadata().is_ok() {
        return Ok(false);
    }
    std::fs::rename(old, new)?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(new.file_name().unwrap_or(new.as_os_str()), old)?;
    Ok(true)
}

/// `TIDY_*` variables that are set while their `TDY_*` names are not.
pub fn old_env_vars() -> Vec<String> {
    ["HOME", "CONFIG", "CONFIG_DIR", "CACHE_DIR", "POLICY", "NO_FNOX"]
        .iter()
        .filter(|v| std::env::var_os(format!("TIDY_{v}")).is_some() && std::env::var_os(format!("TDY_{v}")).is_none())
        .map(|v| format!("TIDY_{v}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_folders_move_once_and_leave_a_link() {
        let t = tempfile::tempdir().unwrap();
        let old = t.path().join("tidy");
        let new = t.path().join("tdy");
        std::fs::create_dir_all(old.join("plans")).unwrap();
        std::fs::write(old.join("plans/p.jsonl"), "x").unwrap();
        assert!(move_over(&old, &new).unwrap());
        assert_eq!(std::fs::read_to_string(new.join("plans/p.jsonl")).unwrap(), "x");
        assert!(old.is_symlink(), "old paths still resolve");
        assert_eq!(std::fs::read_to_string(old.join("plans/p.jsonl")).unwrap(), "x");
        // again: nothing to do
        assert!(!move_over(&old, &new).unwrap());
        // both exist: left alone
        let t2 = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t2.path().join("tidy")).unwrap();
        std::fs::create_dir_all(t2.path().join("tdy")).unwrap();
        assert!(!move_over(&t2.path().join("tidy"), &t2.path().join("tdy")).unwrap());
        assert!(!t2.path().join("tidy").is_symlink());
    }
}
