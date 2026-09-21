use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "imagefind-cli-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn xmp_segment(rating: i32) -> Vec<u8> {
    let mut app1 = b"http://ns.adobe.com/xap/1.0/\0".to_vec();
    app1.extend_from_slice(format!("<xmp:Rating>{rating}</xmp:Rating>").as_bytes());
    let mut segment = vec![0xff, 0xe1];
    segment.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
    segment.extend(app1);
    segment
}

// Header-only JPEGs exercise metadata scanning and the byte-for-byte copy path.
fn photo(path: &Path, rating: i32, width: u16, height: u16) -> Vec<u8> {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut bytes = vec![0xff, 0xd8];
    bytes.extend(xmp_segment(rating));
    bytes.extend_from_slice(&[0xff, 0xc0, 0, 7, 8]);
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&[0xff, 0xd9]);
    fs::write(path, &bytes).unwrap();
    bytes
}

fn run(src: &Path, dst: &Path, overwrite: bool) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_imagefind"));
    if overwrite {
        command.arg("--overwrite");
    }
    let output = command.arg(src).arg(dst).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("errors=0"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn copies_four_and_five_stars_flat_with_exclusions_and_newest_duplicates() {
    let temp = TestDirectory::new();
    let src = temp.0.join("photos");
    let dst = temp.0.join("best-of");
    let newest = photo(&src.join("2026/album/shared.jpg"), 4, 3840, 2160);
    photo(&src.join("2025/shared.jpg"), 5, 3840, 2160);
    photo(&src.join("2008/shared.jpg"), 5, 3840, 2000);
    let five = photo(&src.join("2008/album/five.JPEG"), 5, 3840, 2160);
    let direct = photo(&src.join("direct.jpg"), 4, 3840, 2160);
    photo(&src.join("2026/three.jpg"), 3, 3840, 2160);
    photo(&src.join("2026/unrated.jpg"), 0, 3840, 2160);
    photo(&src.join("2026/square.jpg"), 5, 3840, 3840);
    photo(&src.join("2026/portrait.jpg"), 5, 2160, 3840);
    photo(&src.join("2026/small.jpg"), 5, 2000, 1000);
    for (i, folder) in [
        "Dianne King",
        "Dianne King ",
        "older",
        "adult",
        "2026/album/adult",
        "2025/OLDER",
    ]
    .into_iter()
    .enumerate()
    {
        photo(
            &src.join(format!("{folder}/excluded-{i}.jpg")),
            5,
            3840,
            2160,
        );
    }

    run(&src, &dst, false);
    assert_eq!(fs::read(dst.join("shared.jpg")).unwrap(), newest);
    assert_eq!(fs::read(dst.join("five.JPEG")).unwrap(), five);
    assert_eq!(fs::read(dst.join("direct.jpg")).unwrap(), direct);
    assert_eq!(fs::read_dir(&dst).unwrap().count(), 3);
    assert!(fs::read_dir(&dst)
        .unwrap()
        .all(|entry| entry.unwrap().file_type().unwrap().is_file()));

    fs::write(dst.join("shared.jpg"), b"existing photo").unwrap();
    run(&src, &dst, false);
    assert_eq!(fs::read(dst.join("shared.jpg")).unwrap(), b"existing photo");
    run(&src, &dst, true);
    assert_eq!(fs::read(dst.join("shared.jpg")).unwrap(), newest);
}

#[test]
fn resized_photos_also_respect_overwrite() {
    let temp = TestDirectory::new();
    let src = temp.0.join("photos/2026");
    let dst = temp.0.join("best-of");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir(&dst).unwrap();
    let mut encoded = Vec::new();
    jpeg_encoder::Encoder::new(&mut encoded, 100)
        .encode(
            &vec![128; 4000 * 2 * 3],
            4000,
            2,
            jpeg_encoder::ColorType::Rgb,
        )
        .unwrap();
    encoded.splice(2..2, xmp_segment(4));
    fs::write(src.join("wide.jpg"), encoded).unwrap();

    run(src.parent().unwrap(), &dst, false);
    assert_eq!(
        image::image_dimensions(dst.join("wide.jpg")).unwrap(),
        (3840, 2)
    );
    fs::write(dst.join("wide.jpg"), b"existing photo").unwrap();
    run(src.parent().unwrap(), &dst, false);
    assert_eq!(fs::read(dst.join("wide.jpg")).unwrap(), b"existing photo");
    run(src.parent().unwrap(), &dst, true);
    assert_eq!(
        image::image_dimensions(dst.join("wide.jpg")).unwrap(),
        (3840, 2)
    );
    assert_eq!(fs::read_dir(&dst).unwrap().count(), 1);
}

#[test]
fn destination_inside_source_is_not_scanned() {
    let temp = TestDirectory::new();
    let src = temp.0.join("photos");
    let dst = src.join("best-of");
    photo(&src.join("2026/source.jpg"), 4, 3840, 2160);
    let existing = photo(&dst.join("existing.jpg"), 5, 3840, 2160);
    run(&src, &dst, false);
    assert_eq!(fs::read_dir(&dst).unwrap().count(), 2);
    assert_eq!(fs::read(dst.join("existing.jpg")).unwrap(), existing);
}
