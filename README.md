# image-find

`imagefind` is a fast command line tool to scan a photo tree for JPEGs that are:

- **5-star rated** (`rating == 5`)
- **landscape** (`width > height`)

…and copy those files to a destination directory while preserving the relative path.

## Usage

```text
imagefind <src_root> <dst_root>
```

Example:

```text
imagefind /home/jef/Pictures/photos/ /home/jef/Pictures/theframe
```

If your images are stored as:

```text
./X/YYYY/*.jpg
```

then matched files will be copied to:

```text
./Y/YYYY/*.jpg
```

### Notes

- The tool **does not overwrite** existing destination files (it will count them as matched but not copied).
- It reads rating in this order:
  1) embedded **EXIF/TIFF** tag `0x4746` (Rating) or `0x4749` (RatingPercent)
  2) if EXIF rating is not present, embedded **XMP** (`xmp:Rating`) in JPEG APP1 segments
- It reads JPEG dimensions from the **SOF** header (no full image decode).

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