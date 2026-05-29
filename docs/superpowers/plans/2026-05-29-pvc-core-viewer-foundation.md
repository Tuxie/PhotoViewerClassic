# Core Viewer Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `photoviewer <path>` binary that starts fast, shows the first image (correctly oriented, fit-to-window) almost immediately, and navigates the rest of the directory with the arrow keys (and `H`/`L`), wrapping at the ends.

**Architecture:** A Cargo workspace with pure-logic library crates (`imageset` for the directory set + natural-sort + wrap navigation; `decode` for RGBA8 decode + EXIF-orientation normalization + downscaling) and a thin `app` binary that wires Slint to those crates. The UI thread shows the window immediately; image decoding runs on worker threads and pushes a `Send`-safe `SharedPixelBuffer` back via `upgrade_in_event_loop`. This is build step 1–2 (display + navigation) of the larger Phase 1; interactive zoom/pan/rotate, the auto-hiding chrome, persistence, and the tag/rating subsystem are later plans.

**Tech Stack:** Rust (edition 2021), Slint 1.16 (`backend-winit` + `renderer-femtovg`, software renderer disabled), image-rs 0.25 (decode only), kamadak-exif 0.6 (orientation read). All pure-Rust, no C/C++ deps.

---

## File structure

| File | Responsibility |
|---|---|
| `./Cargo.toml` | Workspace manifest; shared dep versions; release profile |
| `./.gitignore` | Ignore `/target` |
| `./crates/imageset/Cargo.toml`, `src/lib.rs` | Directory scan, supported-extension filter, natural-sort, wrap-around navigation cursor (pure logic) |
| `./crates/decode/Cargo.toml`, `src/lib.rs` | Decode to RGBA8, EXIF orientation read, orientation normalization (pixel-buffer transform), downscale-to-fit |
| `./app/Cargo.toml` | Binary crate; depends on `slint`, `imageset`, `decode`; `slint-build` build-dep |
| `./app/build.rs` | Compiles `ui/main.slint` |
| `./app/ui/main.slint` | Window, image surface, key handling |
| `./app/src/main.rs` | Backend pinning, CLI arg, worker→UI decode pipeline, navigation wiring |

Conventions for every task: run `cargo` from the workspace root. Pure-logic crates are unit-tested (TDD); the GUI wiring tasks are verified by `cargo build` + a manual run (a window can't open in headless CI — that's expected, the fast-startup check is a local dev check).

---

## Task 1: Workspace scaffold + runnable window

**Files:**
- Create: `./Cargo.toml`
- Create: `./.gitignore`
- Create: `./app/Cargo.toml`
- Create: `./app/build.rs`
- Create: `./app/ui/main.slint`
- Create: `./app/src/main.rs`

- [ ] **Step 1: Create the workspace manifest**

`./Cargo.toml`:
```toml
[workspace]
resolver = "2"
members = ["app"]

[workspace.dependencies]
slint = { version = "1.16", default-features = false, features = [
    "backend-winit",
    "renderer-femtovg",
    "compat-1-2",
] }
slint-build = "1.16"
image = { version = "0.25", features = ["gif", "webp"] }
kamadak-exif = "0.6"

[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
panic = "abort"
strip = true
```

> `default-features = false` on `slint` is required — it drops `renderer-software` (which can't scale or rotate images and is in Slint's default set). Members are added in later tasks as their crates are created.

- [ ] **Step 2: Create `.gitignore`**

`./.gitignore`:
```gitignore
/target
```

- [ ] **Step 3: Create the app crate manifest**

`./app/Cargo.toml`:
```toml
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "photoviewer"
path = "src/main.rs"

[dependencies]
slint = { workspace = true }

[build-dependencies]
slint-build = { workspace = true }
```

- [ ] **Step 4: Create the build script**

`./app/build.rs`:
```rust
fn main() {
    slint_build::compile("ui/main.slint").expect("Slint build failed");
}
```

- [ ] **Step 5: Create the minimal UI**

`./app/ui/main.slint`:
```slint
export component AppWindow inherits Window {
    title: "Photo Viewer";
    background: black;
    preferred-width: 1024px;
    preferred-height: 768px;

    callback quit();
    forward-focus: keys;

    keys := FocusScope {
        key-pressed(event) => {
            if (event.text == Key.Escape || event.text == "q" || event.text == "Q") {
                root.quit();
                return EventResult.accept;
            }
            return EventResult.reject;
        }
    }
}
```

- [ ] **Step 6: Create main.rs**

`./app/src/main.rs`:
```rust
slint::include_modules!();

use slint::ComponentHandle;

fn main() -> Result<(), slint::PlatformError> {
    // Deterministically pin the renderer/backend BEFORE creating any component,
    // to avoid probe-and-fallback latency at startup.
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("femtovg".into())
        .select()?;

    let ui = AppWindow::new()?;
    ui.on_quit(|| {
        let _ = slint::quit_event_loop();
    });
    ui.run()
}
```

- [ ] **Step 7: Build**

Run: `cargo build`
Expected: compiles successfully (downloads slint/winit/femtovg on first build).

- [ ] **Step 8: Manual smoke run**

Run: `cargo run -p app`
Expected: a black 1024×768 window titled "Photo Viewer" appears; pressing `Esc` or `Q` closes it and the process exits 0.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml .gitignore app/
git commit -m "feat: workspace scaffold + runnable Slint window"
```

---

## Task 2: `imageset` crate — directory set + natural sort + wrap navigation

> **Post-review note:** during code review, `ImageSet::next`/`prev` were renamed to
> **`advance`/`retreat`** to avoid shadowing `Iterator::next` (clippy
> `should_implement_trait`). Tasks 5 and 6 below call `advance()`/`retreat()`
> accordingly (the code blocks there still show the old names — use `advance`/`retreat`).

**Files:**
- Create: `./crates/imageset/Cargo.toml`
- Create: `./crates/imageset/src/lib.rs`
- Modify: `./Cargo.toml` (add member)

- [ ] **Step 1: Add the crate to the workspace + create its manifest**

Modify `./Cargo.toml` members:
```toml
members = ["app", "crates/imageset"]
```

`./crates/imageset/Cargo.toml`:
```toml
[package]
name = "imageset"
version = "0.1.0"
edition = "2021"

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Write the failing tests**

`./crates/imageset/src/lib.rs` (tests only for now):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn supported_extensions_are_case_insensitive() {
        assert!(is_supported(&PathBuf::from("a.JPG")));
        assert!(is_supported(&PathBuf::from("a.jpeg")));
        assert!(is_supported(&PathBuf::from("a.png")));
        assert!(is_supported(&PathBuf::from("a.WebP")));
        assert!(is_supported(&PathBuf::from("a.gif")));
        assert!(!is_supported(&PathBuf::from("a.txt")));
        assert!(!is_supported(&PathBuf::from("a")));
    }

    #[test]
    fn natural_sort_orders_numbers_numerically_and_ignores_case() {
        let mut v = ["img10.jpg", "img2.jpg", "IMG1.jpg"];
        v.sort_by(|a, b| natural_cmp_ci(a, b));
        assert_eq!(v, ["IMG1.jpg", "img2.jpg", "img10.jpg"]);
    }

    #[test]
    fn imageset_next_prev_wrap() {
        let files = vec![
            PathBuf::from("/d/a.jpg"),
            PathBuf::from("/d/b.jpg"),
            PathBuf::from("/d/c.jpg"),
        ];
        let mut set = ImageSet::new(files, 0);
        assert_eq!(set.len(), 3);
        assert_eq!(set.current(), Some(PathBuf::from("/d/a.jpg")));
        assert_eq!(set.next(), Some(PathBuf::from("/d/b.jpg")));
        assert_eq!(set.next(), Some(PathBuf::from("/d/c.jpg")));
        assert_eq!(set.next(), Some(PathBuf::from("/d/a.jpg"))); // wrap forward
        assert_eq!(set.prev(), Some(PathBuf::from("/d/c.jpg"))); // wrap backward
    }

    #[test]
    fn empty_set_is_safe() {
        let mut set = ImageSet::empty();
        assert!(set.is_empty());
        assert_eq!(set.current(), None);
        assert_eq!(set.next(), None);
        assert_eq!(set.prev(), None);
    }

    #[test]
    fn scan_dir_filters_and_sorts_and_from_file_positions() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["b2.jpg", "b10.jpg", "a.png", "notes.txt"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let files = scan_dir(dir.path());
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a.png", "b2.jpg", "b10.jpg"]); // txt excluded, natural order

        let set = ImageSet::from_file(&dir.path().join("b2.jpg"));
        assert_eq!(set.len(), 3);
        assert_eq!(set.position(), 1); // index of b2.jpg in [a, b2, b10]
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p imageset`
Expected: FAIL — `cannot find function/type` (`is_supported`, `natural_cmp_ci`, `ImageSet`, `scan_dir` not defined).

- [ ] **Step 4: Implement the crate**

Prepend to `./crates/imageset/src/lib.rs` (above the `#[cfg(test)]` module):
```rust
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

const SUPPORTED: [&str; 5] = ["jpg", "jpeg", "png", "webp", "gif"];

/// True if the path has a supported image extension (case-insensitive).
pub fn is_supported(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => {
            let lower = ext.to_ascii_lowercase();
            SUPPORTED.contains(&lower.as_str())
        }
        None => false,
    }
}

/// Natural, case-insensitive comparison: "img2" < "img10", "A" == "a".
/// ASCII digit runs compare by numeric value; everything else by ASCII-lowercased char.
pub fn natural_cmp_ci(a: &str, b: &str) -> Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) => {
                if ca.is_ascii_digit() && cb.is_ascii_digit() {
                    let mut na = String::new();
                    while let Some(&c) = ai.peek() {
                        if c.is_ascii_digit() {
                            na.push(c);
                            ai.next();
                        } else {
                            break;
                        }
                    }
                    let mut nb = String::new();
                    while let Some(&c) = bi.peek() {
                        if c.is_ascii_digit() {
                            nb.push(c);
                            bi.next();
                        } else {
                            break;
                        }
                    }
                    let ta = na.trim_start_matches('0');
                    let tb = nb.trim_start_matches('0');
                    let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                } else {
                    let ord = ca.to_ascii_lowercase().cmp(&cb.to_ascii_lowercase());
                    if ord != Ordering::Equal {
                        return ord;
                    }
                    ai.next();
                    bi.next();
                }
            }
        }
    }
}

/// All supported image files directly in `dir`, natural-sorted by file name.
pub fn scan_dir(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file() && is_supported(p))
            .collect(),
        Err(_) => Vec::new(),
    };
    files.sort_by(|a, b| {
        let an = a.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let bn = b.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        natural_cmp_ci(&an, &bn)
    });
    files
}

/// A navigable, wrap-around set of image paths with a current cursor.
pub struct ImageSet {
    files: Vec<PathBuf>,
    idx: usize,
}

impl ImageSet {
    pub fn new(files: Vec<PathBuf>, start: usize) -> Self {
        let idx = if files.is_empty() { 0 } else { start.min(files.len() - 1) };
        ImageSet { files, idx }
    }

    pub fn empty() -> Self {
        ImageSet { files: Vec::new(), idx: 0 }
    }

    /// Scan the file's directory and position the cursor on that file.
    /// Falls back to a single-element set if the directory can't be scanned.
    pub fn from_file(path: &Path) -> Self {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let files = scan_dir(dir);
        if files.is_empty() {
            return ImageSet { files: vec![path.to_path_buf()], idx: 0 };
        }
        let idx = files.iter().position(|p| p == path).unwrap_or(0);
        ImageSet { files, idx }
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// 0-based index of the current cursor.
    pub fn position(&self) -> usize {
        self.idx
    }

    pub fn current(&self) -> Option<PathBuf> {
        self.files.get(self.idx).cloned()
    }

    /// Advance with wrap-around; returns the new current path.
    pub fn next(&mut self) -> Option<PathBuf> {
        if self.files.is_empty() {
            return None;
        }
        self.idx = (self.idx + 1) % self.files.len();
        self.current()
    }

    /// Step back with wrap-around; returns the new current path.
    pub fn prev(&mut self) -> Option<PathBuf> {
        if self.files.is_empty() {
            return None;
        }
        self.idx = (self.idx + self.files.len() - 1) % self.files.len();
        self.current()
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p imageset`
Expected: PASS (5 tests).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/imageset/
git commit -m "feat(imageset): directory scan, natural sort, wrap navigation"
```

---

## Task 3: `decode` crate — RGBA8 decode + orientation + downscale

**Files:**
- Create: `./crates/decode/Cargo.toml`
- Create: `./crates/decode/src/lib.rs`
- Modify: `./Cargo.toml` (add member)

- [ ] **Step 1: Add the crate to the workspace + create its manifest**

Modify `./Cargo.toml` members:
```toml
members = ["app", "crates/imageset", "crates/decode"]
```

`./crates/decode/Cargo.toml`:
```toml
[package]
name = "decode"
version = "0.1.0"
edition = "2021"

[dependencies]
image = { workspace = true }
kamadak-exif = { workspace = true }

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Write the failing tests**

`./crates/decode/src/lib.rs` (tests only for now):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn two_px() -> RgbaImage {
        // width 2, height 1: left RED, right BLUE
        let mut img = RgbaImage::new(2, 1);
        img.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        img.put_pixel(1, 0, Rgba([0, 0, 255, 255]));
        img
    }

    #[test]
    fn orientation_1_is_identity() {
        let out = apply_orientation(two_px(), 1);
        assert_eq!(out.dimensions(), (2, 1));
        assert_eq!(out.get_pixel(0, 0), &Rgba([255, 0, 0, 255]));
        assert_eq!(out.get_pixel(1, 0), &Rgba([0, 0, 255, 255]));
    }

    #[test]
    fn orientation_2_mirrors_horizontally() {
        let out = apply_orientation(two_px(), 2);
        assert_eq!(out.dimensions(), (2, 1));
        assert_eq!(out.get_pixel(0, 0), &Rgba([0, 0, 255, 255])); // BLUE now left
        assert_eq!(out.get_pixel(1, 0), &Rgba([255, 0, 0, 255]));
    }

    #[test]
    fn orientation_3_rotates_180() {
        let out = apply_orientation(two_px(), 3);
        assert_eq!(out.dimensions(), (2, 1));
        assert_eq!(out.get_pixel(0, 0), &Rgba([0, 0, 255, 255])); // reversed
        assert_eq!(out.get_pixel(1, 0), &Rgba([255, 0, 0, 255]));
    }

    #[test]
    fn rotating_orientations_swap_dimensions() {
        for o in [5u16, 6, 7, 8] {
            let out = apply_orientation(two_px(), o);
            assert_eq!(out.dimensions(), (1, 2), "orientation {o} should swap W/H");
        }
    }

    #[test]
    fn decode_roundtrip_via_temp_png() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.png");
        let mut src = RgbaImage::new(3, 2);
        src.put_pixel(0, 0, Rgba([10, 20, 30, 255]));
        src.save(&path).unwrap();

        let (rgba, w, h) = decode_to_rgba8(&path).unwrap();
        assert_eq!((w, h), (3, 2));
        assert_eq!(rgba.get_pixel(0, 0), &Rgba([10, 20, 30, 255]));
    }

    #[test]
    fn read_orientation_is_none_without_exif() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.png");
        RgbaImage::new(2, 2).save(&path).unwrap();
        assert_eq!(read_orientation(&path), None);
    }

    #[test]
    fn downscale_fits_within_max_and_leaves_small_alone() {
        let big = RgbaImage::new(100, 50);
        let out = downscale_to_fit(big, 40);
        assert!(out.width() <= 40 && out.height() <= 40);
        assert_eq!(out.width(), 40); // limiting dimension hits max

        let small = RgbaImage::new(30, 20);
        let out2 = downscale_to_fit(small, 40);
        assert_eq!(out2.dimensions(), (30, 20)); // unchanged
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p decode`
Expected: FAIL — `apply_orientation`, `decode_to_rgba8`, `read_orientation`, `downscale_to_fit` not defined.

- [ ] **Step 4: Implement the crate**

Prepend to `./crates/decode/src/lib.rs` (above the `#[cfg(test)]` module):
```rust
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use image::imageops::{
    flip_horizontal, flip_vertical, rotate180, rotate270, rotate90, resize, FilterType,
};
use image::{DynamicImage, GenericImageView, ImageReader, RgbaImage};

/// Decode any supported image file to an RGBA8 buffer + its dimensions.
/// Uses magic-byte sniffing (not the extension).
pub fn decode_to_rgba8(path: &Path) -> image::ImageResult<(RgbaImage, u32, u32)> {
    let dynimg: DynamicImage = ImageReader::open(path)?.with_guessed_format()?.decode()?;
    let (w, h) = dynimg.dimensions();
    Ok((dynimg.to_rgba8(), w, h))
}

/// Read the EXIF Orientation tag (1..=8) if present, without decoding pixels.
pub fn read_orientation(path: &Path) -> Option<u16> {
    let mut reader = BufReader::new(File::open(path).ok()?);
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;
    let field = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?;
    match field.value.get_uint(0) {
        Some(v @ 1..=8) => Some(v as u16),
        _ => None,
    }
}

/// Apply an EXIF orientation (1..=8) by transforming the pixel buffer.
/// Handles the mirrored variants, which a single rotation angle cannot.
pub fn apply_orientation(img: RgbaImage, orientation: u16) -> RgbaImage {
    match orientation {
        2 => flip_horizontal(&img),
        3 => rotate180(&img),
        4 => flip_vertical(&img),
        5 => rotate270(&flip_horizontal(&img)), // mirror horizontal + rotate 270 CW
        6 => rotate90(&img),                     // rotate 90 CW
        7 => rotate90(&flip_horizontal(&img)),  // mirror horizontal + rotate 90 CW
        8 => rotate270(&img),                    // rotate 270 CW
        _ => img,                                // 1 or unknown: as-is
    }
}

/// Shrink so both sides are <= `max`, preserving aspect ratio. Images already
/// within bounds are returned unchanged (no upscaling).
pub fn downscale_to_fit(img: RgbaImage, max: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    if w <= max && h <= max {
        return img;
    }
    let scale = (max as f32 / w as f32).min(max as f32 / h as f32);
    let nw = ((w as f32 * scale).round() as u32).max(1);
    let nh = ((h as f32 * scale).round() as u32).max(1);
    resize(&img, nw, nh, FilterType::Triangle)
}
```

> Note (verify during impl): EXIF orientations 5 and 7 (the rare transpose/transverse cases) are composed here from the spec wording; the tests assert the W/H swap. A full pixel-exact fixture test for 5/7 is added in the metadata plan once we can *write* EXIF orientation. Orientations 1–4, 6, 8 are pixel-tested here.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p decode`
Expected: PASS (7 tests).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/decode/
git commit -m "feat(decode): rgba8 decode, EXIF orientation normalize, downscale"
```

---

## Task 4: Display the first image fast

**Files:**
- Modify: `./app/Cargo.toml` (add `decode` dep)
- Modify: `./app/ui/main.slint` (image surface + caption/status)
- Modify: `./app/src/main.rs` (CLI arg + worker decode → UI)

- [ ] **Step 1: Add the `decode` dependency to the app**

Modify `./app/Cargo.toml` `[dependencies]`:
```toml
[dependencies]
slint = { workspace = true }
decode = { path = "../crates/decode" }
```

- [ ] **Step 2: Update the UI with an image surface, caption, and status**

Replace `./app/ui/main.slint` with:
```slint
export component AppWindow inherits Window {
    title: "Photo Viewer";
    background: black;
    preferred-width: 1024px;
    preferred-height: 768px;

    in property <image> current-image;
    in property <string> caption: "";
    in property <string> status-text: "";

    callback quit();
    forward-focus: keys;

    Rectangle {
        clip: true;
        width: 100%;
        height: 100%;

        Image {
            source: root.current-image;
            width: 100%;
            height: 100%;
            image-fit: contain;       // fit-to-window (the default view); zoom comes later
            image-rendering: smooth;
        }
    }

    // Status message (e.g. errors / usage), centered.
    Text {
        text: root.status-text;
        visible: root.status-text != "";
        color: white;
        horizontal-alignment: center;
        vertical-alignment: center;
    }

    // Caption (filename / index), top-left. Plain white text for the foundation;
    // the chrome plan adds a styled, auto-hiding overlay.
    Text {
        x: 8px;
        y: 8px;
        text: root.caption;
        color: white;
        visible: root.caption != "";
    }

    keys := FocusScope {
        key-pressed(event) => {
            if (event.text == Key.Escape || event.text == "q" || event.text == "Q") {
                root.quit();
                return EventResult.accept;
            }
            return EventResult.reject;
        }
    }
}
```

- [ ] **Step 3: Wire the fast-startup decode pipeline in main.rs**

Replace `./app/src/main.rs` with:
```rust
slint::include_modules!();

use slint::ComponentHandle;
use std::path::PathBuf;

fn main() -> Result<(), slint::PlatformError> {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("femtovg".into())
        .select()?;

    let initial: Option<PathBuf> = std::env::args_os().nth(1).map(PathBuf::from);

    let ui = AppWindow::new()?;
    ui.on_quit(|| {
        let _ = slint::quit_event_loop();
    });

    match initial {
        Some(path) => load_image(ui.as_weak(), path),
        None => ui.set_status_text("No image. Usage: photoviewer <path>".into()),
    }

    ui.run()
}

/// Decode `path` on a worker thread and push the oriented, display-sized image
/// (and a caption) back to the UI thread. Errors set the status text instead.
fn load_image(weak: slint::Weak<AppWindow>, path: PathBuf) {
    std::thread::spawn(move || {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match decode::decode_to_rgba8(&path) {
            Ok(rgba) => {
                let orientation = decode::read_orientation(&path).unwrap_or(1);
                let oriented = decode::apply_orientation(rgba, orientation);
                let display = decode::downscale_to_fit(oriented, 4096);
                let (dw, dh) = (display.width(), display.height());
                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    display.as_raw(),
                    dw,
                    dh,
                );
                let _ = weak.upgrade_in_event_loop(move |c| {
                    c.set_current_image(slint::Image::from_rgba8(buffer));
                    c.set_caption(name.into());
                    c.set_status_text("".into());
                });
            }
            Err(e) => {
                let msg = format!("Can't display {name}: {e}");
                let _ = weak.upgrade_in_event_loop(move |c| c.set_status_text(msg.into()));
            }
        }
    });
}
```

- [ ] **Step 4: Build**

Run: `cargo build`
Expected: compiles successfully.

- [ ] **Step 5: Manual run with a real image**

Run: `cargo run -p app -- /path/to/some.jpg` (use any JPEG/PNG/WebP/GIF on disk)
Expected: the window opens and the image appears fit-to-window, correctly oriented, with its filename in the top-left; the window appears effectively immediately even for a large photo (decode happens after the window is already on screen). `Esc`/`Q` quits.

- [ ] **Step 6: Manual run with a bad path**

Run: `cargo run -p app -- /does/not/exist.jpg`
Expected: window opens with a centered "Can't display exist.jpg: ..." message; no crash.

- [ ] **Step 7: Commit**

```bash
git add app/
git commit -m "feat(app): fast-startup decode and display of the first image"
```

---

## Task 5: Directory navigation (prev/next, wrap)

**Files:**
- Modify: `./app/Cargo.toml` (add `imageset` dep)
- Modify: `./app/ui/main.slint` (next/prev callbacks + key bindings)
- Modify: `./app/src/main.rs` (shared nav state + wiring)

- [ ] **Step 1: Add the `imageset` dependency to the app**

Modify `./app/Cargo.toml` `[dependencies]`:
```toml
[dependencies]
slint = { workspace = true }
decode = { path = "../crates/decode" }
imageset = { path = "../crates/imageset" }
```

- [ ] **Step 2: Add navigation callbacks + key bindings to the UI**

In `./app/ui/main.slint`, add the two callbacks near `callback quit();`:
```slint
    callback quit();
    callback next-image();
    callback prev-image();
```

And replace the `key-pressed` body inside `keys := FocusScope { ... }` with:
```slint
        key-pressed(event) => {
            if (event.text == Key.Escape || event.text == "q" || event.text == "Q") {
                root.quit();
                return EventResult.accept;
            }
            if (event.text == Key.RightArrow || event.text == "l" || event.text == "L") {
                root.next-image();
                return EventResult.accept;
            }
            if (event.text == Key.LeftArrow || event.text == "h" || event.text == "H") {
                root.prev-image();
                return EventResult.accept;
            }
            return EventResult.reject;
        }
```

- [ ] **Step 3: Rework main.rs to hold shared navigation state and wire prev/next**

Replace `./app/src/main.rs` with:
```rust
slint::include_modules!();

use slint::ComponentHandle;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn main() -> Result<(), slint::PlatformError> {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("femtovg".into())
        .select()?;

    let initial: Option<PathBuf> = std::env::args_os().nth(1).map(PathBuf::from);

    let ui = AppWindow::new()?;
    ui.on_quit(|| {
        let _ = slint::quit_event_loop();
    });

    // Shared navigation cursor, mutated from UI-thread callbacks and filled by the
    // async directory scan. Arc<Mutex<_>> because the scan runs on a worker thread.
    let nav = Arc::new(Mutex::new(imageset::ImageSet::empty()));

    ui.on_next_image({
        let nav = nav.clone();
        let weak = ui.as_weak();
        move || {
            let next = nav.lock().unwrap().next();
            if let Some(p) = next {
                load_image(weak.clone(), nav.clone(), p);
            }
        }
    });
    ui.on_prev_image({
        let nav = nav.clone();
        let weak = ui.as_weak();
        move || {
            let prev = nav.lock().unwrap().prev();
            if let Some(p) = prev {
                load_image(weak.clone(), nav.clone(), p);
            }
        }
    });

    match initial {
        Some(path) => {
            // 1) Show the requested image immediately.
            load_image(ui.as_weak(), nav.clone(), path.clone());
            // 2) Scan its directory in the background, then refresh the caption with index.
            let nav_scan = nav.clone();
            let weak = ui.as_weak();
            std::thread::spawn(move || {
                let set = imageset::ImageSet::from_file(&path);
                *nav_scan.lock().unwrap() = set;
                let cap = caption(&nav_scan);
                let _ = weak.upgrade_in_event_loop(move |c| c.set_caption(cap.into()));
            });
        }
        None => ui.set_status_text("No image. Usage: photoviewer <path>".into()),
    }

    ui.run()
}

/// "(i/N) name" for the current cursor, or "" if the set is empty.
fn caption(nav: &Arc<Mutex<imageset::ImageSet>>) -> String {
    let g = nav.lock().unwrap();
    match g.current() {
        Some(p) => {
            let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            format!("({}/{}) {}", g.position() + 1, g.len(), name)
        }
        None => String::new(),
    }
}

/// Decode `path` on a worker thread and push the oriented, display-sized image
/// (plus the caption derived from `nav`) back to the UI. Errors set status text.
fn load_image(weak: slint::Weak<AppWindow>, nav: Arc<Mutex<imageset::ImageSet>>, path: PathBuf) {
    std::thread::spawn(move || {
        match decode::decode_to_rgba8(&path) {
            Ok(rgba) => {
                let orientation = decode::read_orientation(&path).unwrap_or(1);
                let oriented = decode::apply_orientation(rgba, orientation);
                let display = decode::downscale_to_fit(oriented, 4096);
                let (dw, dh) = (display.width(), display.height());
                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    display.as_raw(),
                    dw,
                    dh,
                );
                let cap = caption(&nav);
                let _ = weak.upgrade_in_event_loop(move |c| {
                    c.set_current_image(slint::Image::from_rgba8(buffer));
                    c.set_caption(cap.into());
                    c.set_status_text("".into());
                });
            }
            Err(e) => {
                let name = path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let msg = format!("Can't display {name}: {e}");
                let _ = weak.upgrade_in_event_loop(move |c| c.set_status_text(msg.into()));
            }
        }
    });
}
```

- [ ] **Step 4: Build**

Run: `cargo build`
Expected: compiles successfully.

- [ ] **Step 5: Manual navigation run**

Set up a folder with several images named to exercise natural sort (e.g. `img1.jpg`, `img2.jpg`, `img10.jpg`).
Run: `cargo run -p app -- /that/folder/img2.jpg`
Expected: opens on `img2.jpg`; `→`/`L` advances in natural order (`img2` → `img10` → wraps to `img1`); `←`/`H` goes back and wraps; the top-left caption updates to `(i/N) name` each time.

- [ ] **Step 6: Commit**

```bash
git add app/
git commit -m "feat(app): directory navigation with wrap-around"
```

---

## Task 6: Edge cases + README + finalize

**Files:**
- Create: `./README.md`
- Modify: `./crates/imageset/src/lib.rs` (one more edge-case test)

- [ ] **Step 1: Write a failing edge-case test for a single-image directory**

Add to the `tests` module in `./crates/imageset/src/lib.rs`:
```rust
    #[test]
    fn single_image_nav_stays_put() {
        let mut set = ImageSet::new(vec![PathBuf::from("/d/only.jpg")], 0);
        assert_eq!(set.next(), Some(PathBuf::from("/d/only.jpg"))); // wraps to itself
        assert_eq!(set.prev(), Some(PathBuf::from("/d/only.jpg")));
        assert_eq!(set.position(), 0);
    }
```

- [ ] **Step 2: Run it**

Run: `cargo test -p imageset`
Expected: PASS (the existing `% len` wrap logic already handles len==1; this test documents it). If it fails, the wrap arithmetic is wrong — fix `next`/`prev` to use modulo against `files.len()`.

- [ ] **Step 3: Add a README**

`./README.md`:
```markdown
# Photo Viewer Classic

A fast, simple, cross-platform desktop photo viewer (Rust + Slint).

## Run

```bash
cargo run -p app -- path/to/image.jpg
```

- Shows the image fit-to-window, correctly oriented, almost immediately.
- `→` / `L` next image, `←` / `H` previous (natural-sorted directory, wraps).
- `Esc` / `Q` quit.

Phase-1 foundation: display + navigation. Interactive zoom/pan/rotate, the
auto-hiding toolbar, tag/rating editing, and HEIC/AVIF come in later plans.
See `docs/superpowers/specs/` and `docs/superpowers/plans/`.
```

- [ ] **Step 4: Build + full test sweep**

Run: `cargo build && cargo test`
Expected: workspace builds; all `imageset` + `decode` tests pass.

- [ ] **Step 5: Commit**

```bash
git add README.md crates/imageset/
git commit -m "test(imageset): single-image wrap; docs: add README"
```

---

## Done criteria

- `cargo build` and `cargo test` pass from the workspace root.
- `photoviewer <image>` opens a window and shows the image fit-to-window, correctly
  oriented, with the window appearing before decode completes (fast startup).
- Arrow keys / `H` / `L` navigate the directory in natural-sorted, wrap-around order with
  a live caption; `Esc`/`Q` quit; bad paths show a message instead of crashing.

**Next plan (Plan 2 — Interactive view & chrome):** wire the `viewstate` crate for
zoom (`↑`/`K`, `↓`/`J`, scrollwheel-toward-cursor), pan (`Shift`+dirs, left-drag),
rotate (`E`/`R`, via pixel-buffer transform), the `Z` view-mode cycle (Fit / 1:1 /
last), `F` fullscreen, neighbor prefetch, the auto-hiding bottom toolbar + edge buttons,
the `I` info overlay, and `config.toml` geometry/fullscreen persistence.
```
