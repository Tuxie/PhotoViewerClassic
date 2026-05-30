# Zen Migration Phase 1 — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace image-rs (`image`) and `fast_image_resize` entirely with the imazen `zen*` ecosystem for decoding, the pixel-buffer type, downscaling, and rotation/flip — at behavioral parity for today's four formats (JPEG, PNG, WebP, GIF), with parallel decode enabled for fast cold start.

**Architecture:** All decoding stays funnelled through `crates/decode/src/lib.rs`. Each file is decoded by a per-format zen decoder selected via `zencodec`'s magic-byte format detection, producing a `zenpixels::PixelBuffer`. EXIF orientation (read with the retained `kamadak-exif`) and downscale-to-fit are applied together in one `zenresize` pass. The buffer type threaded through `app/src/main.rs`'s cache/prefetch/rotation changes from `image::RgbaImage` to `zenpixels::PixelBuffer`; interactive R/E rotation keeps its base→`rotate_turns` re-derivation but swaps the engine from `image::imageops` to `zenresize` `OrientOutput`. No `image`/`fast_image_resize`, no hand-rolled pixel code, no Slint transform, no `ViewState` rework.

**Tech Stack:** Rust edition 2024, workspace with `app` + `crates/{decode,imageset,viewstate,config}`. New deps: `zencodec`, `zenpixels`, `zenresize`, `zenjpeg`, `zenpng`, `zenwebp`, `zengif` (all imazen, pinned exact). Retained: `kamadak-exif`, `slint` 1.16 (femtovg), `glow`, `i-slint-backend-winit`. Tests: `cargo test` + committed binary fixtures. CI: GitHub Actions (ubuntu/macos/windows), MSRV must be ≥1.93.

**Spec:** `docs/superpowers/specs/2026-05-30-zen-codec-decode-migration-design.md`

> **⚠ Environment note (read before starting):** The zen* crates are **pre-1.0 with a self-described *prerelease* decoder API**, and they were NOT compile-verified when this plan was written (the authoring environment was offline). **Task 1 is a mandatory API-discovery spike** that records the exact, real signatures into `docs/superpowers/plans/zen-api-reference.md`. Every later task that calls a zen decoder, `zenpixels::PixelBuffer`, or `zenresize` says **"reconcile with the API reference from Task 1."** Where this plan shows decoder-call code, treat it as the *expected shape*: if the spike found a different signature, use the spike's signature and keep the surrounding structure/tests. The zenresize `OrientOutput`/`Filter`/`FitMode`/`Resizer` shapes and the EXIF→OrientOutput mapping were extracted verbatim from `zenresize-0.3.1` source and are high-confidence.

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `Cargo.toml` (workspace) | Workspace deps + `license` field | Modify: drop `image`/`fast_image_resize`; add zen* workspace deps; add `license = "AGPL-3.0-only"` |
| `crates/decode/Cargo.toml` | Decode crate deps | Modify: drop `image`/`fast_image_resize`; add zen* + `zenresize` |
| `crates/decode/src/lib.rs` | The single decode/processing chokepoint | Rewrite internals: `DecodeError`, dispatch, `display_image`, `display_dimensions`, `rotate_turns` engine helper; emit `PixelBuffer` |
| `crates/decode/src/error.rs` | `DecodeError` enum + `Display` | Create |
| `crates/decode/src/orient.rs` | EXIF tag (1..8) → `zenresize::OrientOutput` mapping (pure data, NOT pixel code) | Create |
| `crates/decode/tests/fixtures/` | Tiny real sample images per format | Create (binary fixtures) |
| `app/Cargo.toml` | App deps | Modify: drop `image`; add `zenpixels` |
| `app/src/main.rs` | Cache/prefetch/rotation/Slint boundary | Modify: `Arc<image::RgbaImage>` → `Arc<zenpixels::PixelBuffer>`; `rotate_turns` calls `decode::rotate_buffer`; Slint copy via `as_contiguous_bytes` |
| `LICENSE` | AGPL-3.0 license text | Create |
| `README.md` | Tech-stack framing | Modify |
| `AGENTS.md` | Tech-stack + formats + line-55 tag correction | Modify |
| `.github/workflows/ci.yml` | Pin toolchain ≥1.93 | Modify |
| `docs/superpowers/plans/zen-api-reference.md` | Recorded real zen* API signatures (Task 1 output) | Create |

**Note on `crates/decode/src/lib.rs` size:** it is ~290 lines today and will shrink (rotation/flip/resize logic moves into zen calls). Splitting `error.rs` and `orient.rs` out keeps `lib.rs` focused on the decode pipeline. No further split needed.

---

## Task 1: API-discovery spike — wire deps and record real signatures

**Files:**
- Modify: `Cargo.toml` (workspace `[workspace.dependencies]`)
- Modify: `crates/decode/Cargo.toml`
- Create: `crates/decode/src/bin/spike.rs` (throwaway, deleted at end of task)
- Create: `docs/superpowers/plans/zen-api-reference.md`

> This task MUST run on a machine with network access (crates.io). It produces the confirmed API surface every later task depends on. Nothing here is permanent except the recorded reference doc and the dependency lines.

- [ ] **Step 1: Add the zen* dependencies to the workspace manifest**

In `Cargo.toml`, inside `[workspace.dependencies]`, add (keep `kamadak-exif`, remove nothing yet):

```toml
# imazen zen* decode + processing stack (AGPL-3.0; see LICENSE). Pinned exact —
# pre-1.0 with a prerelease decoder API and a known yank history.
zencodec = "=0.1.20"
zenpixels = { version = "=0.2.11", features = ["rgb"] }
zenresize = "=0.3.1"
zenjpeg = { version = "=0.8.3", default-features = false, features = ["decoder", "zencodec", "parallel"] }
zenpng = { version = "=0.4.4", default-features = false }
zenwebp = { version = "=0.4.4", default-features = false }
zengif = "=0.7.3"
```

- [ ] **Step 2: Reference them from the decode crate**

In `crates/decode/Cargo.toml`, under `[dependencies]`, ADD (do not remove `image`/`fast_image_resize` yet — the spike coexists):

```toml
zencodec = { workspace = true }
zenpixels = { workspace = true }
zenresize = { workspace = true }
zenjpeg = { workspace = true }
zenpng = { workspace = true }
zenwebp = { workspace = true }
zengif = { workspace = true }
```

- [ ] **Step 3: Fetch and check the dependency graph**

Run: `cargo fetch && cargo tree -p decode -e features | grep -iE "zen|archmage|rayon|image|fast_image" `
Expected: the zen* crates resolve; note whether `image`/`fast_image_resize` appear anywhere OTHER than as `zenresize` dev-deps (they should not be runtime deps). Record the exact resolved versions.

- [ ] **Step 4: Write a throwaway spike binary to force the real APIs to compile**

Create `crates/decode/src/bin/spike.rs`. Fill in the call sites from `cargo doc --open -p zenjpeg -p zenpixels -p zenresize -p zencodec`; this is the discovery step — make it compile against the REAL API:

```rust
// THROWAWAY — deleted at the end of Task 1. Purpose: force the real zen* APIs to
// compile so their exact signatures can be recorded. Decode one JPEG, orient+resize it,
// get contiguous RGBA8 bytes out.
fn main() {
    let bytes = std::fs::read(std::env::args().nth(1).expect("path arg")).unwrap();

    // (A) format detection — confirm the real call (module path, return type).
    let fmt = zencodec::ImageFormatRegistry::detect(&bytes);
    eprintln!("format = {fmt:?}");

    // (B) JPEG decode -> a buffer. Confirm: decoder type, decode method, output type,
    //     how to get (w, h) and contiguous RGBA8/RGB8 bytes + channel count.
    //     Reconcile this block with docs.rs/zenjpeg/0.8.3 decoder module.
    let decoded = zenjpeg::decoder::DecodeConfig::new().decode(&bytes).unwrap();
    eprintln!("decoded type printed via debug or accessors here");

    // (C) wrap/convert to zenpixels::PixelBuffer — confirm from_vec/try_new arg order
    //     and the PixelDescriptor constants (RGBA8_SRGB / RGB8_SRGB).
    // let buf = zenpixels::PixelBuffer::from_vec(data, w, h, zenpixels::PixelDescriptor::RGBA8_SRGB);

    // (D) zenresize downscale + orient in one pass — confirm builder + Resizer/StreamingResize.
    let cfg = zenresize::ResizeConfig::builder(/* in_w */ 0, /* in_h */ 0, 0, 0)
        .filter(zenresize::Filter::Lanczos)
        .format(zenresize::PixelDescriptor::RGBA8_SRGB)
        .fit(zenresize::FitMode::Within, 4096, 4096)
        .build();
    let _ = zenresize::Resizer::new(&cfg);
    let _ = zenresize::OrientOutput::Rotate90; // confirm variant set compiles

    // (E) contiguous bytes out — confirm as_contiguous_bytes()/copy_to_contiguous_bytes().
}
```

- [ ] **Step 5: Iterate the spike until it compiles, recording each real signature**

Run: `cargo build -p decode --bin spike` (repeat, fixing each call against `cargo doc`).
Expected: eventually compiles. Each compiler error reveals a real signature — capture it.

- [ ] **Step 6: Record the confirmed API into the reference doc**

Create `docs/superpowers/plans/zen-api-reference.md` capturing, verbatim from the compiling spike, the EXACT:
- `zencodec::ImageFormatRegistry::detect` signature + the `ImageFormat` variant names for JPEG/PNG/WebP/GIF
- Each decoder's entry type + decode method + output type, for zenjpeg / zenpng / zenwebp / zengif
- How to obtain `(width, height)`, channel count (RGB vs RGBA), and contiguous bytes from each decoder's output
- `zenpixels::PixelBuffer` constructor (`from_vec`/`try_new`) arg order + the `PixelDescriptor` constants
- `zenpixels::PixelBuffer` accessors: `width()`, `height()`, `as_contiguous_bytes()`, `copy_to_contiguous_bytes()`
- `zenresize`: `ResizeConfig::builder(...)` exact params, builder methods, the output-dims accessor name, `Resizer::resize`/`resize_into`, and how `OrientOutput` is applied (`Resizer` vs `StreamingResize::with_orientation`)

- [ ] **Step 7: Delete the spike and commit the reference + deps**

```bash
rm crates/decode/src/bin/spike.rs
rmdir crates/decode/src/bin 2>/dev/null || true
git add Cargo.toml Cargo.lock crates/decode/Cargo.toml docs/superpowers/plans/zen-api-reference.md
git commit -m "build(decode): add imazen zen* deps; record confirmed API reference (spike)"
git push
```

Expected: `cargo build -p decode` still succeeds (image-rs still present alongside; removed in Task 9).

---

## Task 2: `DecodeError` type

**Files:**
- Create: `crates/decode/src/error.rs`
- Modify: `crates/decode/src/lib.rs` (add `mod error; pub use error::DecodeError;`)

- [ ] **Step 1: Write the failing test**

Append to `crates/decode/src/error.rs`:

```rust
//! The single error type at the decode crate boundary. Replaces `image::ImageError`.

use std::fmt;

#[derive(Debug)]
pub enum DecodeError {
    /// Filesystem read failed.
    Io(std::io::Error),
    /// The bytes are not a format PVC decodes.
    Unsupported,
    /// A decoder rejected the bytes (corrupt, truncated, or — for HEIC later — outside
    /// the decoder's partial conformance coverage).
    Decode(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Io(e) => write!(f, "{e}"),
            DecodeError::Unsupported => write!(f, "unsupported image format"),
            DecodeError::Decode(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl From<std::io::Error> for DecodeError {
    fn from(e: std::io::Error) -> Self {
        DecodeError::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_renders_each_variant() {
        let io = DecodeError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"));
        assert_eq!(io.to_string(), "nope");
        assert_eq!(DecodeError::Unsupported.to_string(), "unsupported image format");
        assert_eq!(DecodeError::Decode("bad jpeg".into()).to_string(), "bad jpeg");
    }

    #[test]
    fn io_error_converts_via_from() {
        let e: DecodeError = std::io::Error::other("x").into();
        assert!(matches!(e, DecodeError::Io(_)));
    }
}
```

Add to the TOP of `crates/decode/src/lib.rs` (after the existing `use` lines):

```rust
mod error;
pub use error::DecodeError;
```

- [ ] **Step 2: Run test to verify it fails (then passes)**

Run: `cargo test -p decode error::tests`
Expected: compiles and PASSES (this type is self-contained). If `lib.rs` doesn't yet compile due to other in-flight changes, run after Task 6; the error module itself is independent.

- [ ] **Step 3: Commit**

```bash
git add crates/decode/src/error.rs crates/decode/src/lib.rs
git commit -m "feat(decode): add DecodeError boundary type"
git push
```

---

## Task 3: EXIF orientation → `OrientOutput` mapping

**Files:**
- Create: `crates/decode/src/orient.rs`
- Modify: `crates/decode/src/lib.rs` (`mod orient;`)

> Pure data mapping. The `OrientOutput` variant names are verbatim from `zenresize-0.3.1/src/streaming.rs`; confirm against the Task 1 reference before relying on them.

- [ ] **Step 1: Write the failing test**

Create `crates/decode/src/orient.rs`:

```rust
//! Map an EXIF Orientation tag (1..=8) to a `zenresize::OrientOutput`. Pure data — the
//! actual pixel transform is done by zenresize. Mirrors the legacy `apply_orientation`
//! table: 5 = transpose (mirror-H + rot270), 7 = transverse (mirror-H + rot90).

use zenresize::OrientOutput;

/// EXIF Orientation (1..=8) → the single zenresize transform that normalizes it.
/// Unknown / 1 → `None` (identity).
pub fn orient_output(exif: u16) -> OrientOutput {
    match exif {
        2 => OrientOutput::FlipHorizontal,
        3 => OrientOutput::Rotate180,
        4 => OrientOutput::FlipVertical,
        5 => OrientOutput::Transpose,
        6 => OrientOutput::Rotate90,
        7 => OrientOutput::Transverse,
        8 => OrientOutput::Rotate270,
        _ => OrientOutput::None, // 1 or unknown
    }
}

/// True if the orientation swaps width and height (90°/270° rotations and the two
/// transposes). Used by `display_dimensions` to report post-orientation dims.
pub fn swaps_dimensions(exif: u16) -> bool {
    matches!(exif, 5 | 6 | 7 | 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_all_eight_orientations() {
        assert!(matches!(orient_output(1), OrientOutput::None));
        assert!(matches!(orient_output(2), OrientOutput::FlipHorizontal));
        assert!(matches!(orient_output(3), OrientOutput::Rotate180));
        assert!(matches!(orient_output(4), OrientOutput::FlipVertical));
        assert!(matches!(orient_output(5), OrientOutput::Transpose));
        assert!(matches!(orient_output(6), OrientOutput::Rotate90));
        assert!(matches!(orient_output(7), OrientOutput::Transverse));
        assert!(matches!(orient_output(8), OrientOutput::Rotate270));
    }

    #[test]
    fn unknown_orientation_is_identity() {
        assert!(matches!(orient_output(0), OrientOutput::None));
        assert!(matches!(orient_output(99), OrientOutput::None));
    }

    #[test]
    fn dimension_swaps_match_rotating_orientations() {
        for o in [5u16, 6, 7, 8] {
            assert!(swaps_dimensions(o), "orientation {o} swaps W/H");
        }
        for o in [1u16, 2, 3, 4] {
            assert!(!swaps_dimensions(o), "orientation {o} keeps W/H");
        }
    }
}
```

Add `mod orient;` near the top of `crates/decode/src/lib.rs`.

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test -p decode orient::tests`
Expected: PASS. If a variant name differs from the Task 1 reference, update the match arm to the real name and re-run.

- [ ] **Step 3: Commit**

```bash
git add crates/decode/src/orient.rs crates/decode/src/lib.rs
git commit -m "feat(decode): EXIF orientation -> zenresize OrientOutput mapping"
git push
```

---

## Task 4: Resize-to-fit wrapper (no-upscale guard preserved)

**Files:**
- Modify: `crates/decode/src/lib.rs` (replace `fast_image_resize` helpers with a `zenresize` wrapper; keep `fit_dims`)

> Keep the existing pure `fit_dims(w, h, max) -> Option<(u32, u32)>` function unchanged (it returns `None` when already within `max` — the no-upscale guard). Replace `resize_u8x4`/`resize_u8x3` with one zenresize call. Reconcile the `ResizeConfig`/`Resizer` calls with the Task 1 reference.

- [ ] **Step 1: Write the failing test**

Add to `crates/decode/src/lib.rs` (inside the existing `#[cfg(test)] mod tests`):

```rust
#[test]
fn fit_dims_guards_against_upscaling() {
    // within max -> None (no resize)
    assert_eq!(super::fit_dims(30, 20, 40), None);
    // larger -> Some, limiting side hits max
    assert_eq!(super::fit_dims(100, 50, 40), Some((40, 20)));
}
```

(The `fit_dims` function already exists and is unchanged — this test pins the no-upscale contract that must survive the migration.)

- [ ] **Step 2: Run the test**

Run: `cargo test -p decode fit_dims_guards_against_upscaling`
Expected: PASS (function pre-exists). If it fails, `fit_dims` was altered — restore the original behavior.

- [ ] **Step 3: Replace the resize implementation**

Replace the bodies of `resize_u8x4`/`resize_u8x3` (or their single replacement) with a `zenresize`-backed helper. EXPECTED SHAPE — reconcile with Task 1 reference:

```rust
use zenresize::{Filter, FitMode, PixelDescriptor, ResizeConfig, Resizer};

/// Downscale interleaved 8-bit pixels so the longest side is <= `max`, preserving aspect.
/// `channels` is 3 (RGB) or 4 (RGBA). Returns (bytes, out_w, out_h). No upscaling: when
/// already within `max`, returns the input unchanged.
fn resize_to_fit(src: &[u8], w: u32, h: u32, channels: u8, max: u32) -> (Vec<u8>, u32, u32) {
    let Some((nw, nh)) = fit_dims(w, h, max) else {
        return (src.to_vec(), w, h);
    };
    let fmt = if channels == 4 { PixelDescriptor::RGBA8_SRGB } else { PixelDescriptor::RGB8_SRGB };
    let cfg = ResizeConfig::builder(w as usize, h as usize, nw as usize, nh as usize)
        .filter(Filter::Lanczos)
        .format(fmt)
        .fit(FitMode::Within, max as usize, max as usize)
        .build();
    let out = Resizer::new(&cfg).resize(src);
    (out, nw, nh)
}
```

- [ ] **Step 4: Build**

Run: `cargo build -p decode`
Expected: compiles. (Full pipeline tested in Task 11.)

- [ ] **Step 5: Commit**

```bash
git add crates/decode/src/lib.rs
git commit -m "feat(decode): zenresize downscale-to-fit, no-upscale guard preserved"
git push
```

---

## Task 5: Format detection + per-format decode dispatch

**Files:**
- Modify: `crates/decode/src/lib.rs` (add `detect_and_decode`)

> The decoder call sites are the LEAST-confirmed part of this plan. The structure (detect → match → per-format decode → `(bytes, w, h, channels)`) is fixed; the exact decode calls MUST come from the Task 1 reference. Each arm returns the raw decoded pixels plus dims and channel count, so the resize step is format-agnostic.

- [ ] **Step 1: Write the decode dispatch**

Add to `crates/decode/src/lib.rs`. EXPECTED SHAPE — reconcile each decoder arm with Task 1:

```rust
/// Raw decoded pixels before orientation/resize: interleaved 8-bit, with dims + channels.
struct RawImage {
    bytes: Vec<u8>,
    w: u32,
    h: u32,
    channels: u8, // 3 or 4
}

/// Magic-byte detect, then dispatch to the per-crate zen decoder. Phase 1 formats only.
fn detect_and_decode(bytes: &[u8]) -> Result<RawImage, DecodeError> {
    use zencodec::ImageFormat; // confirm enum + variant names from Task 1
    match zencodec::ImageFormatRegistry::detect(bytes) {
        Some(ImageFormat::Jpeg) => decode_jpeg(bytes),
        Some(ImageFormat::Png) => decode_png(bytes),
        Some(ImageFormat::WebP) => decode_webp(bytes),
        Some(ImageFormat::Gif) => decode_gif(bytes),
        _ => Err(DecodeError::Unsupported),
    }
}

// Each arm calls its decoder and extracts (bytes, w, h, channels). Map decoder errors to
// DecodeError::Decode(e.to_string()). channels = 4 if the decoder yields RGBA, 3 if RGB
// (preserves the deferred-RGBA memory win for opaque images).
fn decode_jpeg(bytes: &[u8]) -> Result<RawImage, DecodeError> {
    // EXPECTED SHAPE — replace the call + extraction with the exact zenjpeg API recorded
    // in Task 1's zen-api-reference.md. The four extraction lines below are what every arm
    // produces from its decoder's output type (a PixelBuffer or a raw {data,w,h} struct).
    let decoded = zenjpeg::decoder::DecodeConfig::new()
        .decode(bytes)
        .map_err(|e| DecodeError::Decode(e.to_string()))?;
    Ok(RawImage {
        w: decoded.width(),                 // accessor name per Task 1
        h: decoded.height(),                // accessor name per Task 1
        channels: decoded.channels(),       // 3 or 4, per Task 1 (or derive from descriptor)
        bytes: decoded.into_contiguous_rgba_or_rgb(), // exact getter per Task 1
    })
}
```

Then write `decode_png`, `decode_webp`, `decode_gif` as exact copies of `decode_jpeg`'s
structure, swapping `zenjpeg::decoder::DecodeConfig` for the `zenpng` / `zenwebp` / `zengif`
decoder entry point recorded in Task 1's reference. All four return `RawImage`; the only
per-format difference is the decoder type and (possibly) the output accessor names — keep the
`(w, h, channels, bytes)` extraction identical so the resize/orient step downstream is
format-agnostic. If a decoder's output is already a `zenpixels::PixelBuffer`, skip the
extraction and have that arm return the buffer directly via a `RawImage`-from-`PixelBuffer`
constructor (define it once, reuse in all four arms).

> This task's deliverable is FOUR real decode arms, each compiling against the confirmed API
> from Task 1 — not the skeleton above. The method names shown (`width()`, `into_contiguous_*`,
> etc.) are the *expected* shape; use whatever Task 1 recorded. If the spike found that, say,
> zenjpeg returns a `PixelBuffer` directly, the arm is a one-liner and `RawImage` may not be
> needed at all — collapse it.

- [ ] **Step 2: Build against the real decoders**

Run: `cargo build -p decode`
Expected: compiles once each arm uses the confirmed API. If a decoder's output is already a `zenpixels::PixelBuffer`, `RawImage` can borrow its bytes/dims directly — adjust `raw_from_zen` accordingly.

- [ ] **Step 3: Commit**

```bash
git add crates/decode/src/lib.rs
git commit -m "feat(decode): magic-byte format detection + per-format zen decode dispatch"
git push
```

---

## Task 6: `display_image` + `decode_to_rgba8` rewired to the new pipeline

**Files:**
- Modify: `crates/decode/src/lib.rs` (rewrite `display_image`, `decode_to_rgba8`; keep `read_orientation`/`orientation_from_bytes`)

> Pipeline: read → detect+decode → resize+orient in one zenresize pass → `PixelBuffer`. The public return type changes from `image::RgbaImage` to `zenpixels::PixelBuffer` (and `Result<_, DecodeError>`).

- [ ] **Step 1: Rewrite `display_image`**

EXPECTED SHAPE — reconcile zenresize orientation call with Task 1 (Resizer-with-orientation vs StreamingResize):

```rust
use zenpixels::{PixelBuffer, PixelDescriptor};

/// Decode `path`, normalize EXIF orientation, and downscale so both sides <= `max`, in a
/// single zenresize pass. Returns the rotation-0 BASE buffer (EXIF already applied).
pub fn display_image(path: &Path, max: u32) -> Result<PixelBuffer, DecodeError> {
    let bytes = std::fs::read(path)?;
    let raw = detect_and_decode(&bytes)?;
    let ori = orient::orient_output(orientation_from_bytes(&bytes).unwrap_or(1));

    // One pass: downscale-to-fit (no-upscale guarded) + EXIF orient. If both within max,
    // fit_dims returns None and we still need to APPLY orientation — so when there's no
    // resize but ori != None, run an identity-scale orient (see spec Rotation must-verify),
    // else just wrap the raw bytes.
    let descriptor = if raw.channels == 4 { PixelDescriptor::RGBA8_SRGB } else { PixelDescriptor::RGB8_SRGB };
    let oriented = resize_and_orient(&raw.bytes, raw.w, raw.h, descriptor, max, ori); // (bytes, w, h)
    Ok(PixelBuffer::from_vec(oriented.0, oriented.1, oriented.2, descriptor)) // confirm arg order (Task 1)
}

/// Full-resolution decode (no downscale), EXIF-oriented, as RGBA8. Used where a 1:1 buffer
/// is needed. Builds on the same pipeline with `max = u32::MAX`.
pub fn decode_to_rgba8(path: &Path) -> Result<PixelBuffer, DecodeError> {
    display_image(path, u32::MAX)
}
```

`resize_and_orient` extends Task 4's `resize_to_fit` to also apply `ori` (one zenresize pass). When `fit_dims` returns `None` and `ori == None`, return the input bytes unchanged; when `ori != None`, apply orientation per Task 1's API.

- [ ] **Step 2: Keep orientation reading unchanged**

`orientation_from_bytes` and `read_orientation` (kamadak-exif) are UNCHANGED. Verify they still compile (they don't depend on `image`).

- [ ] **Step 3: Build**

Run: `cargo build -p decode`
Expected: compiles. End-to-end decode is tested with real fixtures in Task 11.

- [ ] **Step 4: Commit**

```bash
git add crates/decode/src/lib.rs
git commit -m "feat(decode): display_image pipeline on zen decoders + zenresize (PixelBuffer out)"
git push
```

---

## Task 7: `display_dimensions` via zen header probes

**Files:**
- Modify: `crates/decode/src/lib.rs` (`display_dimensions`)

- [ ] **Step 1: Rewrite `display_dimensions`**

EXPECTED SHAPE — reconcile the probe call with Task 1 (`zencodec`/per-crate `ImageInfo::from_bytes`):

```rust
/// Header-only natural display dimensions (post-EXIF-orientation), without decoding pixels.
/// Magic-byte based (works for extension-less files, unlike the old extension probe).
pub fn display_dimensions(path: &Path) -> Option<(u32, u32)> {
    let bytes = std::fs::read(path).ok()?;
    let (w, h) = zencodec_probe_dims(&bytes)?; // confirm probe API from Task 1
    let exif = orientation_from_bytes(&bytes).unwrap_or(1);
    if orient::swaps_dimensions(exif) { Some((h, w)) } else { Some((w, h)) }
}
```

- [ ] **Step 2: Test with a fixture (after Task 11 fixtures exist) or build-only now**

Run: `cargo build -p decode`
Expected: compiles. A real-dimension assertion is added in Task 11.

- [ ] **Step 3: Commit**

```bash
git add crates/decode/src/lib.rs
git commit -m "feat(decode): header-only display_dimensions via zen magic-byte probe"
git push
```

---

## Task 8: `rotate_turns` engine swap (image::imageops → zenresize)

**Files:**
- Modify: `crates/decode/src/lib.rs` (add `pub fn rotate_buffer`)
- Modify: `app/src/main.rs` (`rotate_turns` calls `decode::rotate_buffer`)

> The base→display re-derivation contract is unchanged; only the engine changes. Expose a decode-crate helper so `main.rs` doesn't depend on zenresize directly.

- [ ] **Step 1: Add `rotate_buffer` to the decode crate**

EXPECTED SHAPE — identity-scale orient (see spec Rotation must-verify); reconcile with Task 1:

```rust
/// Rotate a decoded `PixelBuffer` by `turns` quarter-turns clockwise (1/2/3), returning a
/// new buffer. `turns == 0` is handled by the caller (shares the base Arc). Lossless 90°
/// rotation via zenresize at identity scale + OrientOutput.
pub fn rotate_buffer(base: &PixelBuffer, turns: i32) -> PixelBuffer {
    let ori = match turns.rem_euclid(4) {
        1 => OrientOutput::Rotate90,
        2 => OrientOutput::Rotate180,
        3 => OrientOutput::Rotate270,
        _ => return base.clone(), // 0: identity (caller normally avoids this)
    };
    apply_orient_identity(base, ori) // identity-scale zenresize pass; reconcile with Task 1
}
```

- [ ] **Step 2: Write the failing test (dims swap on odd turns)**

Add to `crates/decode/src/lib.rs` tests, using a real fixture from Task 11 (or a small constructed `PixelBuffer`):

```rust
#[test]
fn rotate_buffer_swaps_dims_on_quarter_turns() {
    // Build a 2x1 RGBA buffer (left red, right blue) via PixelBuffer::from_vec (Task 1 API).
    let px = vec![255,0,0,255,  0,0,255,255];
    let base = zenpixels::PixelBuffer::from_vec(px, 2, 1, zenpixels::PixelDescriptor::RGBA8_SRGB);
    assert_eq!((super::rotate_buffer(&base, 1).width(), super::rotate_buffer(&base, 1).height()), (1, 2));
    assert_eq!((super::rotate_buffer(&base, 2).width(), super::rotate_buffer(&base, 2).height()), (2, 1));
    assert_eq!((super::rotate_buffer(&base, 3).width(), super::rotate_buffer(&base, 3).height()), (1, 2));
}
```

Run: `cargo test -p decode rotate_buffer_swaps_dims_on_quarter_turns`
Expected: PASS.

- [ ] **Step 3: Point `main.rs::rotate_turns` at the new helper**

In `app/src/main.rs`, replace the `image::imageops::rotate90/180/270` calls in `rotate_turns` with `decode::rotate_buffer`, keeping the `turns == 0` shares-the-Arc fast path:

```rust
fn rotate_turns(base: &Arc<zenpixels::PixelBuffer>, turns: i32) -> Arc<zenpixels::PixelBuffer> {
    match turns.rem_euclid(4) {
        0 => Arc::clone(base),
        t => Arc::new(decode::rotate_buffer(base.as_ref(), t)),
    }
}
```

- [ ] **Step 4: Build (full app build happens in Task 9)**

Run: `cargo build -p decode && cargo test -p decode`
Expected: decode crate green.

- [ ] **Step 5: Commit**

```bash
git add crates/decode/src/lib.rs app/src/main.rs
git commit -m "feat(decode): rotate_buffer via zenresize; main.rs rotate_turns uses it"
git push
```

---

## Task 9: `main.rs` buffer-type swap + remove `image`/`fast_image_resize`

**Files:**
- Modify: `app/src/main.rs` (all `image::RgbaImage` → `zenpixels::PixelBuffer`; Slint copy)
- Modify: `app/Cargo.toml` (drop `image`; add `zenpixels`)
- Modify: `crates/decode/Cargo.toml` (drop `image`, `fast_image_resize`)
- Modify: `Cargo.toml` (drop `image` workspace dep; add `zenpixels` workspace dep)

- [ ] **Step 1: Swap the buffer type throughout `main.rs`**

Replace every `image::RgbaImage` with `zenpixels::PixelBuffer` in: `Cached.buffer`, the `Cache` type alias, `DecodedFrame`/`push_frame`, `ShowSource`, `obtain_base`, `resolve_show`, `clamp_to_texture_limit`, the worker closures, and `rotate_turns`. Add `use zenpixels::PixelBuffer;`. Remove `use glow::HasContext;`-adjacent `image` imports.

- [ ] **Step 2: Update the Slint pixel copy in `push_frame`**

Replace `disp.as_raw()` + `disp.width()/height()` with the `PixelBuffer` accessors (reconcile with Task 1):

```rust
let bytes = disp
    .as_contiguous_bytes()
    .map(|b| b.to_vec())
    .unwrap_or_else(|| disp.copy_to_contiguous_bytes());
let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
    &bytes, disp.width(), disp.height(),
);
```

> NOTE: `clamp_to_texture_limit` currently calls `decode::downscale_to_fit((*img).clone(), m)` on an `RgbaImage`. Re-point it at a `PixelBuffer`-based downscale (reuse Task 4's `resize_to_fit` exposed as `pub fn downscale_to_fit(buf: &PixelBuffer, max: u32) -> PixelBuffer`). Confirm `PixelBuffer` is `Clone` (Task 1); if not, build the clamped buffer without cloning.

- [ ] **Step 3: Update manifests — remove image-rs, add zenpixels**

`crates/decode/Cargo.toml`: delete `fast_image_resize` and `image` lines.
`app/Cargo.toml`: delete the `image = { workspace = true }` block (and its comment); add `zenpixels = { workspace = true }`.
`Cargo.toml`: delete the `image = { ... }` workspace dependency; add `zenpixels = { version = "=0.2.11", features = ["rgb"] }` (if not already present from Task 1) — keep a single source of truth.

- [ ] **Step 4: Build the whole workspace**

Run: `cargo build --workspace`
Expected: compiles with zero references to `image`/`fast_image_resize`.

- [ ] **Step 5: Confirm image-rs is fully gone from the lock file**

Run: `cargo tree -e no-dev -i image 2>&1; cargo tree -e no-dev -i fast_image_resize 2>&1`
Expected: both report the package is not in the (non-dev) tree. (`image`/`fast_image_resize` may still appear as `zenresize` DEV-deps — that's fine; `-e no-dev` excludes them.)

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock app/Cargo.toml app/src/main.rs crates/decode/Cargo.toml
git commit -m "refactor(app): thread zenpixels::PixelBuffer; remove image + fast_image_resize"
git push
```

---

## Task 10: Test fixtures + decode/orientation/resize tests

**Files:**
- Create: `crates/decode/tests/fixtures/{red2x1.png, sample.jpg, sample.webp, sample.gif, exif6.jpg}`
- Create: `crates/decode/tests/decode_fixtures.rs`

> The old in-memory tests used `image`'s ENCODER (`RgbaImage::save`), which no longer compiles. Replace with real committed fixtures decoded by the zen decoders.

- [ ] **Step 1: Generate tiny fixtures (one-time, on a machine with ImageMagick or similar)**

Create deterministic small images and commit them. Example commands (any tool works; keep them <1 KB where possible):

```bash
mkdir -p crates/decode/tests/fixtures
# 2x1: left red, right blue (RGBA PNG) — the canonical orientation/rotation probe
magick -size 2x1 xc:none -draw 'fill red point 0,0 fill blue point 1,0' crates/decode/tests/fixtures/red2x1.png
# 100x50 gradient JPEG/WebP/GIF for resize/decode dims
magick -size 100x50 gradient: crates/decode/tests/fixtures/sample.jpg
magick -size 100x50 gradient: crates/decode/tests/fixtures/sample.webp
magick -size 100x50 gradient: crates/decode/tests/fixtures/sample.gif
# 100x50 JPEG tagged EXIF orientation 6 (rotate 90 CW) — dims should report swapped
magick -size 100x50 gradient: -orient RightTop crates/decode/tests/fixtures/exif6.jpg
```

Confirm `exif6.jpg` actually carries Orientation=6: `exiftool crates/decode/tests/fixtures/exif6.jpg | grep Orientation` (or `identify -verbose`). If the tool strips it, set it explicitly with `exiftool -Orientation#=6 exif6.jpg`.

- [ ] **Step 2: Write the fixture tests**

Create `crates/decode/tests/decode_fixtures.rs`:

```rust
use std::path::Path;

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

#[test]
fn decodes_each_phase1_format_to_expected_dims() {
    for (name, w, h) in [("sample.jpg", 100, 50), ("sample.webp", 100, 50), ("sample.gif", 100, 50), ("red2x1.png", 2, 1)] {
        let buf = decode::display_image(&fixture(name), u32::MAX).expect(name);
        assert_eq!((buf.width(), buf.height()), (w, h), "{name}");
    }
}

#[test]
fn downscales_large_side_to_cap() {
    let buf = decode::display_image(&fixture("sample.jpg"), 40).unwrap();
    assert!(buf.width() <= 40 && buf.height() <= 40);
    assert_eq!(buf.width(), 40); // limiting side hits the cap
}

#[test]
fn applies_exif_orientation_6_as_dimension_swap() {
    // exif6.jpg is 100x50 stored, orientation 6 (rotate 90 CW) -> displayed 50x100.
    let buf = decode::display_image(&fixture("exif6.jpg"), u32::MAX).unwrap();
    assert_eq!((buf.width(), buf.height()), (50, 100));
    // header-only probe agrees
    assert_eq!(decode::display_dimensions(&fixture("exif6.jpg")), Some((50, 100)));
}

#[test]
fn unsupported_bytes_error_cleanly() {
    let dir = std::env::temp_dir();
    let p = dir.join("pvc_not_an_image.bin");
    std::fs::write(&p, b"this is not an image").unwrap();
    assert!(decode::display_image(&p, 4096).is_err());
}

#[test]
fn truncated_file_errors_without_panicking() {
    let bytes = std::fs::read(fixture("sample.jpg")).unwrap();
    let dir = std::env::temp_dir();
    let p = dir.join("pvc_truncated.jpg");
    std::fs::write(&p, &bytes[..bytes.len() / 2]).unwrap();
    assert!(decode::display_image(&p, 4096).is_err());
}
```

- [ ] **Step 3: Run the fixture tests**

Run: `cargo test -p decode --test decode_fixtures`
Expected: PASS. If orientation 6 yields `(100,50)` instead of `(50,100)`, the EXIF→OrientOutput mapping or the dims-swap probe is inverted — fix Task 3/Task 7.

- [ ] **Step 4: Run the FULL workspace suite**

Run: `cargo test --workspace`
Expected: all green (decode + viewstate + config + imageset + app gui_tests). Fix any remaining `image`-type references the compiler surfaces.

- [ ] **Step 5: Commit**

```bash
git add crates/decode/tests/
git commit -m "test(decode): real fixture-based decode/orientation/resize coverage"
git push
```

---

## Task 11: Lint, format, MSRV, and dependency audit

**Files:**
- Modify: `.github/workflows/ci.yml` (pin toolchain ≥1.93)

- [ ] **Step 1: Pin the CI toolchain to ≥1.93**

In `.github/workflows/ci.yml`, replace each `dtolnay/rust-toolchain@stable` with a pinned version that satisfies the zen* MSRV (1.93):

```yaml
      - uses: dtolnay/rust-toolchain@1.93.0
        with:
          components: clippy, rustfmt   # (only on the lint job)
```

(Apply the `components` line only to the `lint` job; the `build-test` job uses the bare toolchain.)

- [ ] **Step 2: Format + clippy locally**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean. Fix any warnings (common: unused imports left from the `image` removal).

- [ ] **Step 3: Confirm no image-rs and no C deps slipped in**

Run: `cargo tree -e features --workspace | grep -iE "imagequant|lcms2|libheif|dav1d|cc " || echo "no C deps"`
Expected: "no C deps" (or only expected build-only entries). Also re-run the Task 9 Step 5 image-rs absence check.

- [ ] **Step 4: Verify CI locally with act (per project memory)**

Run: `act pull_request -j lint -j build-test 2>&1 | tail -30` (Linux legs)
Expected: green, matching CI. (Per memory `verify-ci-with-act-before-push`.)

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: pin toolchain to 1.93 for zen* MSRV"
git push
```

---

## Task 12: Cold-start benchmark (gating — speed is the goal)

**Files:**
- Create: `crates/decode/benches/cold_start.rs` (or a `scripts/bench-coldstart.sh` timing the real binary)

> The migration's primary motivation is faster cold start via parallel decode. This task MEASURES it. It is gating: if there is no win on a large restart-marker JPEG on x86_64, investigate before declaring Phase 1 done.

- [ ] **Step 1: Add a decode-time microbenchmark**

Create `crates/decode/benches/cold_start.rs` timing `display_image` on a large image (provide a real ~12 MP JPEG with DRI restart markers via an env var path so it isn't committed):

```rust
// Run: PVC_BENCH_JPEG=/path/to/large.jpg cargo bench -p decode --bench cold_start
// Reports wall-clock for a single cold display_image() at the default full cap.
use std::time::Instant;

fn main() {
    let Some(path) = std::env::var_os("PVC_BENCH_JPEG") else {
        eprintln!("set PVC_BENCH_JPEG to a large JPEG path"); return;
    };
    let p = std::path::PathBuf::from(path);
    // warm file cache
    let _ = std::fs::read(&p).unwrap();
    let runs = 5;
    let mut best = f64::MAX;
    for _ in 0..runs {
        let t = Instant::now();
        let buf = decode::display_image(&p, 16384).unwrap();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        std::hint::black_box(&buf);
        best = best.min(ms);
    }
    println!("display_image best of {runs}: {best:.1} ms");
}
```

Add to `crates/decode/Cargo.toml`:

```toml
[[bench]]
name = "cold_start"
harness = false
```

- [ ] **Step 2: Measure new vs old on real hardware (x86_64 and arm64)**

Run: `PVC_BENCH_JPEG=<large.jpg> cargo bench -p decode --bench cold_start` on the zen branch, and compare against the same on `main` (pre-migration). Record both numbers, the CPU, and whether the JPEG has restart markers (`exiftool -RestartInterval <file>` or `djpeg -verbose`).

- [ ] **Step 3: Record results in the PR**

Write the before/after ms (x86_64 and, if available, arm64) into the Phase 1 PR description. Expectation: meaningful x86_64 speedup with restart markers; smaller arm64 gain (zenjpeg ARM is scalar today). No gain → confirm the test JPEG actually has restart markers before concluding.

- [ ] **Step 4: Commit**

```bash
git add crates/decode/benches/cold_start.rs crates/decode/Cargo.toml
git commit -m "bench(decode): cold-start time-to-first-decode harness"
git push
```

---

## Task 13: Licensing + documentation

**Files:**
- Create: `LICENSE`
- Modify: `Cargo.toml` (workspace `license`)
- Modify: `README.md`, `AGENTS.md`

- [ ] **Step 1: Add the AGPL-3.0 LICENSE file**

Run: download the canonical text into `LICENSE`:

```bash
curl -sSL https://www.gnu.org/licenses/agpl-3.0.txt -o LICENSE
head -1 LICENSE   # expect: "GNU AFFERO GENERAL PUBLIC LICENSE"
```

- [ ] **Step 2: Declare the license in the workspace manifest**

In `Cargo.toml` `[workspace.package]`, add:

```toml
license = "AGPL-3.0-only"
```

And ensure each member crate uses `license.workspace = true` (add the line to any `[package]` that sets other workspace fields).

- [ ] **Step 3: Update README.md**

Replace the "Image decoding and processing | imazen/zen* family of modules" framing with the concrete crate list (zencodec, zenpixels, zenresize, zenjpeg, zenpng, zenwebp, zengif; HEIC/AVIF noted as Phase 2) and add a one-line AGPL-3.0 note. Remove any remaining image-rs references.

- [ ] **Step 4: Correct AGENTS.md**

Update the tech-stack line to name the zen* stack. **Correct line 55** (the tag-writing claim): change "Add, remove and change basic tags in JPEG, PNG, WebP, GIF, HEIC and AVIF images …" to note that Windows-indexer-searchable tag writing is realistically **JPEG-only** (the metadata workstream, not this migration). Keep the `tags.txt` master-list behavior as-is.

- [ ] **Step 5: Commit**

```bash
git add LICENSE Cargo.toml README.md AGENTS.md
git commit -m "docs: AGPL-3.0 LICENSE + workspace license; README/AGENTS reflect zen* stack"
git push
```

---

## Task 14: Manual verification + open the PR

- [ ] **Step 1: Run the app on a real folder**

Run: `cargo run -p app --release -- <path-to-a-photo>` then exercise: arrow-key navigation, R/E rotation (must be snappy, lossless), Z zoom cycle, an EXIF-rotated phone photo (must display upright), a CMYK/odd JPEG and a PNG with alpha (must render correctly). Confirm a broken/non-image file shows "Can't display …" and the app stays alive.

- [ ] **Step 2: Full green gate**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all clean/green.

- [ ] **Step 3: Open the PR**

```bash
gh pr create --title "Phase 1: migrate decoding to imazen zen* (remove image + fast_image_resize)" \
  --body "$(cat <<'EOF'
Implements docs/superpowers/specs/2026-05-30-zen-codec-decode-migration-design.md (Phase 1).

- Removes `image` and `fast_image_resize` entirely.
- Decode JPEG/PNG/WebP/GIF via zen* (zencodec dispatch); zenresize for downscale + EXIF orient; rotation via zenresize OrientOutput.
- Buffer type: zenpixels::PixelBuffer threaded through cache/prefetch/rotation.
- parallel/rayon decode ON (cold-start win — see benchmark below).
- AGPL-3.0 license added.

Cold-start benchmark (x86_64 / arm64): <fill from Task 12>

Phase 2 (HEIC/AVIF) is a separate plan.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Expected: CI goes green on all three OS legs.

---

## Self-Review Notes

- **Spec coverage:** decode swap (T5/6), buffer type (T9), resize (T4), rotation+flip via zenresize (T3/8), EXIF orient fused (T6), display_dimensions probe (T7), remove image+fast_image_resize (T9), parallel/rayon ON (T1 features), DecodeError (T2), fixtures+tests (T10), no-upscale guard (T4), benchmark (T12), licensing+docs (T13), MSRV/CI (T11), `cargo deny`/tree audit (T9/T11). All spec sections map to a task.
- **Prerelease-API caveat:** Task 1 records the real signatures; every decoder/PixelBuffer/zenresize call site says "reconcile with Task 1." This is deliberate, not a placeholder — the authoring environment was offline so the exact decoder entry points could not be compile-verified.
- **Type consistency:** `RawImage{bytes,w,h,channels}` (T5) → `resize_and_orient`/`resize_to_fit` (T4/6) → `PixelBuffer` (T6) → `Arc<PixelBuffer>` in main.rs (T9) → `rotate_buffer` (T8) → `as_contiguous_bytes` → Slint (T9). Consistent throughout.
- **Phase 2** (HEIC/AVIF) intentionally deferred to its own plan, written after Phase 1 confirms the zen API on real hardware.
