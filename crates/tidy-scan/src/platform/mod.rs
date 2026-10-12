//! Platform directory readers. Each exposes the same `Reader` API:
//! `Reader::new(ReadOptions)` and `Reader::read(&CStr, &mut dyn FnMut(&EntryInfo))`.

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod portable;

#[cfg(target_os = "linux")]
pub use linux::{Reader, Rest, supports_sharing};
#[cfg(target_os = "macos")]
pub use macos::{Reader, Rest, supports_sharing};
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub use portable::{Reader, Rest, supports_sharing};

/// Name of the active fast-path backend, for diagnostics.
pub const BACKEND: &str = if cfg!(target_os = "linux") {
    "getdents64+statx"
} else if cfg!(target_os = "macos") {
    "getattrlistbulk"
} else {
    "std::fs"
};

/// Device id of `path` itself (no symlink following), in the same
/// representation the platform reader reports in `EntryInfo::dev`.
pub fn dev_of(path: &std::ffi::CStr) -> std::io::Result<u64> {
    #[cfg(unix)]
    {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::lstat(path.as_ptr(), &mut st) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        #[cfg(target_os = "macos")]
        return Ok(st.st_dev as u32 as u64);
        #[cfg(not(target_os = "macos"))]
        return Ok(st.st_dev as u64);
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(0)
    }
}
