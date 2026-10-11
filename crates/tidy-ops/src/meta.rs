//! Provenance and usage metadata from extended attributes: where a file was
//! downloaded from, whether it is quarantined, when it was last opened.
//! All reads are single `getxattr` calls — no Spotlight process spawns.

use std::path::Path;
use tidy_scan::path::path_to_cstring;

/// Raw extended attribute value, without following symlinks.
pub fn xattr(p: &Path, name: &str) -> Option<Vec<u8>> {
    let c = path_to_cstring(p);
    let n = std::ffi::CString::new(name).ok()?;
    let mut buf = vec![0u8; 4096];
    loop {
        #[cfg(target_os = "macos")]
        let r = unsafe { libc::getxattr(c.as_ptr(), n.as_ptr(), buf.as_mut_ptr() as *mut _, buf.len(), 0, libc::XATTR_NOFOLLOW) };
        #[cfg(target_os = "linux")]
        let r = unsafe { libc::lgetxattr(c.as_ptr(), n.as_ptr(), buf.as_mut_ptr() as *mut _, buf.len()) };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let r: isize = -1;
        if r >= 0 {
            buf.truncate(r as usize);
            return Some(buf);
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ERANGE) && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        return None;
    }
}

/// Set an extended attribute (no symlink following).
pub fn set_xattr(p: &Path, name: &str, value: &[u8]) -> std::io::Result<()> {
    let c = path_to_cstring(p);
    let n = std::ffi::CString::new(name).map_err(std::io::Error::other)?;
    #[cfg(target_os = "macos")]
    let r = unsafe { libc::setxattr(c.as_ptr(), n.as_ptr(), value.as_ptr() as *const _, value.len(), 0, libc::XATTR_NOFOLLOW) };
    #[cfg(target_os = "linux")]
    let r = unsafe { libc::lsetxattr(c.as_ptr(), n.as_ptr(), value.as_ptr() as *const _, value.len(), 0) };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let r = -1;
    if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

pub fn remove_xattr(p: &Path, name: &str) -> std::io::Result<()> {
    let c = path_to_cstring(p);
    let n = std::ffi::CString::new(name).map_err(std::io::Error::other)?;
    #[cfg(target_os = "macos")]
    let r = unsafe { libc::removexattr(c.as_ptr(), n.as_ptr(), libc::XATTR_NOFOLLOW) };
    #[cfg(target_os = "linux")]
    let r = unsafe { libc::lremovexattr(c.as_ptr(), n.as_ptr()) };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let r = -1;
    if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Provenance {
    /// URLs the file was downloaded from (most specific first).
    pub where_from: Vec<String>,
    /// Downloading agent from the quarantine flag (e.g. "Safari").
    pub downloaded_by: Option<String>,
    /// Last time the file was opened (seconds), when the OS records it.
    pub last_used: Option<i64>,
}

pub fn provenance(p: &Path) -> Provenance {
    let mut out = Provenance::default();
    if cfg!(target_os = "macos") {
        if let Some(v) = xattr(p, "com.apple.metadata:kMDItemWhereFroms") {
            out.where_from = bplist_strings(&v);
        }
        if let Some(v) = xattr(p, "com.apple.quarantine") {
            // "0083;5f8e0c3a;Safari;UUID"
            let s = String::from_utf8_lossy(&v).into_owned();
            out.downloaded_by = s.split(';').nth(2).filter(|a| !a.is_empty()).map(str::to_string).or(Some("unknown".into()));
        }
        if let Some(v) = xattr(p, "com.apple.lastuseddate#PS")
            && v.len() >= 8
        {
            out.last_used = Some(i64::from_le_bytes(v[..8].try_into().unwrap()));
        }
    } else {
        for k in ["user.xdg.origin.url", "user.xdg.referrer.url"] {
            if let Some(v) = xattr(p, k) {
                out.where_from.push(String::from_utf8_lossy(&v).into_owned());
            }
        }
    }
    out
}

/// Strings from a binary plist holding an array of strings (or a single
/// string). Enough for `kMDItemWhereFroms` and Finder tags.
pub fn bplist_strings(b: &[u8]) -> Vec<String> {
    let parse = || -> Option<Vec<String>> {
        if b.len() < 40 || &b[..8] != b"bplist00" {
            return None;
        }
        let t = &b[b.len() - 32..];
        let off_size = t[6] as usize;
        let ref_size = t[7] as usize;
        let num = u64::from_be_bytes(t[8..16].try_into().ok()?) as usize;
        let top = u64::from_be_bytes(t[16..24].try_into().ok()?) as usize;
        let table = u64::from_be_bytes(t[24..32].try_into().ok()?) as usize;
        let read_n = |at: usize, n: usize| -> Option<usize> {
            let s = b.get(at..at + n)?;
            Some(s.iter().fold(0usize, |a, &x| (a << 8) | x as usize))
        };
        let offset = |i: usize| -> Option<usize> { read_n(table + i * off_size, off_size) };
        // (length, header bytes consumed) for a marker with an inline/extended count
        let count = |at: usize| -> Option<(usize, usize)> {
            let m = *b.get(at)?;
            let low = (m & 0x0F) as usize;
            if low != 0x0F {
                return Some((low, 1));
            }
            let im = *b.get(at + 1)?;
            let n = 1usize << (im & 0x0F);
            Some((read_n(at + 2, n)?, 2 + n))
        };
        let string_at = |obj: usize| -> Option<String> {
            let at = offset(obj)?;
            let m = *b.get(at)?;
            let (len, hdr) = count(at)?;
            match m >> 4 {
                0x5 => Some(String::from_utf8_lossy(b.get(at + hdr..at + hdr + len)?).into_owned()),
                0x6 => {
                    let raw = b.get(at + hdr..at + hdr + len * 2)?;
                    let u: Vec<u16> = raw.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                    Some(String::from_utf16_lossy(&u))
                }
                _ => None,
            }
        };
        if top >= num {
            return None;
        }
        let at = offset(top)?;
        let m = *b.get(at)?;
        if m >> 4 == 0xA {
            let (len, hdr) = count(at)?;
            let mut v = Vec::new();
            for i in 0..len {
                let r = read_n(at + hdr + i * ref_size, ref_size)?;
                if let Some(s) = string_at(r) {
                    v.push(s);
                }
            }
            Some(v)
        } else {
            string_at(top).map(|s| vec![s])
        }
    };
    parse().unwrap_or_default()
}

/// Encode an array of ASCII/UTF-16 strings as a binary plist (Finder tags).
pub fn bplist_encode_strings(items: &[String]) -> Vec<u8> {
    let mut out = b"bplist00".to_vec();
    let mut offsets = Vec::new();
    let n = items.len() + 1;
    let ref_size = if n < 256 { 1 } else { 2 };
    // object 0: array
    offsets.push(out.len());
    if items.len() < 15 {
        out.push(0xA0 | items.len() as u8);
    } else {
        out.push(0xAF);
        out.push(0x11);
        out.extend_from_slice(&(items.len() as u16).to_be_bytes());
    }
    for i in 0..items.len() {
        let r = i + 1;
        if ref_size == 1 {
            out.push(r as u8);
        } else {
            out.extend_from_slice(&(r as u16).to_be_bytes());
        }
    }
    for s in items {
        offsets.push(out.len());
        let ascii = s.is_ascii();
        let units: Vec<u16> = s.encode_utf16().collect();
        let len = if ascii { s.len() } else { units.len() };
        let marker = if ascii { 0x50 } else { 0x60 };
        if len < 15 {
            out.push(marker | len as u8);
        } else {
            out.push(marker | 0x0F);
            out.push(0x11);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        if ascii {
            out.extend_from_slice(s.as_bytes());
        } else {
            for u in units {
                out.extend_from_slice(&u.to_be_bytes());
            }
        }
    }
    let table = out.len();
    let off_size = if table < 256 {
        1
    } else if table < 65536 {
        2
    } else {
        4
    };
    for o in &offsets {
        let bytes = (*o as u64).to_be_bytes();
        out.extend_from_slice(&bytes[8 - off_size..]);
    }
    let mut trailer = [0u8; 32];
    trailer[6] = off_size as u8;
    trailer[7] = ref_size as u8;
    trailer[8..16].copy_from_slice(&(n as u64).to_be_bytes());
    trailer[16..24].copy_from_slice(&0u64.to_be_bytes());
    trailer[24..32].copy_from_slice(&(table as u64).to_be_bytes());
    out.extend_from_slice(&trailer);
    out
}

/// Raw tag strings as stored (macOS keeps "Name\n<colour>"); round-trips
/// through [`set_tags`] without losing colours.
pub fn get_tags_raw(p: &Path) -> Vec<String> {
    if cfg!(target_os = "macos") {
        xattr(p, "com.apple.metadata:_kMDItemUserTags").map(|v| bplist_strings(&v)).unwrap_or_default()
    } else {
        get_tags(p)
    }
}

/// Tag name without the Finder colour suffix.
pub fn tag_name(raw: &str) -> &str {
    raw.split('\n').next().unwrap_or(raw)
}

/// Finder tags (macOS) or `user.xdg.tags` (Linux, comma-separated).
pub fn get_tags(p: &Path) -> Vec<String> {
    if cfg!(target_os = "macos") {
        xattr(p, "com.apple.metadata:_kMDItemUserTags")
            .map(|v| bplist_strings(&v).into_iter().map(|t| t.split('\n').next().unwrap_or("").to_string()).collect())
            .unwrap_or_default()
    } else {
        xattr(p, "user.xdg.tags")
            .map(|v| String::from_utf8_lossy(&v).split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default()
    }
}

/// Replace the full tag set. An empty set removes the attribute.
pub fn set_tags(p: &Path, tags: &[String]) -> std::io::Result<()> {
    let (name, value) = if cfg!(target_os = "macos") {
        ("com.apple.metadata:_kMDItemUserTags", bplist_encode_strings(tags))
    } else {
        ("user.xdg.tags", tags.join(",").into_bytes())
    };
    if tags.is_empty() {
        return match remove_xattr(p, name) {
            Err(e) if e.raw_os_error() != Some(libc::ENODATA) && e.raw_os_error() != Some(93) => Err(e),
            _ => Ok(()),
        };
    }
    set_xattr(p, name, &value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bplist_roundtrip() {
        let v = vec!["https://www.figma.com/download".to_string(), "Ünïcode tag".to_string(), "x".repeat(40)];
        let enc = bplist_encode_strings(&v);
        assert_eq!(bplist_strings(&enc), v);
        assert!(bplist_strings(b"garbage").is_empty());
    }
    #[test]
    fn tags_roundtrip_when_supported() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("a.txt");
        std::fs::write(&f, b"x").unwrap();
        if set_tags(&f, &["Dogs".into(), "2024".into()]).is_err() {
            return; // filesystem without user xattrs
        }
        assert_eq!(get_tags(&f), vec!["Dogs", "2024"]);
        set_tags(&f, &[]).unwrap();
        assert!(get_tags(&f).is_empty());
    }
}
