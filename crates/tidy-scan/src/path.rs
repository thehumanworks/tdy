//! Byte-level path helpers. Paths are handled as raw bytes on Unix so that
//! non-UTF-8 names survive round trips.

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};

/// Path → NUL-terminated byte vector suitable for `CStr::from_bytes_with_nul`.
pub fn path_to_cbytes(p: &Path) -> Vec<u8> {
    let mut v = path_bytes(p).to_vec();
    v.push(0);
    v
}

pub fn path_to_cstring(p: &Path) -> CString {
    CString::new(path_bytes(p).to_vec()).unwrap_or_else(|_| CString::new("/").unwrap())
}

#[cfg(unix)]
pub fn path_bytes(p: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes()
}
#[cfg(not(unix))]
pub fn path_bytes(p: &Path) -> &[u8] {
    p.to_str().unwrap_or("").as_bytes()
}

#[cfg(unix)]
pub fn bytes_to_path(b: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(b))
}
#[cfg(not(unix))]
pub fn bytes_to_path(b: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(b).into_owned())
}

pub fn cstr_to_path(c: &CStr) -> PathBuf {
    bytes_to_path(c.to_bytes())
}

/// `parent` (with trailing NUL) + "/" + `name` + NUL.
pub fn join_cbytes(parent: &[u8], name: &[u8]) -> Box<[u8]> {
    let p = &parent[..parent.len() - 1];
    let mut v = Vec::with_capacity(p.len() + name.len() + 2);
    v.extend_from_slice(p);
    if !p.ends_with(b"/") {
        v.push(b'/');
    }
    v.extend_from_slice(name);
    v.push(0);
    v.into_boxed_slice()
}

/// Expand a leading `~` to `$HOME`.
pub fn expand_tilde(s: &str) -> PathBuf {
    if s == "~" {
        return home_dir();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(s)
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Contract `$HOME` back to `~` for display.
pub fn display_path(p: &Path) -> String {
    let home = home_dir();
    if home != Path::new("/") {
        if let Ok(rest) = p.strip_prefix(&home) {
            return if rest.as_os_str().is_empty() { "~".into() } else { format!("~/{}", rest.display()) };
        }
    }
    p.display().to_string()
}
