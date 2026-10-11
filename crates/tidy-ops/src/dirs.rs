//! Where Tidy keeps its own state.

use std::path::PathBuf;
use tidy_scan::path::home_dir;

/// `$TIDY_HOME`, else `~/Library/Application Support/tidy` (macOS) or
/// `$XDG_DATA_HOME/tidy` / `~/.local/share/tidy`.
pub fn data_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("TIDY_HOME") {
        return PathBuf::from(x);
    }
    if cfg!(target_os = "macos") {
        return home_dir().join("Library/Application Support/tidy");
    }
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(x).join("tidy");
    }
    home_dir().join(".local/share/tidy")
}

/// `$TIDY_CONFIG`'s directory, else `~/.config/tidy` on both platforms
/// (dotfile-friendly; macOS users of CLI tools expect it there).
pub fn config_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("TIDY_CONFIG_DIR") {
        return PathBuf::from(x);
    }
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(x).join("tidy");
    }
    home_dir().join(".config/tidy")
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
