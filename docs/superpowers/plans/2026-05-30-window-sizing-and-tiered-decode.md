# Window Sizing & Tiered Decode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Open/refit the window to 80%-of-monitor at the image's aspect, cache current+prev1+next1 at full GPU resolution (true 1:1) with next2 as a 4096 preview, and shave cold-start time-to-first-display — all on the existing `image`/zune-jpeg + `fast_image_resize` stack (no `jpeg-decoder`, no DCT).

**Architecture:** A hardcoded `PrefetchPlan` drives a quality-aware cache (`Cached { buffer, cap }`). The single foreground worker shows the current image at `full_cap = min(16384, GL_MAX_TEXTURE_SIZE)` and upgrades a 4096-preview neighbour to full when navigated to; the single prefetch worker fills neighbours at their tier's cap. Window sizing happens on the **first `image-presented(is_new=true)`** (the winit window doesn't exist before `run()`), reached via `i-slint-backend-winit`'s `WinitWindowAccessor`. The GPU texture limit is read once via Slint's rendering notifier + `glow`.

**Tech Stack:** Rust 2021, Slint 1.16.1 (winit + femtovg), `image` 0.25.10 (zune-jpeg), `fast_image_resize` 6, new deps `i-slint-backend-winit =1.16.1` and `glow =0.17`.

**Spec:** `docs/superpowers/specs/2026-05-30-window-sizing-and-tiered-decode-design.md` (rev. 2).

**Conventions for every task:** run `cargo test -p <crate>` for the crate you changed (or `cargo test` at the root), and `cargo build -p app` after app changes. Commit only after the relevant tests pass. The branch is `feat-window-sizing-tiered-decode`.

---

## Task 1: `display_dimensions` (decode crate)

**Files:**
- Modify: `crates/decode/src/lib.rs`
- Test: `crates/decode/src/lib.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to `mod tests`:

```rust
#[test]
fn display_dimensions_reads_header_without_exif_swap_for_orientation_1() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.png");
    image::RgbaImage::new(40, 20).save(&path).unwrap(); // no EXIF → orientation 1
    assert_eq!(display_dimensions(&path), Some((40, 20)));
}

#[test]
fn display_dimensions_is_none_for_missing_file() {
    assert_eq!(display_dimensions(std::path::Path::new("/no/such/file.jpg")), None);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p decode display_dimensions`
Expected: FAIL — `cannot find function display_dimensions`.

- [ ] **Step 3: Implement `display_dimensions`**

Add (near `read_orientation`) in `crates/decode/src/lib.rs`:

```rust
/// Header-only natural display dimensions (post-EXIF-orientation), without decoding
/// pixels. Used to size the window before the first decode. Returns None on error.
/// NOTE: `image::image_dimensions` guesses format by file EXTENSION (unlike the
/// magic-byte decode path), so a missing/wrong extension yields None — the caller
/// falls back to the default window size.
pub fn display_dimensions(path: &Path) -> Option<(u32, u32)> {
    let (w, h) = image::image_dimensions(path).ok()?;
    match read_orientation(path) {
        Some(5 | 6 | 7 | 8) => Some((h, w)), // 90°/270° transpose → swap W/H
        _ => Some((w, h)),
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p decode display_dimensions`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add crates/decode/src/lib.rs
git commit -m "feat(decode): header-only display_dimensions with EXIF swap"
```

---

## Task 2: Defer RGBA expansion in `display_image` (decode crate)

Decode in the native channel layout (RGB for JPEG), orient + downscale there, expand to RGBA only after downscaling — avoiding a full native-resolution RGBA allocation when downscaling. Output stays `RgbaImage`; pixels match the old path.

**Files:**
- Modify: `crates/decode/src/lib.rs`
- Test: `crates/decode/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add to `mod tests`:

```rust
#[test]
fn display_image_rgb_path_matches_naive_and_fits_cap() {
    use image::{Rgb, RgbImage};
    // An RGB (no-alpha) PNG decodes to DynamicImage::ImageRgb8 → exercises the deferred path.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rgb.png");
    let mut src = RgbImage::new(100, 50);
    for (x, _y, px) in src.enumerate_pixels_mut() {
        *px = Rgb([(x % 256) as u8, 20, 200]); // some horizontal gradient
    }
    src.save(&path).unwrap();

    // Reference: the old naive path (decode → rgba8 → downscale).
    let naive = {
        let dynimg = image::ImageReader::open(&path).unwrap().decode().unwrap();
        downscale_to_fit(dynimg.to_rgba8(), 40)
    };
    let got = display_image(&path, 40).unwrap();

    assert!(got.width() <= 40 && got.height() <= 40);
    assert_eq!(got.dimensions(), naive.dimensions());
    // Per-channel equality within rounding tolerance (Lanczos on RGB vs RGBA).
    for (a, b) in got.pixels().zip(naive.pixels()) {
        for c in 0..4 {
            assert!((a.0[c] as i16 - b.0[c] as i16).abs() <= 1, "channel {c} differs");
        }
    }
    // Opaque alpha preserved.
    assert_eq!(got.get_pixel(0, 0).0[3], 255);
}
```

- [ ] **Step 2: Run to verify it fails or is incidental-pass**

Run: `cargo test -p decode display_image_rgb_path_matches_naive`
Expected: PASS today (the naive `display_image` already produces this) — this test is a **regression guard** for the refactor. Confirm it passes before refactoring, then keep it green after.

- [ ] **Step 3: Refactor `display_image` + add the generic/RGB helpers**

Make `apply_orientation` generic and add `fit_dims` / an RGB resize + downscale, then branch `display_image`. Replace the existing `apply_orientation`, `downscale_to_fit`, `resize_rgba`, and `display_image` with:

```rust
use image::{DynamicImage, ImageReader, Rgb, RgbImage, RgbaImage};

/// Read once, decode, normalize EXIF orientation, downscale so both sides <= `max`.
/// Defers RGBA expansion past the downscale for the no-alpha (RGB/JPEG) case so no
/// full native-resolution RGBA buffer is allocated when downscaling.
pub fn display_image(path: &Path, max: u32) -> image::ImageResult<RgbaImage> {
    let bytes = std::fs::read(path)?;
    let orientation = orientation_from_bytes(&bytes).unwrap_or(1);
    let dynimg = ImageReader::new(Cursor::new(bytes.as_slice()))
        .with_guessed_format()?
        .decode()?;
    Ok(match dynimg {
        DynamicImage::ImageRgb8(rgb) => {
            let small = downscale_rgb_to_fit(apply_orientation(rgb, orientation), max);
            DynamicImage::ImageRgb8(small).into_rgba8()
        }
        other => downscale_to_fit(apply_orientation(other.into_rgba8(), orientation), max),
    })
}

/// Apply an EXIF orientation (1..=8) by transforming the pixel buffer. Generic over the
/// pixel type so it serves both the RGB (deferred) and RGBA paths. Mirrored variants
/// (2,4,5,7) compose a flip with a rotation.
pub fn apply_orientation<P>(
    img: image::ImageBuffer<P, Vec<P::Subpixel>>,
    orientation: u16,
) -> image::ImageBuffer<P, Vec<P::Subpixel>>
where
    P: image::Pixel<Subpixel = u8> + 'static,
{
    match orientation {
        2 => flip_horizontal(&img),
        3 => rotate180(&img),
        4 => flip_vertical(&img),
        5 => rotate270(&flip_horizontal(&img)),
        6 => rotate90(&img),
        7 => rotate90(&flip_horizontal(&img)),
        8 => rotate270(&img),
        _ => img,
    }
}

/// Target dims that fit `(w, h)` within `max` preserving aspect, or None if already within.
fn fit_dims(w: u32, h: u32, max: u32) -> Option<(u32, u32)> {
    if w <= max && h <= max {
        return None;
    }
    let scale = (max as f32 / w as f32).min(max as f32 / h as f32);
    Some((
        ((w as f32 * scale).round() as u32).max(1),
        ((h as f32 * scale).round() as u32).max(1),
    ))
}

/// Shrink an RGBA image so both sides are <= `max` (no upscaling). SIMD Lanczos3.
pub fn downscale_to_fit(img: RgbaImage, max: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    match fit_dims(w, h, max) {
        None => img,
        Some((nw, nh)) => resize_u8x4(img, nw, nh),
    }
}

/// Shrink an RGB image so both sides are <= `max` (no upscaling). SIMD Lanczos3.
fn downscale_rgb_to_fit(img: RgbImage, max: u32) -> RgbImage {
    let (w, h) = img.dimensions();
    match fit_dims(w, h, max) {
        None => img,
        Some((nw, nh)) => resize_u8x3(img, nw, nh),
    }
}

fn resize_u8x4(img: RgbaImage, nw: u32, nh: u32) -> RgbaImage {
    use fast_image_resize::images::Image as FirImage;
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
    let (w, h) = img.dimensions();
    let src = FirImage::from_vec_u8(w, h, img.into_raw(), PixelType::U8x4)
        .expect("RgbaImage backing buffer is exactly w*h*4 bytes");
    let mut dst = FirImage::new(nw, nh, PixelType::U8x4);
    Resizer::new()
        .resize(&src, &mut dst,
            &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3)))
        .expect("U8x4 -> U8x4 convolution with nonzero dimensions");
    RgbaImage::from_raw(nw, nh, dst.into_vec()).expect("resized buffer is exactly nw*nh*4 bytes")
}

fn resize_u8x3(img: RgbImage, nw: u32, nh: u32) -> RgbImage {
    use fast_image_resize::images::Image as FirImage;
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
    let (w, h) = img.dimensions();
    let src = FirImage::from_vec_u8(w, h, img.into_raw(), PixelType::U8x3)
        .expect("RgbImage backing buffer is exactly w*h*3 bytes");
    let mut dst = FirImage::new(nw, nh, PixelType::U8x3);
    Resizer::new()
        .resize(&src, &mut dst,
            &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3)))
        .expect("U8x3 -> U8x3 convolution with nonzero dimensions");
    RgbImage::from_raw(nw, nh, dst.into_vec()).expect("resized buffer is exactly nw*nh*3 bytes")
}
```

Update the top `use` line to include `flip_horizontal, flip_vertical, rotate180, rotate270, rotate90` (already imported) and remove the now-unused `image::{ImageReader, RgbaImage}` duplicate if the compiler warns (the new `use image::{...}` above supersedes it — keep a single import set). Keep `use std::io::Cursor;` and `use std::path::Path;`.

- [ ] **Step 4: Run all decode tests**

Run: `cargo test -p decode`
Expected: PASS — the new RGB-path test, the `apply_orientation` tests (they call `apply_orientation(two_px(), n)` where `two_px()` is `RgbaImage`; generic inference picks `P = Rgba<u8>`), `downscale_*`, and `display_image_decodes_and_downscales` (an RGBA PNG → `other` branch). Fix any unused-import warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/decode/src/lib.rs
git commit -m "perf(decode): defer RGBA expansion past downscale (RGB path)"
```

---

## Task 3: `PrefetchPlan` + `targets()` (imageset crate)

**Files:**
- Modify: `crates/imageset/src/lib.rs`
- Test: `crates/imageset/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add to `mod tests`:

```rust
#[test]
fn default_plan_targets_are_inner_full_next2_preview() {
    let t = DEFAULT_PLAN.targets();
    assert_eq!(
        t,
        vec![(0, 16384), (1, 16384), (-1, 16384), (2, 4096)],
        "current/next1/prev1 full; next2 preview; forward-priority order"
    );
    // No duplicate offsets.
    let mut offs: Vec<isize> = t.iter().map(|(o, _)| *o).collect();
    offs.sort();
    let len = offs.len();
    offs.dedup();
    assert_eq!(offs.len(), len, "offsets must be unique");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p imageset default_plan_targets`
Expected: FAIL — `cannot find value DEFAULT_PLAN`.

- [ ] **Step 3: Implement `PrefetchPlan`**

Add near the top of `crates/imageset/src/lib.rs` (after the `use` lines):

```rust
/// How many neighbours to keep cached and at what quality. Hardcoded for now; every
/// field is a future CLI/config knob, so the cache-window size is trivially configurable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrefetchPlan {
    pub full_cap: u32,         // 16384 ceiling, clamped to GL_MAX_TEXTURE_SIZE at runtime
    pub preview_cap: u32,      // 4096
    pub behind_full: usize,    // 1 → prev1 at full
    pub ahead_full: usize,     // 1 → next1 at full
    pub behind_preview: usize, // 0
    pub ahead_preview: usize,  // 1 → next2 at preview
}

pub const DEFAULT_PLAN: PrefetchPlan = PrefetchPlan {
    full_cap: 16384,
    preview_cap: 4096,
    behind_full: 1,
    ahead_full: 1,
    behind_preview: 0,
    ahead_preview: 1,
};

impl PrefetchPlan {
    /// (offset, cap) pairs for the keep-set, forward-priority. The preview band sits
    /// strictly beyond the full band, so offsets never overlap or duplicate.
    pub fn targets(&self) -> Vec<(isize, u32)> {
        let mut v = vec![(0isize, self.full_cap)];
        for i in 1..=self.ahead_full as isize {
            v.push((i, self.full_cap));
        }
        for i in 1..=self.behind_full as isize {
            v.push((-i, self.full_cap));
        }
        for i in 1..=self.ahead_preview as isize {
            v.push((self.ahead_full as isize + i, self.preview_cap));
        }
        for i in 1..=self.behind_preview as isize {
            v.push((-(self.behind_full as isize + i), self.preview_cap));
        }
        v
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p imageset default_plan_targets`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/imageset/src/lib.rs
git commit -m "feat(imageset): PrefetchPlan + targets (hardcoded default)"
```

---

## Task 4: `ImageSet::keep_set` (imageset crate)

Map `targets()` through `peek` into `(PathBuf, cap)`, deduping wrapped offsets to the **highest** cap (full beats preview).

**Files:**
- Modify: `crates/imageset/src/lib.rs`
- Test: `crates/imageset/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn keep_set_maps_targets_through_peek() {
    let files: Vec<PathBuf> = (0..5).map(|i| PathBuf::from(format!("/d/{i}.jpg"))).collect();
    let set = ImageSet::new(files, 2);
    assert_eq!(
        set.keep_set(&DEFAULT_PLAN),
        vec![
            (PathBuf::from("/d/2.jpg"), 16384),
            (PathBuf::from("/d/3.jpg"), 16384),
            (PathBuf::from("/d/1.jpg"), 16384),
            (PathBuf::from("/d/4.jpg"), 4096),
        ]
    );
}

#[test]
fn keep_set_dedupes_wrapped_offsets_keeping_full() {
    // 2 files: offsets {0,+1,-1,+2} wrap → each path appears twice; full must win.
    let set = ImageSet::new(vec![PathBuf::from("/d/a.jpg"), PathBuf::from("/d/b.jpg")], 0);
    let ks = set.keep_set(&DEFAULT_PLAN);
    assert_eq!(ks.len(), 2);
    assert!(ks.contains(&(PathBuf::from("/d/a.jpg"), 16384))); // off 0 (full) beats off +2 (preview)
    assert!(ks.contains(&(PathBuf::from("/d/b.jpg"), 16384)));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p imageset keep_set`
Expected: FAIL — `no method named keep_set`.

- [ ] **Step 3: Implement `keep_set`**

Add inside `impl ImageSet`:

```rust
/// The prefetch keep-set: `(path, cap)` for each plan target, via `peek` (no cursor
/// move). Wrapped offsets that resolve to the same path are deduped to the highest cap.
pub fn keep_set(&self, plan: &PrefetchPlan) -> Vec<(PathBuf, u32)> {
    let mut out: Vec<(PathBuf, u32)> = Vec::new();
    for (off, cap) in plan.targets() {
        let Some(p) = self.peek(off) else { continue };
        match out.iter_mut().find(|(ep, _)| *ep == p) {
            Some(existing) if cap > existing.1 => existing.1 = cap,
            Some(_) => {}
            None => out.push((p, cap)),
        }
    }
    out
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p imageset keep_set`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/imageset/src/lib.rs
git commit -m "feat(imageset): ImageSet::keep_set (dedup, full wins)"
```

---

## Task 5: Quality-aware cache + tiered prefetch (app)

Replace the bare `Arc<RgbaImage>` cache with `Cached { buffer, cap }`, rework `obtain_base` to take a target cap, and make both workers + `send_prefetch` cap-aware. The foreground worker now decodes the current at `plan.full_cap` (preview→full upgrade comes in Task 10).

**Files:**
- Modify: `app/src/main.rs`
- Test: `app/src/main.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Update the `obtain_base` test (write the new expectation)**

Replace `obtain_base_uses_cache_on_hit_and_decodes_on_miss` with:

```rust
#[test]
fn obtain_base_respects_cap_on_hit_and_decodes_on_miss() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let cache: Cache = Arc::new(Mutex::new(HashMap::new()));
    let p = PathBuf::from("/d/x.jpg");

    // Seeded at preview cap 4096.
    let seeded = Arc::new(image::RgbaImage::new(1, 1));
    cache.lock().unwrap().insert(p.clone(), Cached { buffer: seeded.clone(), cap: 4096 });

    let calls = AtomicUsize::new(0);
    let decode = |_p: &Path| -> Result<image::RgbaImage, std::io::Error> {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(image::RgbaImage::new(2, 2))
    };

    // Requesting at <= cached cap → HIT, no decode, same Arc.
    let got = obtain_base(&cache, &p, 4096, decode).unwrap();
    assert!(Arc::ptr_eq(&got, &seeded));
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // Requesting at a HIGHER cap → MISS, decode once, cache upgraded to the new cap.
    let got = obtain_base(&cache, &p, 16384, decode).unwrap();
    assert_eq!(got.dimensions(), (2, 2));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(cache.lock().unwrap().get(&p).unwrap().cap, 16384);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p app obtain_base`
Expected: FAIL to compile — `Cached` undefined, `obtain_base` arity wrong.

- [ ] **Step 3: Introduce `Cached`, rework `obtain_base`, both workers, `send_prefetch`**

In `app/src/main.rs`:

(a) Replace the `Cache` type alias and add `Cached`:

```rust
/// A cached decode plus the cap it was decoded to (so a full entry satisfies a preview
/// need but never the reverse).
struct Cached {
    buffer: Arc<image::RgbaImage>,
    cap: u32,
}
type Cache = Arc<Mutex<HashMap<PathBuf, Cached>>>;
```

(b) Delete the `const MAX_DISPLAY_DIM: u32 = 4096;` line (replaced by plan caps).

(c) Replace `obtain_base`:

```rust
/// Obtain the rotation-0 BASE buffer for `path` at >= `target_cap`: return the cached
/// buffer on a sufficient hit, else decode at `target_cap`, cache it (tagged with the
/// cap), and return it.
fn obtain_base<E>(
    cache: &Cache,
    path: &Path,
    target_cap: u32,
    decode: impl FnOnce(&Path) -> Result<image::RgbaImage, E>,
) -> Result<Arc<image::RgbaImage>, E> {
    if let Some(buf) = cache
        .lock()
        .unwrap()
        .get(path)
        .filter(|c| c.cap >= target_cap)
        .map(|c| c.buffer.clone())
    {
        return Ok(buf);
    }
    let buffer = Arc::new(decode(path)?);
    cache.lock().unwrap().insert(
        path.to_path_buf(),
        Cached { buffer: buffer.clone(), cap: target_cap },
    );
    Ok(buffer)
}
```

(d) In `spawn_decode_worker`, change the `obtain_base` call (and pass the plan in — see signature change). Update the signature to `fn spawn_decode_worker(weak: slint::Weak<AppWindow>, cache: Cache, plan: imageset::PrefetchPlan) -> mpsc::Sender<Job>` and the Show branch body:

```rust
match obtain_base(&cache, &path, plan.full_cap, |p| decode::display_image(p, plan.full_cap)) {
    Ok(base) => {
        current = Some((path, base));
        turns = 0;
        push_frame(&weak, &current, turns, caption, true);
    }
    Err(e) => {
        let msg = format!("Can't display {}: {e}", file_name_of(&path));
        let _ = weak.upgrade_in_event_loop(move |c| c.set_status_text(msg.into()));
        continue;
    }
}
```

(e) Replace `spawn_prefetch_worker` and its channel type:

```rust
fn spawn_prefetch_worker(cache: Cache) -> mpsc::Sender<Vec<(PathBuf, u32)>> {
    let (tx, rx) = mpsc::channel::<Vec<(PathBuf, u32)>>();
    std::thread::spawn(move || {
        while let Ok(first) = rx.recv() {
            let keep = coalesce_latest(first, &rx);
            for (path, cap) in &keep {
                let have = cache.lock().unwrap().get(path).map_or(false, |c| c.cap >= *cap);
                if have {
                    continue;
                }
                if let Ok(rgba) = decode::display_image(path, *cap) {
                    cache.lock().unwrap().insert(
                        path.clone(),
                        Cached { buffer: Arc::new(rgba), cap: *cap },
                    );
                }
            }
            let keep_paths: std::collections::HashSet<&PathBuf> = keep.iter().map(|(p, _)| p).collect();
            cache.lock().unwrap().retain(|k, _| keep_paths.contains(k));
        }
    });
    tx
}
```

(f) Replace `send_prefetch` to use the plan + `keep_set`:

```rust
fn send_prefetch(
    nav: &Arc<Mutex<imageset::ImageSet>>,
    plan: &imageset::PrefetchPlan,
    pf: &mpsc::Sender<Vec<(PathBuf, u32)>>,
) {
    let keep = { nav.lock().unwrap().keep_set(plan) };
    if !keep.is_empty() {
        let _ = pf.send(keep);
    }
}
```

(g) In `main`, thread the plan through. Add near the cache creation:

```rust
let plan = imageset::DEFAULT_PLAN;
```

Update the worker spawn to `spawn_decode_worker(ui.as_weak(), cache.clone(), plan)`. Update all three `send_prefetch(&nav, &prefetch_tx)` call sites (in `on_next_image`, `on_prev_image`, and the directory-scan thread) to `send_prefetch(&nav, &plan, &pf)` — capture `plan` (it is `Copy`) into each closure alongside `nav`/`pf`.

- [ ] **Step 4: Run app tests + build**

Run: `cargo test -p app obtain_base && cargo build -p app`
Expected: PASS / clean build. (The `obtain_base` test passes; the GUI tests are unaffected.)

- [ ] **Step 5: Commit**

```bash
git add app/src/main.rs
git commit -m "feat(app): quality-aware cache + tiered prefetch (no upgrade yet)"
```

---

## Task 6: `fit_80_dims` pure geometry (window module)

**Files:**
- Create: `app/src/window.rs`
- Modify: `app/src/main.rs` (add `mod window;`)
- Test: `app/src/window.rs`

- [ ] **Step 1: Write the failing tests**

Create `app/src/window.rs`:

```rust
//! Window-control helpers (sizing, monitor geometry). Kept out of `main.rs`.

/// Largest (w, h) of ratio `aspect` (w/h) fitting in 0.8 × monitor, centered on that
/// monitor (top-left = mon_pos + offset). Pure; positions may be negative on
/// multi-monitor setups. Returns (w, h, x, y) in physical pixels.
pub fn fit_80_dims(aspect: f32, mon_w: u32, mon_h: u32, mon_x: i32, mon_y: i32) -> (u32, u32, i32, i32) {
    let box_w = mon_w as f32 * 0.8;
    let box_h = mon_h as f32 * 0.8;
    let (w, h) = if box_w / box_h > aspect {
        (box_h * aspect, box_h) // box is wider than the image → height-limited
    } else {
        (box_w, box_w / aspect) // → width-limited
    };
    let w = (w.round() as u32).max(1);
    let h = (h.round() as u32).max(1);
    let x = mon_x + (mon_w as i32 - w as i32) / 2;
    let y = mon_y + (mon_h as i32 - h as i32) / 2;
    (w, h, x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landscape_image_in_landscape_monitor_is_width_limited() {
        // 1000x1000 monitor, 0.8 box = 800x800. aspect 2.0 (wide) → width-limited: 800x400.
        let (w, h, x, y) = fit_80_dims(2.0, 1000, 1000, 0, 0);
        assert_eq!((w, h), (800, 400));
        assert_eq!((x, y), ((1000 - 800) / 2, (1000 - 400) / 2)); // (100, 300)
    }

    #[test]
    fn portrait_image_is_height_limited() {
        // box 800x800, aspect 0.5 (tall) → height-limited: 400x800.
        let (w, h, _, _) = fit_80_dims(0.5, 1000, 1000, 0, 0);
        assert_eq!((w, h), (400, 800));
    }

    #[test]
    fn centering_adds_monitor_offset_and_stays_signed() {
        // Monitor to the left of primary (negative x).
        let (w, _h, x, _y) = fit_80_dims(1.0, 1000, 1000, -1920, 0);
        assert_eq!(w, 800);
        assert_eq!(x, -1920 + (1000 - 800) / 2); // -1820
    }
}
```

- [ ] **Step 2: Wire the module + run**

In `app/src/main.rs`, add after `slint::include_modules!();`:

```rust
mod window;
```

Run: `cargo test -p app fit_80`
Expected: PASS (all three).

- [ ] **Step 3–4: (implementation is already in Step 1; tests pass)**

- [ ] **Step 5: Commit**

```bash
git add app/src/window.rs app/src/main.rs
git commit -m "feat(app): fit_80_dims pure window-fit geometry"
```

---

## Task 7: Monitor access + `fit_window_to_aspect` (window module)

**Files:**
- Modify: `app/Cargo.toml`, `app/src/window.rs`

- [ ] **Step 1: Add the dependency**

In `app/Cargo.toml` under `[dependencies]`:

```toml
# Reach the winit window for monitor size (current_monitor) — only transitive via slint
# today. Pinned lockstep with slint to keep a single winit 0.30.x in the tree.
i-slint-backend-winit = { version = "=1.16.1", default-features = false }
```

Run: `cargo build -p app`
Expected: builds (dep resolves to the already-locked 1.16.1).

- [ ] **Step 2: Add the accessor + applier functions**

Append to `app/src/window.rs`:

```rust
use crate::AppWindow;
use i_slint_backend_winit::winit::dpi::{PhysicalPosition, PhysicalSize};
use i_slint_backend_winit::WinitWindowAccessor;
use slint::ComponentHandle;

/// Monitor (size, top-left position) of the window's current monitor — None until the
/// winit window is live (i.e. after `run()` starts) or on a non-winit backend.
fn monitor_size(ui: &AppWindow) -> Option<(PhysicalSize<u32>, PhysicalPosition<i32>)> {
    ui.window()
        .with_winit_window(|w| w.current_monitor().map(|m| (m.size(), m.position())))
        .flatten()
}

/// Whether the window is maximized (read from Slint directly — no winit round-trip).
fn is_maximized(ui: &AppWindow) -> bool {
    ui.window().is_maximized()
}

/// If windowed (not fullscreen, not maximized) and the monitor is known, size+center the
/// window to 80%/aspect. No-op otherwise (so it's safe to call on every new image and
/// under the headless testing backend, where the winit window is absent).
pub fn fit_window_to_aspect(ui: &AppWindow, aspect_w: u32, aspect_h: u32, fullscreen: bool) {
    if fullscreen || aspect_h == 0 || is_maximized(ui) {
        return;
    }
    let Some((mon, pos)) = monitor_size(ui) else { return };
    let aspect = aspect_w as f32 / aspect_h as f32;
    let (w, h, x, y) = fit_80_dims(aspect, mon.width, mon.height, pos.x, pos.y);
    ui.window().set_size(slint::PhysicalSize::new(w, h));
    ui.window().set_position(slint::PhysicalPosition::new(x, y));
}
```

- [ ] **Step 3: Build (no unit test — needs a live winit window; covered by manual run in Task 11)**

Run: `cargo build -p app`
Expected: clean build. (`monitor_size`/`is_maximized` are `dead_code` until Task 9 wires `fit_window_to_aspect`; that's fine — `fit_window_to_aspect` is `pub` so no warning, and it uses both.)

- [ ] **Step 4: Commit**

```bash
git add app/Cargo.toml app/src/window.rs Cargo.lock
git commit -m "feat(app): winit monitor access + fit_window_to_aspect"
```

---

## Task 8: CLI argument parsing (app)

**Files:**
- Modify: `app/src/main.rs`
- Test: `app/src/main.rs`

- [ ] **Step 1: Write the failing tests**

Add to `mod tests`:

```rust
#[test]
fn parse_args_takes_first_non_flag_as_path_and_recognizes_restore() {
    let a = parse_args(["photoviewer".into(), "--restore-geometry".into(), "/p/x.jpg".into()].into_iter());
    assert_eq!(a.path, Some(PathBuf::from("/p/x.jpg")));
    assert!(a.restore_geometry);
}

#[test]
fn parse_args_defaults_and_ignores_unknown_flags() {
    let a = parse_args(["photoviewer".into(), "--bogus".into()].into_iter());
    assert_eq!(a.path, None);
    assert!(!a.restore_geometry);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p app parse_args`
Expected: FAIL — `cannot find function parse_args`.

- [ ] **Step 3: Implement `Args` + `parse_args`**

Add to `app/src/main.rs`:

```rust
/// Parsed CLI: the first non-`--` argument is the image path; `--restore-geometry` is the
/// only recognized flag. Unknown `--flags` are ignored with a one-line stderr note.
/// Structured so future flags (e.g. cache-plan overrides) are mechanical to add.
struct Args {
    path: Option<PathBuf>,
    restore_geometry: bool,
}

fn parse_args(args: impl Iterator<Item = String>) -> Args {
    let mut path = None;
    let mut restore_geometry = false;
    for arg in args.skip(1) {
        match arg.as_str() {
            "--restore-geometry" => restore_geometry = true,
            s if s.starts_with("--") => eprintln!("photoviewer: ignoring unknown flag {s}"),
            _ if path.is_none() => path = Some(PathBuf::from(arg)),
            _ => {}
        }
    }
    Args { path, restore_geometry }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p app parse_args`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add app/src/main.rs
git commit -m "feat(app): hand-rolled CLI parse (Args, --restore-geometry)"
```

---

## Task 9: Window sizing wiring — startup priority + fit-on-new (app)

Replace the unconditional geometry restore with `fullscreen > --restore-geometry > aspect-presize`, and fit to 80%/monitor on every `is_new` image-presented (the first such event is the startup fit).

**Files:**
- Modify: `app/src/main.rs`
- Test: `app/src/main.rs` (`gui_tests`)

- [ ] **Step 1: Write the failing headless test**

Add to `mod gui_tests`:

```rust
#[test]
fn on_new_image_hook_fires_on_new_but_not_on_rotate() {
    use std::rc::Rc;
    init_backend();
    let ui = AppWindow::new().expect("AppWindow under testing backend");
    let vs = Rc::new(RefCell::new(viewstate::ViewState::new()));
    let count = Rc::new(Cell::new(0u32));
    attach_view_handlers(&ui, &vs, {
        let count = count.clone();
        move |_ui, _w, _h| count.set(count.get() + 1)
    });

    ui.invoke_viewport_changed(800.0, 600.0);
    ui.invoke_image_presented(400, 200, true); // new → hook fires
    ui.invoke_image_presented(200, 400, false); // rotate → no hook
    assert_eq!(count.get(), 1, "hook fires only on is_new");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p app on_new_image_hook`
Expected: FAIL to compile — `attach_view_handlers` takes 2 args.

- [ ] **Step 3: Add the `on_new_image` hook to `attach_view_handlers`**

Change the signature and the `on_image_presented` handler in `attach_view_handlers`:

```rust
fn attach_view_handlers(
    ui: &AppWindow,
    vs: &Rc<RefCell<viewstate::ViewState>>,
    on_new_image: impl Fn(&AppWindow, u32, u32) + 'static,
) {
    // ... unchanged on_viewport_changed / on_zoom_by / on_pan_by / on_cycle_view ...

    ui.on_image_presented({
        let vs = vs.clone();
        let weak = ui.as_weak();
        move |nat_w, nat_h, is_new| {
            let ui = weak.unwrap();
            {
                let mut v = vs.borrow_mut();
                if is_new {
                    v.load(nat_w as f32, nat_h as f32);
                } else {
                    v.set_natural(nat_w as f32, nat_h as f32);
                }
            }
            apply_geometry(&ui, &vs.borrow());
            if is_new {
                on_new_image(&ui, nat_w as u32, nat_h as u32);
            }
        }
    });
}
```

Update the two **other** `gui_tests` that call `attach_view_handlers(&ui, &vs)` (`callback_zoom_doubles_scale_and_updates_properties`, `rotation_keeps_zoom_then_navigation_resets_it`) to pass a no-op hook: `attach_view_handlers(&ui, &vs, |_, _, _| {});`.

- [ ] **Step 4: Wire the real hook + startup priority in `main`**

In `main`, replace the existing restore block (the `let cfg = config::load(); if let Some(g) = cfg.geometry { ... } if cfg.fullscreen { ... }`) with:

```rust
let cfg = config::load();
// auto_fit is active only when we are NOT restoring geometry and NOT starting fullscreen;
// it stays on across runtime fullscreen toggles (fit_window_to_aspect no-ops while fullscreen).
let restoring = args.restore_geometry && cfg.geometry.is_some();
let auto_fit = Rc::new(Cell::new(!cfg.fullscreen && !restoring));

if cfg.fullscreen {
    fullscreen.set(true);
    ui.window().set_fullscreen(true);
} else if restoring {
    let g = cfg.geometry.as_ref().unwrap();
    ui.window().set_position(slint::PhysicalPosition::new(g.x, g.y));
    ui.window().set_size(slint::PhysicalSize::new(g.w, g.h));
} else if let Some(path) = args.path.as_ref() {
    // Aspect pre-size: open at the image's shape so the first-frame 80% fit only rescales.
    if let Some((w, h)) = decode::display_dimensions(path) {
        if h > 0 {
            let ref_h: u32 = 768; // default preferred height (matches main.slint)
            let pre_w = ((ref_h as f32) * (w as f32 / h as f32)).round().max(1.0) as u32;
            ui.window().set_size(slint::PhysicalSize::new(pre_w, ref_h));
        }
    }
}
```

Then change the `attach_view_handlers(&ui, &vs);` call to pass the fit hook:

```rust
attach_view_handlers(&ui, &vs, {
    let auto_fit = auto_fit.clone();
    let fs = fullscreen.clone();
    move |ui, w, h| {
        if auto_fit.get() {
            window::fit_window_to_aspect(ui, w, h, fs.get());
        }
    }
});
```

Replace the `initial`/`std::env::args_os()` usage: at the top of `main`, parse args once — `let args = parse_args(std::env::args());` — and use `args.path` everywhere the old `initial` was used (the `match initial { Some(path) => ... }` becomes `match args.path.clone() { Some(path) => ... }`). Remove the old `let initial = std::env::args_os().nth(1)...` line.

- [ ] **Step 5: Run tests + build, then commit**

Run: `cargo test -p app && cargo build -p app`
Expected: PASS (new hook test + all existing GUI tests with the no-op hook).

```bash
git add app/src/main.rs
git commit -m "feat(app): startup sizing priority + fit-to-80%/aspect on new image"
```

---

## Task 10: Foreground preview→full upgrade (app)

The worker shows a cached/decoded buffer immediately, and when it only had a 4096 preview, upgrades to `full_cap` once the queue is idle (skip-on-newer), re-applying rotation. Also switch the worker's UI handle to channel-delivery so Task 11 can dispatch the cold decode before the window exists.

**Files:**
- Modify: `app/src/main.rs`
- Test: `app/src/main.rs`

- [ ] **Step 1: Write the failing test for the pure resolver**

Add to `mod tests`:

```rust
#[test]
fn resolve_show_classifies_full_preview_and_miss() {
    let cache: Cache = Arc::new(Mutex::new(HashMap::new()));
    let full = PathBuf::from("/d/full.jpg");
    let prev = PathBuf::from("/d/prev.jpg");
    let miss = PathBuf::from("/d/miss.jpg");
    cache.lock().unwrap().insert(full.clone(), Cached { buffer: Arc::new(image::RgbaImage::new(1, 1)), cap: 16384 });
    cache.lock().unwrap().insert(prev.clone(), Cached { buffer: Arc::new(image::RgbaImage::new(1, 1)), cap: 4096 });

    assert!(matches!(resolve_show(&cache, &full, 16384), ShowSource::Full(_)));
    assert!(matches!(resolve_show(&cache, &prev, 16384), ShowSource::Preview(_)));
    assert!(matches!(resolve_show(&cache, &miss, 16384), ShowSource::Miss));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p app resolve_show`
Expected: FAIL — `ShowSource`/`resolve_show` undefined.

- [ ] **Step 3: Implement `ShowSource` + `resolve_show` and rewrite the worker**

Add:

```rust
/// What the cache can offer for a path, relative to the full cap.
enum ShowSource {
    Full(Arc<image::RgbaImage>),    // cached at >= full_cap: show directly
    Preview(Arc<image::RgbaImage>), // cached at preview only: show now, upgrade later
    Miss,                           // not cached: decode at full directly
}

fn resolve_show(cache: &Cache, path: &Path, full_cap: u32) -> ShowSource {
    match cache.lock().unwrap().get(path) {
        Some(c) if c.cap >= full_cap => ShowSource::Full(c.buffer.clone()),
        Some(c) => ShowSource::Preview(c.buffer.clone()),
        None => ShowSource::Miss,
    }
}
```

Rewrite `spawn_decode_worker` (UI handle via channel; pending-upgrade loop):

```rust
fn spawn_decode_worker(
    weak_rx: mpsc::Receiver<slint::Weak<AppWindow>>,
    cache: Cache,
    plan: imageset::PrefetchPlan,
) -> mpsc::Sender<Job> {
    let (tx, rx) = mpsc::channel::<Job>();
    std::thread::spawn(move || {
        let mut weak: Option<slint::Weak<AppWindow>> = None;
        let mut current: Option<(PathBuf, Arc<image::RgbaImage>)> = None;
        let mut turns: i32 = 0;
        let mut pending_upgrade: Option<PathBuf> = None;

        loop {
            // If an upgrade is pending and the queue is idle, perform it; otherwise block.
            let job = if pending_upgrade.is_some() {
                match rx.try_recv() {
                    Ok(j) => Some(j),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            } else {
                match rx.recv() {
                    Ok(j) => Some(j),
                    Err(_) => break,
                }
            };

            match job {
                Some(first) => {
                    let (show, delta) = reduce_batch(drain_batch(first, &rx));
                    if let Some((path, caption)) = show {
                        pending_upgrade = None; // a fresh Show cancels any pending upgrade
                        let base = match resolve_show(&cache, &path, plan.full_cap) {
                            ShowSource::Full(b) => b,
                            ShowSource::Preview(b) => {
                                pending_upgrade = Some(path.clone());
                                b
                            }
                            ShowSource::Miss => {
                                match decode::display_image(&path, plan.full_cap) {
                                    Ok(img) => {
                                        let b = Arc::new(img);
                                        cache.lock().unwrap().insert(
                                            path.clone(),
                                            Cached { buffer: b.clone(), cap: plan.full_cap },
                                        );
                                        b
                                    }
                                    Err(e) => {
                                        let msg = format!("Can't display {}: {e}", file_name_of(&path));
                                        let w = weak.get_or_insert_with(|| weak_rx.recv().expect("UI handle"));
                                        let _ = w.upgrade_in_event_loop(move |c| c.set_status_text(msg.into()));
                                        continue;
                                    }
                                }
                            }
                        };
                        current = Some((path, base));
                        turns = 0;
                        let w = weak.get_or_insert_with(|| weak_rx.recv().expect("UI handle"));
                        push_frame(w, &current, turns, caption, true);
                    }
                    if delta != 0 && current.is_some() {
                        turns = (turns + delta).rem_euclid(4);
                        let w = weak.get_or_insert_with(|| weak_rx.recv().expect("UI handle"));
                        push_frame(w, &current, turns, None, false);
                    }
                }
                None => {
                    // Idle + pending upgrade: decode the full and swap it in (re-applying turns).
                    let path = pending_upgrade.take().expect("pending upgrade present");
                    if let Ok(img) = decode::display_image(&path, plan.full_cap) {
                        let b = Arc::new(img);
                        cache.lock().unwrap().insert(
                            path.clone(),
                            Cached { buffer: b.clone(), cap: plan.full_cap },
                        );
                        if current.as_ref().map_or(false, |(p, _)| *p == path) {
                            current = Some((path, b));
                            let w = weak.get_or_insert_with(|| weak_rx.recv().expect("UI handle"));
                            push_frame(w, &current, turns, None, false); // is_new=false → keeps zoom; re-applies turns
                        }
                    }
                }
            }
        }
    });
    tx
}
```

Update `main`: create the handle channel and deliver the handle after `AppWindow::new`. Where the worker was spawned, replace with:

```rust
let (weak_tx, weak_rx) = mpsc::channel::<slint::Weak<AppWindow>>();
let decode_tx = spawn_decode_worker(weak_rx, cache.clone(), plan);
```

and immediately after `let ui = AppWindow::new()?;` add:

```rust
let _ = weak_tx.send(ui.as_weak());
```

(Note: `AppWindow::new()` currently precedes the cache/worker setup; keep `weak_tx.send` right after the worker is spawned for now — it is reordered in Task 11. If the worker is spawned after `AppWindow::new`, send the handle on the next line.)

- [ ] **Step 4: Run tests + build**

Run: `cargo test -p app resolve_show && cargo build -p app`
Expected: PASS / clean build. The threaded upgrade path is exercised manually (Task 11 manual list: next2 preview→sharpen, and rotate-during-preview keeps rotation).

- [ ] **Step 5: Commit**

```bash
git add app/src/main.rs
git commit -m "feat(app): preview→full upgrade (skip-on-newer, keeps rotation)"
```

---

## Task 11: Cold-start — decode before UI setup (app)

Dispatch the initial `Job::Show` before `AppWindow::new`/backend init so the decode overlaps UI construction; deliver the UI handle once the window exists. Exactly one decode (the worker is the only decoder).

**Files:**
- Modify: `app/src/main.rs`

- [ ] **Step 1: Reorder `main`**

Restructure the top of `main` to this order (move worker creation + the cold `Show` dispatch **before** the backend selector and `AppWindow::new`):

```rust
fn main() -> Result<(), slint::PlatformError> {
    let args = parse_args(std::env::args());
    let plan = imageset::DEFAULT_PLAN;

    // Cache + foreground worker first, so the cold image's decode overlaps backend init
    // and AppWindow construction. The worker blocks for the UI handle only at its first push.
    let cache: Cache = Arc::new(Mutex::new(HashMap::new()));
    let (weak_tx, weak_rx) = mpsc::channel::<slint::Weak<AppWindow>>();
    let decode_tx = spawn_decode_worker(weak_rx, cache.clone(), plan);
    let prefetch_tx = spawn_prefetch_worker(cache.clone());

    if let Some(path) = args.path.clone() {
        let _ = decode_tx.send(Job::Show { path, caption: None });
    }

    // Now build the UI (overlaps the in-flight decode).
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("femtovg".into())
        .select()?;
    let ui = AppWindow::new()?;
    let _ = weak_tx.send(ui.as_weak()); // unblocks the worker's first push

    // ... the rest of main (fullscreen cell, saved/snapshot, handlers, config restore /
    //     startup-sizing priority, attach_view_handlers, nav, directory scan, run()) ...
}
```

Adjust the remaining body:
- Remove the now-duplicated cache/worker/prefetch creation lines further down (they moved up).
- The `match args.path` block that set `"Loading…"`/caption and sent `Job::Show` must **no longer send** the `Show` (already dispatched above). Keep the directory-scan thread and the `None` branch. Set the initial status/caption right after `AppWindow::new` instead:

```rust
match args.path.clone() {
    Some(path) => {
        ui.set_status_text("Loading…".into());
        ui.set_caption(file_name_of(&path).into());
        // (Job::Show already dispatched before AppWindow::new.)
        let nav_scan = nav.clone();
        let weak = ui.as_weak();
        let pf = prefetch_tx.clone();
        std::thread::spawn(move || {
            let set = imageset::ImageSet::from_file(&path);
            let cap = {
                let mut g = nav_scan.lock().unwrap();
                *g = set;
                caption_for(g.position(), g.len(), &path)
            };
            send_prefetch(&nav_scan, &plan, &pf);
            let _ = weak.upgrade_in_event_loop(move |c| c.set_caption(cap.into()));
        });
    }
    None => ui.set_status_text("No image. Usage: photoviewer <path>".into()),
}
```

- [ ] **Step 2: Build + test**

Run: `cargo build -p app && cargo test -p app`
Expected: clean build, all tests pass.

- [ ] **Step 3: Manual smoke test**

Run: `cargo run -p app --release -- <a-large.jpg>`
Expected: the window opens at ~80% of the monitor at the image's aspect; the image appears (cold decode overlapped setup). Navigate next/prev — prev1/next1 are instant-full; stepping to next2 shows a frame that then sharpens. Rotate (R/E) during the brief next2 preview keeps the rotation after it sharpens. Maximize the window, navigate — it is **not** resized. `--restore-geometry` reopens at the last size/position.

- [ ] **Step 4: Commit**

```bash
git add app/src/main.rs
git commit -m "perf(app): start the cold image's decode before UI setup"
```

---

## Task 12: Clamp `full_cap` to GL_MAX_TEXTURE_SIZE (app)

Read the GPU texture limit once at renderer setup and use `full_cap = min(16384, GL_MAX)`; clamp the displayed buffer at texture-build time as a backstop (the cold decode may run before the limit is known). Closes the silent-black-frame hole on 8192-cap GPUs.

**Files:**
- Modify: `app/Cargo.toml`, `app/src/main.rs`

- [ ] **Step 1: Add the `glow` dependency**

In `app/Cargo.toml` `[dependencies]` (matches femtovg 0.23.2's `glow ^0.17`):

```toml
glow = "0.17"
```

Run: `cargo build -p app`
Expected: builds.

- [ ] **Step 2: Shared limit + effective cap + backstop clamp**

In `app/src/main.rs`:

(a) Add imports near the top:

```rust
use std::sync::atomic::{AtomicU32, Ordering};
use glow::HasContext;
```

(b) Add a shared cell and effective-cap helper:

```rust
/// GPU GL_MAX_TEXTURE_SIZE, 0 until read at renderer setup.
type TexLimit = Arc<AtomicU32>;

/// full_cap clamped to the GPU's texture limit (0 = unknown → optimistic plan cap).
fn effective_full_cap(plan: &imageset::PrefetchPlan, limit: &TexLimit) -> u32 {
    match limit.load(Ordering::Relaxed) {
        0 => plan.full_cap,
        m => plan.full_cap.min(m),
    }
}

/// Downscale a buffer to the known GPU texture limit if it exceeds it (backstop for the
/// cold decode that ran before the limit was known). No-op when within the limit/unknown.
fn clamp_to_texture_limit(img: Arc<image::RgbaImage>, limit: &TexLimit) -> Arc<image::RgbaImage> {
    let m = limit.load(Ordering::Relaxed);
    if m == 0 {
        return img;
    }
    let (w, h) = img.dimensions();
    if w <= m && h <= m {
        return img;
    }
    Arc::new(decode::downscale_to_fit((*img).clone(), m))
}
```

(c) Thread `limit: TexLimit` into `spawn_decode_worker` and `spawn_prefetch_worker` (add a param), and into `push_frame` (add a param). In both workers, replace `plan.full_cap` with `effective_full_cap(&plan, &limit)` at each decode/`obtain_base`/`resolve_show` use site. In `push_frame`, clamp before building the buffer:

```rust
fn push_frame(
    weak: &slint::Weak<AppWindow>,
    current: &Option<(PathBuf, Arc<image::RgbaImage>)>,
    turns: i32,
    caption: Option<String>,
    is_new_image: bool,
    limit: &TexLimit,
) {
    let Some((path, base)) = current else { return };
    let disp = clamp_to_texture_limit(rotate_turns(base, turns), limit);
    // ... unchanged: build SharedPixelBuffer from disp.as_raw()/width()/height(), info, push ...
}
```

Update every `push_frame(...)` call inside the worker to pass `&limit` as the last arg (the worker captures `limit`).

(d) In `main`, create the limit and register the rendering notifier right after `AppWindow::new`:

```rust
let tex_limit: TexLimit = Arc::new(AtomicU32::new(0));
{
    let tex_limit = tex_limit.clone();
    let _ = ui.window().set_rendering_notifier(move |state, api| {
        if let (slint::RenderingState::RenderingSetup,
                slint::GraphicsAPI::NativeOpenGL { get_proc_address }) = (state, api)
        {
            // SAFETY: GL context is current during RenderingSetup; read-only query only.
            let limit = unsafe {
                let gl = glow::Context::from_loader_function_cstr(|s| get_proc_address(s));
                gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE)
            };
            tex_limit.store(limit.max(0) as u32, Ordering::Relaxed);
        }
    });
}
```

Pass `tex_limit.clone()` into the worker spawns: `spawn_decode_worker(weak_rx, cache.clone(), plan, tex_limit.clone())` and `spawn_prefetch_worker(cache.clone(), tex_limit.clone())`. (The notifier is registered after `AppWindow::new`, which is after the worker spawn from Task 11 — that's fine; the worker reads `limit` lazily and the backstop clamp in `push_frame` covers the first frame, since `RenderingSetup` fires before the first paint.)

- [ ] **Step 3: Build + test**

Run: `cargo build -p app && cargo test -p app`
Expected: clean build (note the agent-flagged caveat — confirm `glow` 0.17's `from_loader_function_cstr` / `get_parameter_i32` / `MAX_TEXTURE_SIZE` compile; they are stable across 0.16–0.17). All tests pass.

- [ ] **Step 4: Manual verification**

Run: `cargo run -p app --release -- <a-60MP.jpg>`
Expected: a 60 MP photo zooms to true 1:1 (native pixels). On a GPU with `GL_MAX_TEXTURE_SIZE` 8192 (or temporarily hardcode the stored limit to 8192 to simulate), a >8192-side image shows **downscaled, not black**.

- [ ] **Step 5: Commit**

```bash
git add app/Cargo.toml app/src/main.rs Cargo.lock
git commit -m "fix(app): clamp full_cap to GL_MAX_TEXTURE_SIZE (no silent black frame)"
```

---

## Final verification

- [ ] Run the whole suite: `cargo test` (root) and `cargo build --release`.
- [ ] Run the CI Linux legs locally with `act` before pushing (per project memory).
- [ ] Manual pass over the spec's "Manual" testing list (window 80%/aspect open + right aspect immediately; refit on nav; left alone maximized/fullscreen; `--restore-geometry`; 60 MP 1:1; prev1/next1 instant-full; next2 preview→sharpen; 8192-GPU downscale-not-black; large-photo fit-view sharpness acceptable).

---

## Self-review notes (author)

- **Spec coverage:** C1 → Tasks 1–2; C2 → Tasks 3–5; C3 → Tasks 5,10; C4 → Tasks 6,7,9 + aspect-presize in 9; C5 → Task 8; GL clamp → Task 12; cold-start (decode-early/defer-RGBA/aspect-presize) → Tasks 11/2/9. Memory/limitations are documentation-only.
- **Type consistency:** `Cached { buffer, cap }`, `obtain_base(cache, path, target_cap, decode)`, `PrefetchPlan`/`DEFAULT_PLAN`/`targets()`/`keep_set()`, `fit_80_dims`, `Args { path, restore_geometry }`/`parse_args`, `ShowSource`/`resolve_show`, `TexLimit`/`effective_full_cap`/`clamp_to_texture_limit`, and `push_frame(..., limit)` are used identically across tasks.
- **Sequencing:** each task leaves the build green; the worker is rewritten once (Task 10) then only reordered (11) and parameter-extended (12).
