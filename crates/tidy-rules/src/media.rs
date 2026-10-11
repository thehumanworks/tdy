//! Media helpers: EXIF capture date (JPEG, HEIC/HEIF, TIFF/RAW).

use std::io::Read;
use std::path::Path;

/// Capture time ("DateTimeOriginal", else "DateTime") as seconds since the
/// epoch, interpreting the camera's local time as UTC (fine for bucketing
/// by year/month/day).
pub fn taken_at(p: &Path) -> Option<i64> {
    let mut f = std::fs::File::open(p).ok()?;
    let mut buf = vec![0u8; 256 * 1024];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    let tiff = find_tiff(&buf)?;
    parse_tiff_date(tiff)
}

fn find_tiff(b: &[u8]) -> Option<&[u8]> {
    // TIFF/DNG/most RAW files start with the TIFF header itself.
    if b.starts_with(b"II*\0") || b.starts_with(b"MM\0*") {
        return Some(b);
    }
    // JPEG APP1 and HEIF 'Exif' items carry "Exif\0\0" followed by a TIFF header.
    let pos = b.windows(6).position(|w| w == b"Exif\0\0")?;
    let t = &b[pos + 6..];
    if t.starts_with(b"II*\0") || t.starts_with(b"MM\0*") { Some(t) } else { None }
}

fn parse_tiff_date(t: &[u8]) -> Option<i64> {
    let le = t.starts_with(b"II");
    let u16_at = |o: usize| -> Option<u16> {
        let s = t.get(o..o + 2)?;
        Some(if le { u16::from_le_bytes([s[0], s[1]]) } else { u16::from_be_bytes([s[0], s[1]]) })
    };
    let u32_at = |o: usize| -> Option<u32> {
        let s = t.get(o..o + 4)?;
        Some(if le { u32::from_le_bytes([s[0], s[1], s[2], s[3]]) } else { u32::from_be_bytes([s[0], s[1], s[2], s[3]]) })
    };
    // Returns (tag → (type, count, value_offset_field_pos)) entries of an IFD.
    let read_ifd = |off: usize| -> Option<Vec<(u16, u16, u32, usize)>> {
        let n = u16_at(off)? as usize;
        let mut v = Vec::with_capacity(n);
        for i in 0..n.min(512) {
            let e = off + 2 + i * 12;
            v.push((u16_at(e)?, u16_at(e + 2)?, u32_at(e + 4)?, e + 8));
        }
        Some(v)
    };
    let ascii = |count: u32, field: usize| -> Option<String> {
        let off = if count <= 4 { field } else { u32_at(field)? as usize };
        let s = t.get(off..off + count as usize)?;
        Some(String::from_utf8_lossy(s).trim_end_matches('\0').to_string())
    };
    let ifd0 = read_ifd(u32_at(4)? as usize)?;
    let mut fallback = None;
    for &(tag, _ty, count, field) in &ifd0 {
        if tag == 0x0132 {
            fallback = ascii(count, field);
        }
        if tag == 0x8769 {
            let exif = read_ifd(u32_at(field)? as usize)?;
            for &(t2, _, c2, f2) in &exif {
                if t2 == 0x9003 || t2 == 0x9004 {
                    if let Some(s) = ascii(c2, f2) {
                        if let Some(ts) = parse_exif_datetime(&s) {
                            return Some(ts);
                        }
                    }
                }
            }
        }
    }
    fallback.and_then(|s| parse_exif_datetime(&s))
}

/// "YYYY:MM:DD HH:MM:SS" → epoch seconds (as UTC).
pub fn parse_exif_datetime(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, mo, d, h, mi, se) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1900..=2200).contains(&y) || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    // days from civil (Howard Hinnant)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + se)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal little-endian TIFF with IFD0 → ExifIFD → DateTimeOriginal.
    fn tiff_with_date(date: &str) -> Vec<u8> {
        let mut t = b"II*\0".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes()); // IFD0 at 8
        // IFD0: 1 entry (ExifIFD pointer)
        t.extend_from_slice(&1u16.to_le_bytes());
        t.extend_from_slice(&0x8769u16.to_le_bytes());
        t.extend_from_slice(&4u16.to_le_bytes());
        t.extend_from_slice(&1u32.to_le_bytes());
        t.extend_from_slice(&26u32.to_le_bytes()); // exif ifd at 26
        t.extend_from_slice(&0u32.to_le_bytes());
        // Exif IFD at 26: 1 entry DateTimeOriginal, ASCII, 20 bytes at 44
        t.extend_from_slice(&1u16.to_le_bytes());
        t.extend_from_slice(&0x9003u16.to_le_bytes());
        t.extend_from_slice(&2u16.to_le_bytes());
        t.extend_from_slice(&20u32.to_le_bytes());
        t.extend_from_slice(&44u32.to_le_bytes());
        t.extend_from_slice(&0u32.to_le_bytes());
        t.extend_from_slice(date.as_bytes());
        t.push(0);
        t
    }

    #[test]
    fn exif_in_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xE1, 0, 0];
        jpg.extend_from_slice(b"Exif\0\0");
        jpg.extend_from_slice(&tiff_with_date("2023:07:14 18:30:05"));
        jpg.extend_from_slice(&[0xFF, 0xD9]);
        let p = dir.path().join("a.jpg");
        std::fs::write(&p, &jpg).unwrap();
        let ts = taken_at(&p).unwrap();
        assert_eq!(crate::items::ymd(ts), "2023-07-14");
        assert_eq!(parse_exif_datetime("2024:02:29 00:00:00").map(crate::items::ymd), Some("2024-02-29".into()));
        std::fs::write(&p, b"not an image").unwrap();
        assert!(taken_at(&p).is_none());
    }
}
