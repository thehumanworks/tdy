//! Built-in checks that rules can require (`check = "…"`), evaluated in code.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const KNOWN: &[&str] = &["extracted-sibling"];

/// Checks that prove a removal loses no data (an identical copy exists).
/// Removal-only keep rules (`protects = "removal"`) don't veto these.
pub const LOSSLESS: &[&str] = &["extracted-sibling"];

/// Run a named check against a path. Unknown checks never pass.
pub fn passes(check: &str, p: &Path) -> bool {
    match check {
        "extracted-sibling" => extracted_sibling(p).is_some(),
        _ => false,
    }
}

/// For a `.zip`, the folder next to it that already holds every file in the
/// archive with identical content (size and CRC-32 match). Tries
/// `<dir>/<stem>/<entry>` (loose archives, or Archive Utility's
/// wrap-in-folder) and `<dir>/<entry>` (archives with a top-level folder).
/// macOS metadata entries (`__MACOSX/`, `.DS_Store`) are ignored.
pub fn extracted_sibling(zip: &Path) -> Option<PathBuf> {
    let entries = zip_entries(zip).ok()?;
    let files: Vec<&ZipEntry> = entries
        .iter()
        .filter(|e| {
            !e.name.ends_with('/') && !e.name.starts_with("__MACOSX/") && !e.name.rsplit('/').next().is_some_and(|n| n == ".DS_Store")
        })
        .collect();
    if files.is_empty() {
        return None;
    }
    let parent = zip.parent()?;
    let stem = zip.file_stem()?;
    for base in [parent.join(stem), parent.to_path_buf()] {
        if !base.is_dir() {
            continue;
        }
        let all = files.iter().all(|e| {
            if e.name.contains("..") || e.name.starts_with('/') {
                return false;
            }
            let p = base.join(&e.name);
            match std::fs::symlink_metadata(&p) {
                Ok(m) if m.is_file() && m.len() == e.size => crc32_file(&p).is_some_and(|c| c == e.crc),
                _ => false,
            }
        });
        // `<dir>/<entry>` only counts when entries live under a folder (not loose
        // files that would merely sit next to the zip).
        let under_folder = base != parent || files.iter().all(|e| e.name.contains('/'));
        if all && under_folder {
            return Some(base);
        }
    }
    None
}

fn crc32_file(p: &Path) -> Option<u32> {
    let mut f = std::fs::File::open(p).ok()?;
    let mut h = crc32fast::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Some(h.finalize())
}

#[derive(Debug)]
pub struct ZipEntry {
    pub name: String,
    pub size: u64,
    pub crc: u32,
}

/// Read a zip's central directory (no decompression). Supports ZIP64 sizes.
pub fn zip_entries(p: &Path) -> std::io::Result<Vec<ZipEntry>> {
    let bad = || std::io::Error::other("not a zip archive");
    let mut f = std::fs::File::open(p)?;
    let len = f.metadata()?.len();
    let tail_len = len.min(66_000 + 22);
    f.seek(SeekFrom::Start(len - tail_len))?;
    let mut tail = vec![0u8; tail_len as usize];
    f.read_exact(&mut tail)?;
    let eocd = tail.windows(4).rposition(|w| w == b"PK\x05\x06").ok_or_else(bad)?;
    let u16_at = |b: &[u8], o: usize| -> Option<u64> { Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?) as u64) };
    let u32_at = |b: &[u8], o: usize| -> Option<u64> { Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?) as u64) };
    let u64_at = |b: &[u8], o: usize| -> Option<u64> { Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?)) };
    let mut count = u16_at(&tail, eocd + 10).ok_or_else(bad)?;
    let mut cd_size = u32_at(&tail, eocd + 12).ok_or_else(bad)?;
    let mut cd_off = u32_at(&tail, eocd + 16).ok_or_else(bad)?;
    if (count == 0xFFFF || cd_off == 0xFFFF_FFFF) && eocd >= 20 && &tail[eocd - 20..eocd - 16] == b"PK\x06\x07" {
        let z64_off = u64_at(&tail, eocd - 20 + 8).ok_or_else(bad)?;
        f.seek(SeekFrom::Start(z64_off))?;
        let mut z = [0u8; 56];
        f.read_exact(&mut z)?;
        if &z[..4] != b"PK\x06\x06" {
            return Err(bad());
        }
        count = u64_at(&z, 32).ok_or_else(bad)?;
        cd_size = u64_at(&z, 40).ok_or_else(bad)?;
        cd_off = u64_at(&z, 48).ok_or_else(bad)?;
    }
    if cd_size > 256 * 1024 * 1024 || cd_off + cd_size > len {
        return Err(bad());
    }
    f.seek(SeekFrom::Start(cd_off))?;
    let mut cd = vec![0u8; cd_size as usize];
    f.read_exact(&mut cd)?;
    let mut out = Vec::with_capacity(count as usize);
    let mut o = 0usize;
    for _ in 0..count {
        if cd.get(o..o + 4) != Some(b"PK\x01\x02") {
            return Err(bad());
        }
        let crc = u32_at(&cd, o + 16).ok_or_else(bad)? as u32;
        let mut size = u32_at(&cd, o + 24).ok_or_else(bad)?;
        let name_len = u16_at(&cd, o + 28).ok_or_else(bad)? as usize;
        let extra_len = u16_at(&cd, o + 30).ok_or_else(bad)? as usize;
        let comment_len = u16_at(&cd, o + 32).ok_or_else(bad)? as usize;
        let name = String::from_utf8_lossy(cd.get(o + 46..o + 46 + name_len).ok_or_else(bad)?).into_owned();
        if size == 0xFFFF_FFFF {
            // ZIP64 extra field (0x0001): uncompressed size comes first.
            let extra = cd.get(o + 46 + name_len..o + 46 + name_len + extra_len).ok_or_else(bad)?;
            let mut i = 0;
            while i + 4 <= extra.len() {
                let id = u16_at(extra, i).ok_or_else(bad)?;
                let l = u16_at(extra, i + 2).ok_or_else(bad)? as usize;
                if id == 1 {
                    size = u64_at(extra, i + 4).ok_or_else(bad)?;
                    break;
                }
                i += 4 + l;
            }
        }
        out.push(ZipEntry { name, size, crc });
        o += 46 + name_len + extra_len + comment_len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Minimal stored (uncompressed) zip writer for tests.
    fn write_zip(path: &Path, files: &[(&str, &[u8])]) {
        let mut out = Vec::new();
        let mut cd = Vec::new();
        for (name, data) in files {
            let crc = crc32fast::hash(data);
            let off = out.len() as u32;
            out.extend_from_slice(b"PK\x03\x04");
            out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
            cd.extend_from_slice(b"PK\x01\x02");
            cd.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            cd.extend_from_slice(&crc.to_le_bytes());
            cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
            cd.extend_from_slice(&[0; 12]); // extra, comment, disk, internal and external attributes
            cd.extend_from_slice(&off.to_le_bytes());
            cd.extend_from_slice(name.as_bytes());
        }
        let cd_off = out.len() as u32;
        out.extend_from_slice(&cd);
        out.extend_from_slice(b"PK\x05\x06\0\0\0\0");
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(cd.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        std::fs::File::create(path).unwrap().write_all(&out).unwrap();
    }

    #[test]
    fn detects_extracted_siblings() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        // archive with a top-level folder, extracted next to it
        write_zip(
            &d.join("project.zip"),
            &[("project/a.txt", b"alpha"), ("project/sub/b.txt", b"beta"), ("__MACOSX/project/._a.txt", b"x")],
        );
        std::fs::create_dir_all(d.join("project/sub")).unwrap();
        std::fs::write(d.join("project/a.txt"), b"alpha").unwrap();
        std::fs::write(d.join("project/sub/b.txt"), b"beta").unwrap();
        assert_eq!(extracted_sibling(&d.join("project.zip")), Some(d.to_path_buf()));
        // a modified file means the archive still holds unique content
        std::fs::write(d.join("project/sub/b.txt"), b"BETA").unwrap();
        assert_eq!(extracted_sibling(&d.join("project.zip")), None);
        // loose archive extracted into <stem>/
        write_zip(&d.join("photos.zip"), &[("1.jpg", b"one"), ("2.jpg", b"two")]);
        std::fs::create_dir_all(d.join("photos")).unwrap();
        std::fs::write(d.join("photos/1.jpg"), b"one").unwrap();
        assert_eq!(extracted_sibling(&d.join("photos.zip")), None, "missing 2.jpg");
        std::fs::write(d.join("photos/2.jpg"), b"two").unwrap();
        assert_eq!(extracted_sibling(&d.join("photos.zip")), Some(d.join("photos")));
        // loose files next to the zip itself don't count
        write_zip(&d.join("loose.zip"), &[("a.txt", b"alpha")]);
        std::fs::write(d.join("a.txt"), b"alpha").unwrap();
        assert_eq!(extracted_sibling(&d.join("loose.zip")), None);
        // not a zip
        std::fs::write(d.join("fake.zip"), b"nope").unwrap();
        assert_eq!(extracted_sibling(&d.join("fake.zip")), None);
    }
}
