# Image Find for Samsung The Frame

`imagefind` is a fast command line tool to scan a photo tree for JPEGs that are:

- **4- or 5-star rated**
- **landscape** (`width > height`)
- **not square** (`width != height`)

…and copy those files to a destination directory as a **flat** set of files using their original filenames.

The default source is `/home/jef/Pictures/photos` and the default destination is `/home/jef/Pictures/best-of`.

Folders named **Dianne King**, **older**, or **adult** are excluded at every depth. Matching ignores case and surrounding whitespace, including the existing `Dianne King ` folder.

## Usage

```text
imagefind [-o|--overwrite] [<src_root> <dst_root>]
```

Run with the default directories:

```text
./target/release/imagefind
```

Or specify both directories explicitly:

```text
./target/release/imagefind /home/jef/Pictures/photos /home/jef/Pictures/best-of
```

Overwrite existing destination files (off by default):

```text
./target/release/imagefind --overwrite
```

For example, a matching source photo at:

```text
/home/jef/Pictures/photos/2026/album/IMG_0001.jpg
```

is written directly to:

```text
/home/jef/Pictures/best-of/IMG_0001.jpg
```

### Notes

- The tool runs in **two phases**:
  1) scan + read JPEG metadata + match the filter (prints a read progress line)
  2) copy/resize the matched set (prints a progress bar with percent and `X/Y`)
- Files are queued in **reverse path order**, so the year folders are searched **2026 → 2025 → … → 2008**. Within each year, paths also sort in reverse order. This uses folder/path names, not EXIF capture dates or modification times; the year range is not hard-coded.
- Metadata reads and writes run in parallel. If matching photos share a filename, the first in that newest-first search order wins, regardless of when workers finish. This also applies with `--overwrite`, so an older duplicate cannot replace the selected newer source in the same run.
- By default the tool **does not overwrite** existing destination files, including files created while the tool is running. Use `-o` or `--overwrite` to replace them. The flag replaces existing outputs without comparing their dates.
- Output contains **no subdirectories or added year prefixes**. An existing `IMG_0001.jpg` is skipped by default regardless of which source year contains that filename.
- It reads rating in this order:
  1) embedded **EXIF/TIFF** tag `0x4746` (Rating), falling back to `0x4749` (RatingPercent, converted to stars)
  2) if EXIF rating is not present, embedded **XMP** (`xmp:Rating`) in JPEG APP1 segments
- It reads JPEG dimensions from the **SOF** header (no full image decode).

### Resize behavior (Samsung The Frame)

- Output images are limited to **3840px on the long edge** (no crop).
- If `max(width,height) < 3840`, the file is **skipped** (not transferred).
- If `max(width,height) == 3840`, the file is **copied as-is** (no re-encode).
- If `max(width,height) > 3840`, it is **resized** and re-encoded as JPEG with **quality=100** and **4:4:4 (no chroma subsampling)**.
- Metadata is **not preserved** in resized outputs.

## Build

This project is implemented in Rust.

```text
cargo build --release
```

The binary will be at:

```text
target/release/imagefind
```

Run:

```text
target/release/imagefind
```

Run tests:

```text
cargo test
```
