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
                if (t2 == 0x9003 || t2 == 0x9004)
                    && let Some(s) = ascii(c2, f2)
                    && let Some(ts) = parse_exif_datetime(&s)
                {
                    return Some(ts);
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

// ---- thumbnails ------------------------------------------------------------------

/// EXIF orientation (1..=8) for JPEG/HEIC/TIFF files.
pub fn orientation(p: &Path) -> Option<u16> {
    let mut f = std::fs::File::open(p).ok()?;
    let mut buf = vec![0u8; 128 * 1024];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    let t = find_tiff(&buf)?;
    let le = t.starts_with(b"II");
    let u16_at = |o: usize| -> Option<u16> {
        let s = t.get(o..o + 2)?;
        Some(if le { u16::from_le_bytes([s[0], s[1]]) } else { u16::from_be_bytes([s[0], s[1]]) })
    };
    let u32_at = |o: usize| -> Option<u32> {
        let s = t.get(o..o + 4)?;
        Some(if le { u32::from_le_bytes([s[0], s[1], s[2], s[3]]) } else { u32::from_be_bytes([s[0], s[1], s[2], s[3]]) })
    };
    let ifd = u32_at(4)? as usize;
    let n = u16_at(ifd)? as usize;
    for i in 0..n.min(512) {
        let e = ifd + 2 + i * 12;
        if u16_at(e)? == 0x0112 {
            return u16_at(e + 8);
        }
    }
    None
}

/// A JPEG thumbnail (longest side ≤ `max_side`) suitable for a vision judge.
/// JPEGs decode at reduced DCT scale (fast); PNG/WebP/GIF/BMP/TIFF via the
/// `image` crate; HEIC/RAW via `sips` on macOS.
pub fn thumbnail(p: &Path, max_side: u32) -> Option<tidy_judge::Image> {
    let md = std::fs::metadata(p).ok()?;
    if !md.is_file() || md.len() > 200 * 1024 * 1024 {
        return None;
    }
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let img: image::DynamicImage = match ext.as_str() {
        "jpg" | "jpeg" => decode_jpeg_scaled(p, max_side)?,
        "png" | "webp" | "gif" | "bmp" | "tif" | "tiff" => image::ImageReader::open(p).ok()?.with_guessed_format().ok()?.decode().ok()?,
        _ => return system_thumbnail(p, max_side),
    };
    let img = match orientation(p).unwrap_or(1) {
        3 => img.rotate180(),
        6 => img.rotate90(),
        8 => img.rotate270(),
        _ => img,
    };
    let img = if img.width().max(img.height()) > max_side { img.thumbnail(max_side, max_side) } else { img };
    encode_jpeg(&img.to_rgb8())
}

fn encode_jpeg(rgb: &image::RgbImage) -> Option<tidy_judge::Image> {
    let mut out = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80);
    rgb.write_with_encoder(enc).ok()?;
    Some(tidy_judge::Image { mime: "image/jpeg".into(), data: out, caption: None })
}

fn decode_jpeg_scaled(p: &Path, max_side: u32) -> Option<image::DynamicImage> {
    let f = std::io::BufReader::new(std::fs::File::open(p).ok()?);
    let mut d = jpeg_decoder::Decoder::new(f);
    d.read_info().ok()?;
    let target = (max_side.min(u16::MAX as u32)) as u16;
    d.scale(target, target).ok()?;
    let px = d.decode().ok()?;
    let info = d.info()?;
    let (w, h) = (info.width as u32, info.height as u32);
    match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => image::RgbImage::from_raw(w, h, px).map(image::DynamicImage::ImageRgb8),
        jpeg_decoder::PixelFormat::L8 => image::GrayImage::from_raw(w, h, px).map(image::DynamicImage::ImageLuma8),
        jpeg_decoder::PixelFormat::CMYK32 => {
            let rgb: Vec<u8> = px
                .chunks(4)
                .flat_map(|c| {
                    let k = 255 - c[3] as u16;
                    [((255 - c[0] as u16) * k / 255) as u8, ((255 - c[1] as u16) * k / 255) as u8, ((255 - c[2] as u16) * k / 255) as u8]
                })
                .collect();
            image::RgbImage::from_raw(w, h, rgb).map(image::DynamicImage::ImageRgb8)
        }
        _ => None,
    }
}

/// HEIC/HEIF/RAW: let the OS render a JPEG (macOS `sips`, Linux `heif-convert`).
fn system_thumbnail(p: &Path, max_side: u32) -> Option<tidy_judge::Image> {
    let tmp = std::env::temp_dir().join(format!(
        "tidy-thumb-{}-{}.jpg",
        std::process::id(),
        blake3::hash(tidy_scan::path::path_bytes(p)).to_hex()
    ));
    let ok = if cfg!(target_os = "macos") {
        std::process::Command::new("sips")
            .args(["-s", "format", "jpeg", "-s", "formatOptions", "80", "-Z", &max_side.to_string()])
            .arg(p)
            .arg("--out")
            .arg(&tmp)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        std::process::Command::new("heif-convert")
            .args(["-q", "80"])
            .arg(p)
            .arg(&tmp)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    if !ok {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    let data = std::fs::read(&tmp).ok();
    let _ = std::fs::remove_file(&tmp);
    // heif-convert does not resize: re-thumbnail through the JPEG path.
    let data = data?;
    if cfg!(target_os = "macos") {
        return Some(tidy_judge::Image { mime: "image/jpeg".into(), data, caption: None });
    }
    let img = image::load_from_memory(&data).ok()?;
    encode_jpeg(&img.thumbnail(max_side, max_side).to_rgb8())
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
