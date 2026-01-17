# Image Find for Samsung The Frame

`imagefind` is a fast command line tool to scan a photo tree for JPEGs that are:

- **5-star rated** (`rating == 5`)
- **landscape** (`width > height`)
- **not square** (`width != height`)

…and copy those files to a destination directory as a **flat** set of files named:

`<YYYY>-<original_filename>.jpg`

## Usage

```text
imagefind [-o|--overwrite] <src_root> <dst_root>
```

Example:

```text
imagefind /home/jef/Pictures/photos/ /home/jef/Pictures/theframe
```

Overwrite existing destination files:

```text
imagefind -o /home/jef/Pictures/photos/ /home/jef/Pictures/theframe
```

If your images are stored as:

```text
./X/YYYY/*.jpg
```

then matched files will be copied to (no subdirectories):

```text
./Y/YYYY-<original_filename>.jpg
```

### Notes

- By default the tool **does not overwrite** existing destination files (it will count them as matched but not copied). Use `-o` to overwrite.
- Because the destination is **flat**, if you have multiple images with the same filename under the same `YYYY` (e.g. `./X/2024/a/IMG_0001.jpg` and `./X/2024/b/IMG_0001.jpg`), they will map to the same destination name (`2024-IMG_0001.jpg`). In that case the later one will be **skipped** (or **overwritten** with `-o`).
- It reads rating in this order:
  1) embedded **EXIF/TIFF** tag `0x4746` (Rating) or `0x4749` (RatingPercent)
  2) if EXIF rating is not present, embedded **XMP** (`xmp:Rating`) in JPEG APP1 segments
- It reads JPEG dimensions from the **SOF** header (no full image decode).

### Resize behavior (Samsung The Frame)

- Output images are limited to **3840px on the long edge** (no crop).
- If `max(width,height) <= 3840`, the file is **copied as-is** (no re-encode).
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
target/release/imagefind ./X ./Y
```