use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::Arc;
use std::thread;

#[derive(Debug, Clone, Copy)]
struct ImageMeta {
    width: u16,
    height: u16,
    rating: Option<i32>,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let src = match args.next() {
        Some(v) => PathBuf::from(v),
        None => return usage_and_exit(),
    };
    let dst = match args.next() {
        Some(v) => PathBuf::from(v),
        None => return usage_and_exit(),
    };
    if args.next().is_some() {
        return usage_and_exit();
    }

    let src = match fs::canonicalize(&src) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Failed to resolve src path: {e}");
            std::process::exit(2);
        }
    };
    // dst may not exist yet; ensure we still have an absolute path.
    let dst = match fs::canonicalize(&dst).or_else(|_| absolutize(&dst)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Failed to resolve dst path: {e}");
            std::process::exit(2);
        }
    };

    let jobs = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    let scanned = Arc::new(AtomicUsize::new(0));
    let matched = Arc::new(AtomicUsize::new(0));
    let copied = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(AtomicUsize::new(0));

    // One channel per worker to avoid receiver contention.
    let mut senders: Vec<SyncSender<PathBuf>> = Vec::with_capacity(jobs);
    let mut handles = Vec::with_capacity(jobs);

    for _ in 0..jobs {
        let (tx, rx) = mpsc::sync_channel::<PathBuf>(2048);
        senders.push(tx);

        let src_root = src.clone();
        let dst_root = dst.clone();
        let scanned = Arc::clone(&scanned);
        let matched = Arc::clone(&matched);
        let copied = Arc::clone(&copied);
        let errors = Arc::clone(&errors);

        handles.push(thread::spawn(move || {
            while let Ok(path) = rx.recv() {
                scanned.fetch_add(1, Ordering::Relaxed);
                match process_one(&path, &src_root, &dst_root) {
                    Ok(ProcessOutcome::Skipped) => {}
                    Ok(ProcessOutcome::MatchedNotCopied) => {
                        matched.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(ProcessOutcome::MatchedCopied) => {
                        matched.fetch_add(1, Ordering::Relaxed);
                        copied.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }

    // Producer: walk filesystem and dispatch paths round-robin.
    let skip_dir = if dst.starts_with(&src) { Some(dst.clone()) } else { None };
    if let Err(e) = walk_and_dispatch(&src, skip_dir.as_deref(), &senders) {
        eprintln!("Walk error: {e}");
        std::process::exit(2);
    }
    drop(senders); // close channels
    for h in handles {
        let _ = h.join();
    }

    eprintln!(
        "scanned={} matched={} copied={} errors={}",
        scanned.load(Ordering::Relaxed),
        matched.load(Ordering::Relaxed),
        copied.load(Ordering::Relaxed),
        errors.load(Ordering::Relaxed)
    );
}

fn absolutize(p: &Path) -> io::Result<PathBuf> {
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(p))
    }
}

fn usage_and_exit() {
    eprintln!("Usage: imagefind <src_root> <dst_root>");
    eprintln!("Copies files where rating==5 and width>height to dst, preserving relative paths.");
    std::process::exit(2);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessOutcome {
    Skipped,
    MatchedNotCopied,
    MatchedCopied,
}

fn process_one(src_path: &Path, src_root: &Path, dst_root: &Path) -> io::Result<ProcessOutcome> {
    let meta = match read_jpeg_meta(src_path)? {
        Some(m) => m,
        None => return Ok(ProcessOutcome::Skipped),
    };

    let rating_ok = meta.rating == Some(5);
    let landscape = meta.width > meta.height;
    if !(rating_ok && landscape) {
        return Ok(ProcessOutcome::Skipped);
    }

    let rel = match src_path.strip_prefix(src_root) {
        Ok(r) => r,
        Err(_) => return Ok(ProcessOutcome::Skipped),
    };
    let dst_path = dst_root.join(rel);

    if dst_path.exists() {
        // Safe default: do not overwrite.
        return Ok(ProcessOutcome::MatchedNotCopied);
    }

    if let Some(parent) = dst_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(src_path, &dst_path)?;
    Ok(ProcessOutcome::MatchedCopied)
}

fn walk_and_dispatch(
    src_root: &Path,
    skip_dir: Option<&Path>,
    senders: &[SyncSender<PathBuf>],
) -> io::Result<()> {
    let mut dirs: Vec<PathBuf> = vec![src_root.to_path_buf()];
    let mut i = 0usize;

    while let Some(dir) = dirs.pop() {
        // Avoid infinite recursion if dst is inside src.
        if let Some(skip) = skip_dir {
            if dir.starts_with(skip) {
                continue;
            }
        }

        let rd = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in rd {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                dirs.push(path);
            } else if ft.is_file() && is_jpeg_path(&path) {
                let tx = &senders[i % senders.len()];
                i = i.wrapping_add(1);
                let _ = tx.send(path);
            }
        }
    }
    Ok(())
}

fn is_jpeg_path(path: &Path) -> bool {
    let ext = path.extension().and_then(OsStr::to_str).unwrap_or("");
    ext.eq_ignore_ascii_case("jpg") || ext.eq_ignore_ascii_case("jpeg")
}

fn read_jpeg_meta(path: &Path) -> io::Result<Option<ImageMeta>> {
    let f = fs::File::open(path)?;
    let mut r = io::BufReader::with_capacity(64 * 1024, f);
    parse_jpeg_meta(&mut r)
}

fn parse_jpeg_meta<R: Read>(r: &mut R) -> io::Result<Option<ImageMeta>> {
    let mut soi = [0u8; 2];
    if r.read_exact(&mut soi).is_err() {
        return Ok(None);
    }
    if soi != [0xFF, 0xD8] {
        return Ok(None);
    }

    let mut width: Option<u16> = None;
    let mut height: Option<u16> = None;
    let mut rating: Option<i32> = None;

    loop {
        let marker = match read_marker(r) {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(_) => break,
        };
        if marker == 0xD9 || marker == 0xDA {
            // EOI / SOS
            break;
        }
        if (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            continue;
        }
        let seg_len = match read_u16_be(r) {
            Ok(v) => v,
            Err(_) => break,
        };
        if seg_len < 2 {
            break;
        }
        let mut remaining = (seg_len - 2) as usize;

        if is_sof_marker(marker) {
            if remaining < 5 {
                discard(r, remaining)?;
                continue;
            }
            let mut head = [0u8; 5];
            r.read_exact(&mut head)?;
            remaining -= 5;
            // head[0] = precision
            height = Some(u16::from_be_bytes([head[1], head[2]]));
            width = Some(u16::from_be_bytes([head[3], head[4]]));
            discard(r, remaining)?;
        } else if marker == 0xE1 {
            // APP1: EXIF or XMP
            // Safety cap: APP1 segments should be relatively small; avoid huge allocations.
            if remaining > 4 * 1024 * 1024 {
                discard(r, remaining)?;
                continue;
            }
            let mut data = vec![0u8; remaining];
            r.read_exact(&mut data)?;
            if rating.is_none() {
                // Prefer EXIF rating (common for many workflows). If absent, fall back to XMP.
                if let Some(x) = parse_app1_exif_rating(&data) {
                    rating = Some(x);
                } else if let Some(x) = parse_app1_xmp_rating(&data) {
                    rating = Some(x);
                }
            }
        } else {
            discard(r, remaining)?;
        }

        if width.is_some() && height.is_some() && rating.is_some() {
            break;
        }
    }

    match (width, height) {
        (Some(w), Some(h)) => Ok(Some(ImageMeta {
            width: w,
            height: h,
            rating,
        })),
        _ => Ok(None),
    }
}

fn read_marker<R: Read>(r: &mut R) -> io::Result<Option<u8>> {
    // Markers are 0xFF followed by a non-0xFF byte.
    let mut b = [0u8; 1];
    loop {
        if r.read_exact(&mut b).is_err() {
            return Ok(None);
        }
        if b[0] == 0xFF {
            break;
        }
    }
    loop {
        if r.read_exact(&mut b).is_err() {
            return Ok(None);
        }
        if b[0] != 0xFF {
            return Ok(Some(b[0]));
        }
    }
}

fn read_u16_be<R: Read>(r: &mut R) -> io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_be_bytes(b))
}

fn discard<R: Read>(r: &mut R, mut n: usize) -> io::Result<()> {
    let mut buf = [0u8; 8192];
    while n > 0 {
        let take = buf.len().min(n);
        r.read_exact(&mut buf[..take])?;
        n -= take;
    }
    Ok(())
}

fn is_sof_marker(m: u8) -> bool {
    matches!(
        m,
        0xC0 | 0xC1 | 0xC2 | 0xC3 | 0xC5 | 0xC6 | 0xC7 | 0xC9 | 0xCA | 0xCB | 0xCD | 0xCE | 0xCF
    )
}

fn parse_app1_xmp_rating(app1: &[u8]) -> Option<i32> {
    const XMP_HEAD: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
    if !app1.starts_with(XMP_HEAD) {
        return None;
    }
    let xml = &app1[XMP_HEAD.len()..];
    parse_xmp_rating(xml)
}

fn parse_xmp_rating(xml: &[u8]) -> Option<i32> {
    let s = std::str::from_utf8(xml).ok()?;

    // Common forms:
    //   <xmp:Rating>5</xmp:Rating>
    //   xmp:Rating="5"
    // Keep parsing simple and fast.
    if let Some(pos) = s.find("<xmp:Rating>") {
        let rest = &s[pos + "<xmp:Rating>".len()..];
        return parse_leading_int(rest);
    }
    if let Some(pos) = s.find("xmp:Rating=\"") {
        let rest = &s[pos + "xmp:Rating=\"".len()..];
        return parse_until_quote(rest, '"');
    }
    if let Some(pos) = s.find("xmp:Rating='") {
        let rest = &s[pos + "xmp:Rating='".len()..];
        return parse_until_quote(rest, '\'');
    }
    None
}

fn parse_leading_int(s: &str) -> Option<i32> {
    let s = s.trim_start();
    let mut end = 0usize;
    for (idx, ch) in s.char_indices() {
        if ch.is_ascii_digit() || (idx == 0 && ch == '-') {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    s[..end].parse().ok()
}

fn parse_until_quote(s: &str, q: char) -> Option<i32> {
    let end = s.find(q).unwrap_or(s.len());
    s[..end].trim().parse().ok()
}

fn parse_app1_exif_rating(app1: &[u8]) -> Option<i32> {
    const EXIF_HEAD: &[u8] = b"Exif\0\0";
    if !app1.starts_with(EXIF_HEAD) {
        return None;
    }
    parse_tiff_rating(&app1[EXIF_HEAD.len()..])
}

fn parse_tiff_rating(tiff: &[u8]) -> Option<i32> {
    if tiff.len() < 8 {
        return None;
    }
    let little = match &tiff[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    if read_u16(tiff, 2, little)? != 42 {
        return None;
    }
    let ifd0 = read_u32(tiff, 4, little)? as usize;
    let n = read_u16(tiff, ifd0, little)? as usize;
    let mut off = ifd0 + 2;
    for _ in 0..n {
        if off + 12 > tiff.len() {
            return None;
        }
        let tag = read_u16(tiff, off, little)?;
        let typ = read_u16(tiff, off + 2, little)?;
        let count = read_u32(tiff, off + 4, little)?;
        let value = &tiff[off + 8..off + 12];

        // 0x4746 = Rating, 0x4749 = RatingPercent (Windows).
        if tag == 0x4746 || tag == 0x4749 {
            let size = type_size(typ)?;
            let total = (count as usize).checked_mul(size)?;
            if count == 1 {
                if total <= 4 {
                    return read_inline_value(value, typ, little);
                }
                let data_off = read_u32(tiff, off + 8, little)? as usize;
                return read_typed_value(tiff, data_off, typ, little);
            }
        }

        off += 12;
    }
    None
}

fn type_size(typ: u16) -> Option<usize> {
    match typ {
        1 => Some(1), // BYTE
        3 => Some(2), // SHORT
        4 => Some(4), // LONG
        9 => Some(4), // SLONG
        _ => None,
    }
}

fn read_inline_value(value: &[u8], typ: u16, little: bool) -> Option<i32> {
    match typ {
        1 => Some(value[0] as i32),
        3 => Some(read_u16_from_4(value, little)? as i32),
        4 => Some(read_u32_from_4(value, little)? as i32),
        9 => Some(read_i32_from_4(value, little)?),
        _ => None,
    }
}

fn read_typed_value(buf: &[u8], off: usize, typ: u16, little: bool) -> Option<i32> {
    match typ {
        1 => buf.get(off).map(|b| *b as i32),
        3 => Some(read_u16(buf, off, little)? as i32),
        4 => Some(read_u32(buf, off, little)? as i32),
        9 => Some(read_i32(buf, off, little)?),
        _ => None,
    }
}

fn read_u16(buf: &[u8], off: usize, little: bool) -> Option<u16> {
    let b0 = *buf.get(off)?;
    let b1 = *buf.get(off + 1)?;
    Some(if little {
        u16::from_le_bytes([b0, b1])
    } else {
        u16::from_be_bytes([b0, b1])
    })
}

fn read_u32(buf: &[u8], off: usize, little: bool) -> Option<u32> {
    let b0 = *buf.get(off)?;
    let b1 = *buf.get(off + 1)?;
    let b2 = *buf.get(off + 2)?;
    let b3 = *buf.get(off + 3)?;
    Some(if little {
        u32::from_le_bytes([b0, b1, b2, b3])
    } else {
        u32::from_be_bytes([b0, b1, b2, b3])
    })
}

fn read_i32(buf: &[u8], off: usize, little: bool) -> Option<i32> {
    Some(read_u32(buf, off, little)? as i32)
}

fn read_u16_from_4(v: &[u8], little: bool) -> Option<u16> {
    if v.len() < 4 {
        return None;
    }
    Some(if little {
        u16::from_le_bytes([v[0], v[1]])
    } else {
        u16::from_be_bytes([v[0], v[1]])
    })
}

fn read_u32_from_4(v: &[u8], little: bool) -> Option<u32> {
    if v.len() < 4 {
        return None;
    }
    Some(if little {
        u32::from_le_bytes([v[0], v[1], v[2], v[3]])
    } else {
        u32::from_be_bytes([v[0], v[1], v[2], v[3]])
    })
}

fn read_i32_from_4(v: &[u8], little: bool) -> Option<i32> {
    Some(read_u32_from_4(v, little)? as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_jpeg_with_sof_and_xmp(width: u16, height: u16, rating: i32) -> Vec<u8> {
        // SOI
        let mut v = vec![0xFF, 0xD8];
        // APP1 XMP
        let xmp = format!(
            "<?xpacket begin='\u{feff}' id='W5M0MpCehiHzreSzNTczkc9d'?>\n<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description xmlns:xmp='http://ns.adobe.com/xap/1.0/'><xmp:Rating>{}</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>",
            rating
        );
        let mut app1 = b"http://ns.adobe.com/xap/1.0/\0".to_vec();
        app1.extend_from_slice(xmp.as_bytes());
        let len = (app1.len() + 2) as u16;
        v.extend_from_slice(&[0xFF, 0xE1]);
        v.extend_from_slice(&len.to_be_bytes());
        v.extend_from_slice(&app1);

        // SOF0
        let mut sof = vec![8];
        sof.extend_from_slice(&height.to_be_bytes());
        sof.extend_from_slice(&width.to_be_bytes());
        sof.extend_from_slice(&[3, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0]);
        let len = (sof.len() + 2) as u16;
        v.extend_from_slice(&[0xFF, 0xC0]);
        v.extend_from_slice(&len.to_be_bytes());
        v.extend_from_slice(&sof);

        // EOI
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    #[test]
    fn parses_dims_and_xmp_rating() {
        let jpg = minimal_jpeg_with_sof_and_xmp(4000, 3000, 5);
        let mut r = io::Cursor::new(jpg);
        let meta = parse_jpeg_meta(&mut r).unwrap().unwrap();
        assert_eq!(meta.width, 4000);
        assert_eq!(meta.height, 3000);
        assert_eq!(meta.rating, Some(5));
    }

    #[test]
    fn parses_tiff_rating_ii_short_inline() {
        // Minimal TIFF with IFD0 containing tag 0x4746 (Rating) type SHORT count 1 value=5.
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes()); // ifd0 offset
        tiff.extend_from_slice(&1u16.to_le_bytes()); // one entry
        tiff.extend_from_slice(&0x4746u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&5u16.to_le_bytes());
        tiff.extend_from_slice(&0u16.to_le_bytes()); // padding to 4 bytes
        tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD
        assert_eq!(parse_tiff_rating(&tiff), Some(5));
    }
}
