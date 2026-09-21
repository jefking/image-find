use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const TARGET_LONG_EDGE: u32 = 3840;
const DEFAULT_SOURCE: &str = "/home/jef/Pictures/photos";
const DEFAULT_DESTINATION: &str = "/home/jef/Pictures/best-of";
const EXCLUDED_DIRS: &[&str] = &["Dianne King", "older", "adult"];

static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy)]
struct ImageMeta {
    width: u16,
    height: u16,
    rating: Option<i32>,
}

#[derive(Debug, Clone)]
struct Job {
    scan_order: usize,
    src_path: PathBuf,
    dst_path: PathBuf,
    meta: ImageMeta,
}

fn main() {
    let (overwrite, src, dst) = match parse_args() {
        Ok(v) => v,
        Err(_) => return usage_and_exit(),
    };

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

    if let Err(e) = fs::create_dir_all(&dst) {
        eprintln!("Failed to create destination directory: {e}");
        std::process::exit(2);
    }

    // Two-phase processing:
    // 1) Read/match: scan JPEGs, read metadata, build an explicit list of jobs.
    // 2) Write: copy/resize those jobs.

    let skip_dir = if dst.starts_with(&src) {
        Some(dst.clone())
    } else {
        None
    };
    let all_files = match collect_jpeg_files(&src, skip_dir.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Walk error: {e}");
            std::process::exit(2);
        }
    };

    let workers = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(1);

    let total = all_files.len();
    if total == 0 {
        eprintln!("No JPEG(s) found under source directory.");
        return;
    }
    eprintln!("Phase 1/2: reading metadata for {total} JPEG(s) using {workers} thread(s)...");

    // ---- Phase 1 (parallel): read metadata and build job list ----
    // NOTE: counters represent COMPLETED files/jobs, not merely dispatched.
    let read_scanned = Arc::new(AtomicUsize::new(0));
    let read_errors = Arc::new(AtomicUsize::new(0));
    let matched_filter = Arc::new(AtomicUsize::new(0));
    let skipped_existing = Arc::new(AtomicUsize::new(0));
    let skipped_too_small = Arc::new(AtomicUsize::new(0));
    let jobs_out: Arc<Mutex<Vec<Job>>> = Arc::new(Mutex::new(Vec::new()));

    let ui_stop = Arc::new(AtomicUsize::new(0));
    let ui_scanned = Arc::clone(&read_scanned);
    let ui_stop2 = Arc::clone(&ui_stop);
    let ui_total = total.max(1);
    let ui_handle = thread::spawn(move || {
        while ui_stop2.load(Ordering::Relaxed) == 0 {
            let done = ui_scanned.load(Ordering::Relaxed).min(ui_total);
            print_progress_line("READ", done, ui_total, None);
            if done >= ui_total {
                break;
            }
            thread::sleep(Duration::from_millis(120));
        }
        print_progress_line(
            "READ",
            ui_scanned.load(Ordering::Relaxed).min(ui_total),
            ui_total,
            None,
        );
        finish_progress_line();
    });

    // One channel per worker (keeps receiver contention low).
    let mut senders: Vec<SyncSender<(usize, PathBuf)>> = Vec::with_capacity(workers);
    let mut handles = Vec::with_capacity(workers);

    for _ in 0..workers {
        let (tx, rx) = mpsc::sync_channel::<(usize, PathBuf)>(2048);
        senders.push(tx);

        let src_root = src.clone();
        let dst_root = dst.clone();
        let read_scanned = Arc::clone(&read_scanned);
        let read_errors = Arc::clone(&read_errors);
        let matched_filter = Arc::clone(&matched_filter);
        let skipped_existing = Arc::clone(&skipped_existing);
        let skipped_too_small = Arc::clone(&skipped_too_small);
        let jobs_out = Arc::clone(&jobs_out);

        handles.push(thread::spawn(move || {
            while let Ok((scan_order, path)) = rx.recv() {
                let mut job_to_push: Option<Job> = None;

                match read_jpeg_meta(&path) {
                    Ok(Some(meta)) => {
                        // Do not copy or process square images.
                        if meta.width != meta.height {
                            let rating_ok = matches!(meta.rating, Some(4 | 5));
                            let landscape = meta.width > meta.height;
                            if rating_ok && landscape {
                                // Enforce minimum resolution: do not transfer images whose long edge is < TARGET_LONG_EDGE.
                                let long_edge = u32::from(meta.width).max(u32::from(meta.height));
                                if long_edge < TARGET_LONG_EDGE {
                                    skipped_too_small.fetch_add(1, Ordering::Relaxed);
                                    read_scanned.fetch_add(1, Ordering::Relaxed);
                                    continue;
                                }

                                matched_filter.fetch_add(1, Ordering::Relaxed);
                                if let Some(dst_path) = dst_path_flat(&path, &src_root, &dst_root) {
                                    if dst_path.exists() && !overwrite {
                                        skipped_existing.fetch_add(1, Ordering::Relaxed);
                                    } else {
                                        job_to_push = Some(Job {
                                            scan_order,
                                            src_path: path,
                                            dst_path,
                                            meta,
                                        });
                                    }
                                }
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(_) => {
                        read_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }

                if let Some(job) = job_to_push {
                    jobs_out.lock().unwrap().push(job);
                }
                read_scanned.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    for (i, p) in all_files.into_iter().enumerate() {
        let _ = senders[i % senders.len()].send((i, p));
    }
    drop(senders);
    for h in handles {
        let _ = h.join();
    }
    ui_stop.store(1, Ordering::Relaxed);
    let _ = ui_handle.join();

    let errors = read_errors.load(Ordering::Relaxed);
    let matched_filter_n = matched_filter.load(Ordering::Relaxed);
    let skipped_existing_n = skipped_existing.load(Ordering::Relaxed);
    let skipped_too_small_n = skipped_too_small.load(Ordering::Relaxed);

    // Drain jobs and de-duplicate destination paths to avoid concurrent writes to the same file.
    let jobs = {
        let mut guard = jobs_out.lock().unwrap();
        std::mem::take(&mut *guard)
    };
    let (jobs, dup_dsts) = deduplicate_jobs(jobs);

    eprintln!(
	        "Phase 1/2 complete: scanned={} matched_filter={} will_process={} skipped_existing={} skipped_too_small={} dup_destinations={} errors={}",
        total,
        matched_filter_n,
        jobs.len(),
        skipped_existing_n,
	        skipped_too_small_n,
        dup_dsts,
        errors
    );

    // ---- Phase 2 (parallel): copy/resize jobs ----
    if jobs.is_empty() {
        eprintln!("Phase 2/2: nothing to do (0 jobs). Done.");
        return;
    }

    eprintln!(
        "Phase 2/2: copy/resize {} image(s) using {workers} thread(s)...",
        jobs.len()
    );
    let jobs = Arc::new(jobs);
    let total_jobs = jobs.len();
    let next_job = Arc::new(AtomicUsize::new(0));
    let write_done = Arc::new(AtomicUsize::new(0));
    let write_ok = Arc::new(AtomicUsize::new(0));
    let write_errors = Arc::new(AtomicUsize::new(0));
    let skipped_at_write = Arc::new(AtomicUsize::new(0));

    let ui_stop = Arc::new(AtomicUsize::new(0));
    let ui_done = Arc::clone(&write_done);
    let ui_stop2 = Arc::clone(&ui_stop);
    let ui_handle = thread::spawn(move || {
        while ui_stop2.load(Ordering::Relaxed) == 0 {
            let done = ui_done.load(Ordering::Relaxed).min(total_jobs);
            print_progress_line("WRITE", done, total_jobs, None);
            if done >= total_jobs {
                break;
            }
            thread::sleep(Duration::from_millis(120));
        }
        print_progress_line(
            "WRITE",
            ui_done.load(Ordering::Relaxed).min(total_jobs),
            total_jobs,
            None,
        );
        finish_progress_line();
    });

    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let jobs = Arc::clone(&jobs);
        let next_job = Arc::clone(&next_job);
        let write_done = Arc::clone(&write_done);
        let write_ok = Arc::clone(&write_ok);
        let write_errors = Arc::clone(&write_errors);
        let skipped_at_write = Arc::clone(&skipped_at_write);

        handles.push(thread::spawn(move || loop {
            let idx = next_job.fetch_add(1, Ordering::Relaxed);
            if idx >= jobs.len() {
                break;
            }
            let job = &jobs[idx];
            match copy_or_resize_matched_jpeg(&job.src_path, &job.dst_path, job.meta, overwrite) {
                Ok(true) => {
                    write_ok.fetch_add(1, Ordering::Relaxed);
                }
                Ok(false) => {
                    skipped_at_write.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    write_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
            write_done.fetch_add(1, Ordering::Relaxed);
        }));
    }

    for h in handles {
        let _ = h.join();
    }
    ui_stop.store(1, Ordering::Relaxed);
    let _ = ui_handle.join();

    let processed_ok = write_ok.load(Ordering::Relaxed);
    let write_errors_n = write_errors.load(Ordering::Relaxed);
    eprintln!(
        "Done: processed_ok={} skipped_at_write={} errors={}",
        processed_ok,
        skipped_at_write.load(Ordering::Relaxed),
        errors + write_errors_n
    );
}

fn parse_args() -> Result<(bool, PathBuf, PathBuf), ()> {
    parse_args_from(std::env::args_os().skip(1))
}

fn parse_args_from(
    args: impl IntoIterator<Item = OsString>,
) -> Result<(bool, PathBuf, PathBuf), ()> {
    let mut overwrite = false;
    let mut positional = Vec::new();

    for arg in args {
        match arg.to_str() {
            Some("-o" | "--overwrite") => overwrite = true,
            Some(s) if s.starts_with('-') => return Err(()),
            _ => positional.push(arg),
        }
    }
    match positional.as_slice() {
        [] => Ok((overwrite, DEFAULT_SOURCE.into(), DEFAULT_DESTINATION.into())),
        [src, dst] => Ok((overwrite, src.into(), dst.into())),
        _ => Err(()),
    }
}

fn absolutize(p: &Path) -> io::Result<PathBuf> {
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(p))
    }
}

fn usage_and_exit() {
    eprintln!("Usage: imagefind [-o|--overwrite] [<src_root> <dst_root>]");
    eprintln!("  -o, --overwrite   overwrite destination files (default: skip existing)");
    eprintln!("  Default source: {DEFAULT_SOURCE}");
    eprintln!("  Default destination: {DEFAULT_DESTINATION}");
    eprintln!("  Excluded folders: Dianne King, older, adult (at any depth)");
    eprintln!("  Searches paths in reverse order (newest year first).");
    eprintln!(
        "Copies landscape JPEGs rated 4 or 5 stars, with a long edge of at least {TARGET_LONG_EDGE}px, to a flat destination."
    );
    std::process::exit(2);
}

fn deduplicate_jobs(mut jobs: Vec<Job>) -> (Vec<Job>, usize) {
    // Metadata workers finish out of order; restore newest-first priority before
    // selecting a single source for each destination, even with --overwrite.
    jobs.sort_unstable_by_key(|job| job.scan_order);
    let original_len = jobs.len();
    let mut seen = HashSet::new();
    jobs.retain(|job| seen.insert(job.dst_path.clone()));
    let duplicates = original_len - jobs.len();
    (jobs, duplicates)
}

fn render_bar(current: usize, total: usize, width: usize) -> (usize, String) {
    let total = total.max(1);
    let current = current.min(total);
    let pct = (current * 100) / total;
    let filled = (current * width) / total;
    let mut s = String::with_capacity(width);
    for i in 0..width {
        s.push(if i < filled { '#' } else { '-' });
    }
    (pct, s)
}

fn format_path_tail(p: &Path, max_len: usize) -> String {
    let s = p.display().to_string();
    if s.len() <= max_len {
        return s;
    }
    let keep = max_len.saturating_sub(3);
    let tail = &s[s.len().saturating_sub(keep)..];
    format!("...{tail}")
}

fn print_progress_line(phase: &str, current: usize, total: usize, path: Option<&Path>) {
    let (pct, bar) = render_bar(current, total, 30);
    let path_s = path.map(|p| format_path_tail(p, 80)).unwrap_or_default();
    let msg = if path_s.is_empty() {
        format!("[{bar}] {pct:3}% {phase} {current}/{total}")
    } else {
        format!("[{bar}] {pct:3}% {phase} {current}/{total}  {path_s}")
    };
    let mut out = io::stdout();
    let _ = write!(out, "\r{msg: <140}");
    let _ = out.flush();
}

fn finish_progress_line() {
    let mut out = io::stdout();
    let _ = writeln!(out);
    let _ = out.flush();
}

fn copy_or_resize_matched_jpeg(
    src_path: &Path,
    dst_path: &Path,
    meta: ImageMeta,
    overwrite: bool,
) -> io::Result<bool> {
    let w = meta.width as u32;
    let h = meta.height as u32;
    let long_edge = w.max(h);

    // Minimum resolution requirement: do not transfer images whose long edge is below the target.
    if long_edge < TARGET_LONG_EDGE {
        return Ok(false);
    }

    if !overwrite && dst_path.try_exists()? {
        return Ok(false);
    }

    // Only re-encode when needed.
    if long_edge <= TARGET_LONG_EDGE {
        return write_output(dst_path, overwrite, |output| {
            io::copy(&mut fs::File::open(src_path)?, output)?;
            Ok(())
        });
    }

    let (new_w, new_h) = resized_dims_long_edge(w, h, TARGET_LONG_EDGE);
    resize_and_encode_jpeg(src_path, dst_path, new_w, new_h, overwrite)
}

fn resized_dims_long_edge(width: u32, height: u32, target_long_edge: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (target_long_edge.max(1), target_long_edge.max(1));
    }
    if width >= height {
        let new_w = target_long_edge.max(1);
        let new_h = (((height as f64) * (new_w as f64) / (width as f64)).round() as i64)
            .clamp(1, i64::from(u32::MAX)) as u32;
        (new_w, new_h)
    } else {
        let new_h = target_long_edge.max(1);
        let new_w = (((width as f64) * (new_h as f64) / (height as f64)).round() as i64)
            .clamp(1, i64::from(u32::MAX)) as u32;
        (new_w, new_h)
    }
}

fn resize_and_encode_jpeg(
    src_path: &Path,
    dst_path: &Path,
    new_w: u32,
    new_h: u32,
    overwrite: bool,
) -> io::Result<bool> {
    // Decode source (read-only). We intentionally do not preserve metadata.
    let img = image::open(src_path).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Failed to decode JPEG {}: {e}", src_path.display()),
        )
    })?;

    let resized = img.resize_exact(new_w, new_h, image::imageops::FilterType::Lanczos3);
    let rgb = resized.to_rgb8();
    let (w, h) = rgb.dimensions();
    let raw = rgb.into_raw();

    let w16 = u16::try_from(w)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Resized width exceeds u16"))?;
    let h16 = u16::try_from(h)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Resized height exceeds u16"))?;

    write_output(dst_path, overwrite, |output| {
        let mut buffered = io::BufWriter::new(output);
        let mut enc = jpeg_encoder::Encoder::new(&mut buffered, 100);
        enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1); // 4:4:4 (no subsampling)
        enc.encode(&raw, w16, h16, jpeg_encoder::ColorType::Rgb)
            .map_err(|e| io::Error::other(format!("JPEG encode failed: {e}")))?;
        buffered.flush()
    })
}

fn write_output(
    dst_path: &Path,
    overwrite: bool,
    write: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> io::Result<bool> {
    // Stage both copies and resized JPEGs next to the destination. create_new
    // also prevents separate runs from sharing or truncating a temporary file.
    let (tmp_path, mut output) = loop {
        let path = tmp_path_for(dst_path);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => break (path, file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    let written = write(&mut output).and_then(|_| output.flush());
    drop(output);
    let result = written.and_then(|_| {
        if overwrite {
            fs::rename(&tmp_path, dst_path).map(|_| true)
        } else {
            // Unlike rename/copy, hard_link atomically refuses an existing name,
            // including a file created after the initial existence check.
            match fs::hard_link(&tmp_path, dst_path) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
                Err(e) => Err(e),
            }
        }
    });
    let _ = fs::remove_file(&tmp_path);
    result
}

fn tmp_path_for(dst_path: &Path) -> PathBuf {
    let parent = dst_path.parent().unwrap_or_else(|| Path::new("."));
    let fname = dst_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "output.jpg".to_string());
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".{fname}.tmp{}-{n}", std::process::id()))
}

fn dst_path_flat(src_path: &Path, src_root: &Path, dst_root: &Path) -> Option<PathBuf> {
    src_path.strip_prefix(src_root).ok()?;
    Some(dst_root.join(src_path.file_name()?))
}

fn collect_jpeg_files(src_root: &Path, skip_dir: Option<&Path>) -> io::Result<Vec<PathBuf>> {
    let mut dirs: Vec<PathBuf> = vec![src_root.to_path_buf()];
    let mut out: Vec<PathBuf> = Vec::new();

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
                let excluded = entry.file_name().to_str().is_some_and(|name| {
                    EXCLUDED_DIRS
                        .iter()
                        .any(|excluded| name.trim().eq_ignore_ascii_case(excluded))
                });
                if !excluded {
                    dirs.push(path);
                }
            } else if ft.is_file() && is_jpeg_path(&path) {
                out.push(path);
            }
        }
    }
    out.sort_unstable_by(|a, b| b.cmp(a));
    Ok(out)
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
    // Prefer EXIF rating over XMP, regardless of segment order.
    let mut rating_exif: Option<i32> = None;
    let mut rating_xmp: Option<i32> = None;

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
            // Prefer EXIF rating (common for many workflows). If absent, fall back to XMP.
            if rating_exif.is_none() {
                if let Some(x) = parse_app1_exif_rating(&data) {
                    rating_exif = Some(x);
                }
            }
            if rating_xmp.is_none() {
                if let Some(x) = parse_app1_xmp_rating(&data) {
                    rating_xmp = Some(x);
                }
            }
        } else {
            discard(r, remaining)?;
        }

        // If we already have dimensions and an EXIF rating, we can stop early.
        if width.is_some() && height.is_some() && rating_exif.is_some() {
            break;
        }
    }

    let rating = rating_exif.or(rating_xmp);

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
    let mut rating_percent = None;
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
                let rating = if total <= 4 {
                    read_inline_value(value, typ, little)
                } else {
                    let data_off = read_u32(tiff, off + 8, little)? as usize;
                    read_typed_value(tiff, data_off, typ, little)
                };
                if tag == 0x4746 {
                    return rating;
                }
                // Windows RatingPercent uses a 1–99 scale, not a star count.
                // https://learn.microsoft.com/en-us/windows/win32/properties/props-system-rating
                rating_percent = rating.and_then(|value| match value {
                    0 => Some(0),
                    1..=12 => Some(1),
                    13..=37 => Some(2),
                    38..=62 => Some(3),
                    63..=87 => Some(4),
                    88..=99 => Some(5),
                    _ => None,
                });
            }
        }

        off += 12;
    }
    rating_percent
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

    #[test]
    fn defaults_to_photos_and_best_of_without_overwriting() {
        assert_eq!(
            parse_args_from([]),
            Ok((false, DEFAULT_SOURCE.into(), DEFAULT_DESTINATION.into()))
        );
        for flag in ["-o", "--overwrite"] {
            assert_eq!(
                parse_args_from([flag.into()]),
                Ok((true, DEFAULT_SOURCE.into(), DEFAULT_DESTINATION.into()))
            );
        }
        assert_eq!(
            parse_args_from(["/source".into(), "/destination".into()]),
            Ok((false, "/source".into(), "/destination".into()))
        );
        assert!(parse_args_from(["/source".into()]).is_err());
        assert!(parse_args_from(["--unknown".into()]).is_err());
    }

    #[test]
    fn duplicate_selection_uses_scan_order_instead_of_worker_completion_order() {
        let meta = ImageMeta {
            width: 3840,
            height: 2160,
            rating: Some(4),
        };
        let jobs = [
            (2, "/photos/2008/shared.jpg"),
            (0, "/photos/2026/shared.jpg"),
            (1, "/photos/2025/shared.jpg"),
        ]
        .into_iter()
        .map(|(scan_order, path)| Job {
            scan_order,
            src_path: path.into(),
            dst_path: "/best-of/shared.jpg".into(),
            meta,
        })
        .collect();
        let (jobs, duplicates) = deduplicate_jobs(jobs);
        assert_eq!(duplicates, 2);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].src_path, Path::new("/photos/2026/shared.jpg"));
    }

    #[test]
    fn output_created_during_processing_is_preserved_and_temp_files_are_cleaned() {
        let directory =
            std::env::temp_dir().join(format!("imagefind-write-test-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let dst = directory.join("photo.jpg");
        let copied = write_output(&dst, false, |output| {
            output.write_all(b"older photo")?;
            // Simulate another process creating the destination after scanning.
            fs::write(&dst, b"existing photo")
        })
        .unwrap();
        assert!(!copied);
        assert_eq!(fs::read(&dst).unwrap(), b"existing photo");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);

        assert!(write_output(&dst, true, |output| output.write_all(b"replacement")).unwrap());
        assert_eq!(fs::read(&dst).unwrap(), b"replacement");

        assert!(write_output(&dst, true, |output| {
            output.write_all(b"incomplete")?;
            Err(io::Error::other("simulated encoder failure"))
        })
        .is_err());
        assert_eq!(fs::read(&dst).unwrap(), b"replacement");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(&directory).unwrap();
    }

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

    fn minimal_exif_app1_with_rating(rating: u16) -> Vec<u8> {
        // Minimal TIFF with IFD0 containing tag 0x4746 (Rating) type SHORT count 1 value=<rating>.
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes()); // ifd0 offset
        tiff.extend_from_slice(&1u16.to_le_bytes()); // one entry
        tiff.extend_from_slice(&0x4746u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&rating.to_le_bytes());
        tiff.extend_from_slice(&0u16.to_le_bytes()); // padding to 4 bytes
        tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD

        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&tiff);
        app1
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
    fn prefers_exif_rating_even_if_xmp_comes_first() {
        // SOI
        let mut jpg = vec![0xFF, 0xD8];

        // APP1 XMP rating 1
        let xmp_jpg = minimal_jpeg_with_sof_and_xmp(4000, 3000, 1);
        // xmp_jpg already contains SOI/EOI; extract only the first APP1 segment from it.
        // It starts at offset 2 (after SOI) and ends before SOF0 marker.
        let sof_pos = xmp_jpg.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        jpg.extend_from_slice(&xmp_jpg[2..sof_pos]);

        // APP1 EXIF rating 5
        let exif = minimal_exif_app1_with_rating(5);
        let len = (exif.len() + 2) as u16;
        jpg.extend_from_slice(&[0xFF, 0xE1]);
        jpg.extend_from_slice(&len.to_be_bytes());
        jpg.extend_from_slice(&exif);

        // SOF0 + EOI from xmp_jpg
        jpg.extend_from_slice(&xmp_jpg[sof_pos..]);

        let mut r = io::Cursor::new(jpg);
        let meta = parse_jpeg_meta(&mut r).unwrap().unwrap();
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

    #[test]
    fn converts_exif_rating_percent_to_stars() {
        for (percent, stars) in [
            (0, 0),
            (1, 1),
            (25, 2),
            (50, 3),
            (63, 4),
            (75, 4),
            (87, 4),
            (88, 5),
            (99, 5),
        ] {
            let mut app1 = minimal_exif_app1_with_rating(percent);
            // Replace Rating with RatingPercent in the TIFF IFD entry.
            app1[16..18].copy_from_slice(&0x4749u16.to_le_bytes());
            assert_eq!(parse_app1_exif_rating(&app1), Some(stars));
        }
    }

    #[test]
    fn flattens_dest_path_with_original_filename() {
        let src_root = PathBuf::from("/X");
        let dst_root = PathBuf::from("/Y");
        let src_path = PathBuf::from("/X/2024/IMG_0001.jpg");
        let out = dst_path_flat(&src_path, &src_root, &dst_root).unwrap();
        assert_eq!(out, PathBuf::from("/Y/IMG_0001.jpg"));
    }

    #[test]
    fn resized_dims_long_edge_landscape() {
        let (w, h) = resized_dims_long_edge(6000, 4000, TARGET_LONG_EDGE);
        assert_eq!((w, h), (3840, 2560));
    }

    #[test]
    fn resized_dims_long_edge_portrait() {
        let (w, h) = resized_dims_long_edge(3000, 6000, TARGET_LONG_EDGE);
        assert_eq!((w, h), (1920, 3840));
    }

    #[test]
    fn resized_dims_long_edge_rounding() {
        let (w, h) = resized_dims_long_edge(5000, 3333, TARGET_LONG_EDGE);
        assert_eq!((w, h), (3840, 2560));
    }
}
