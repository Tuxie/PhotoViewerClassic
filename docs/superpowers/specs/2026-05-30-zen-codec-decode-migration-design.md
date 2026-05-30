# Zen Codec + Processing Migration — Design

**Status:** Approved — not yet implemented
**Date:** 2026-05-30
**Scope:** Go **all-in on the imazen `zen*` ecosystem**: replace image-rs (`image`) **and**
`fast_image_resize` **entirely** — decoding, the pixel-buffer type, downscaling, and
rotation/flip — using imazen modules. Tag/metadata writing remains out of scope (separate
workstream).

## Context

PhotoViewerClassic (PVC) currently decodes with `image` 0.25 (features `jpeg/png/gif/webp`),
downscales with `fast_image_resize` (SIMD Lanczos3), rotates with `image::imageops`, threads
`image::RgbaImage` through its cache/prefetch/rotation, and reads EXIF orientation with
`kamadak-exif`. The original request was explicit: use "the imazen family of codecs **and image
processing modules** instead of image-rs." This spec takes that literally — **`image` and
`fast_image_resize` are removed completely**, replaced by imazen modules designed to
interoperate.

Verification (2026-05-30, two adversarial workflow passes against primary sources, plus direct
inspection of the unpacked `zenresize-0.3.1` crate source) confirmed the full replacement is
feasible. The decode crate (`crates/decode/src/lib.rs`) is the chokepoint; the buffer *type* it
emits changes from `image::RgbaImage` to `zenpixels::PixelBuffer`, which ripples into
`app/src/main.rs` as a mechanical type change. **Rotation and flip — including all 8 EXIF
orientations — are handled by `zenresize`, not by any hand-rolled code or a Slint transform.**

## Licensing (decided)

The decoders and `zenresize` are dual-licensed **`AGPL-3.0-only OR LicenseRef-Imazen-Commercial`**
(`zenjpeg`, `zenpng`, `zenwebp`, `zenavif`, `heic`, `zenresize`). The shared trait/buffer crates
`zencodec`, `zenpixels` and `zengif` are permissive (Apache-2.0/MIT), but linking any AGPL crate
makes the whole binary AGPL-3.0.

**Decision (Per, 2026-05-30):** PVC ships under **AGPL-3.0**. No commercial Imazen license
needed. See user memory `pvc-agpl-license-decision.md`. The §13 network clause is dormant
(PVC is not a network service); ordinary GPLv3 copyleft applies on distribution. A HEVC/HEIF
patent notice in `heic`'s README is flagged for Phase 2.

## Goals

- **Remove `image` and `fast_image_resize` entirely.** No image-rs residue anywhere in the
  workspace. Decode, buffer type, downscale, and rotate/flip all handled by imazen modules —
  **no hand-rolled pixel code**. This is the headline goal.
- Decode JPEG, PNG, WebP, GIF (Phase 1) and HEIC/HEIF + AVIF (Phase 2) via `zen*`/`heic`.
- **Fast cold start via parallel decode** — the primary *performance* motivation. Enable the
  `zen*` single-image parallel decode (`parallel`/rayon) so the first image decodes across all
  cores. Cold-start time-to-first-display is the most important benchmark for this app.
- Preserve PVC's *application-level* concurrency model: **single foreground decode worker +
  single prefetch worker; no polling.** rayon operates *inside* a single decode (intra-decode
  parallelism), orthogonal to — and complementary with — the one-decode-at-a-time worker
  model. At cold start only the foreground decode runs, so it gets every core. (The macOS
  blowup that motivated the single-worker model was thread-*per-navigation-keypress* spawning
  many whole decodes at once — a different axis the worker model still prevents; rayon does
  not re-introduce it.)
- **Keep PVC's current rotation architecture** — cache the rotation-0 base, re-derive the
  displayed (rotated) buffer in `rotate_turns` — swapping only the *engine* (`image::imageops`
  → `zenresize OrientOutput`). No Slint transform, no `ViewState` geometry rework.
- Keep the **decode crate as the single decode/processing chokepoint.** Buffer type changes
  (`RgbaImage` → `PixelBuffer`); this ripple into `main.rs` is mechanical and well-bounded —
  the accepted cost of going all-in.
- Remain pure-Rust with **no C toolchain** on the default build path. (rayon and `zenresize`
  are pure-Rust; they do not pull a C toolchain.)
- Keep the codebase honest: real fixture-based decode tests; updated docs.

## Non-goals

- **Tag/keyword writing.** The `zen*` codecs are decode-only; writing needs separate
  permissive crates (`img-parts` + `little_exif`) and is realistically JPEG-only for the
  Windows indexer. Tracked separately as the "Plan 3 metadata" workstream.
- **Adopting `zenpipe`.** It's a streaming pull-DAG for memory-bounded server/batch pipelines,
  is git-only/unpublished (would force a git dep, hurting reproducible/offline builds and AGPL
  hygiene), and would fight PVC's bespoke prefetch/cache/worker model. The decode → orient →
  downscale → Slint flow is glued with plain function calls over `Arc<PixelBuffer>`.
- **Slint display-side rotation.** Rejected: `zenresize` already rotates losslessly in the pass
  we run anyway, so a GPU transform buys nothing while adding unverified-femtovg risk, a
  `ViewState` fit/zoom/pan rework for the rotated frame, and the footgun that a transform on the
  wrong element rotates the whole GUI. Pixels carry the rotation; the view layer stays
  axis-aligned and unchanged.
- **A hand-rolled `orient.rs`.** Not needed — `zenresize OrientOutput` covers all 8 EXIF
  orientations *including mirrors*, so there is no rotate/flip gap to fill.
- **Keeping an image-rs decode fallback.** Clean cut; correctness is guarded by fixture-based
  golden tests, not a dual decode path.
- **Byte-identical output vs the old stack.** `zenresize` resamples in linear light with
  premultiplied alpha (gamma/alpha-correct), unlike `fast_image_resize`'s defaults — output is
  a quality *change* (likely improvement), not a pixel-identical swap. Accepted; golden tests
  are regenerated against the new output.

## Architecture

The decode crate is rewritten internally; its buffer type becomes `zenpixels::PixelBuffer`.
EXIF orientation is applied **in the same `zenresize` pass as the downscale**:

```
display_image(path, max) -> Result<PixelBuffer, DecodeError>
    bytes  = fs::read(path)
    fmt    = zencodec::ImageFormatRegistry::detect(&bytes)   // magic bytes, not extension
    buf    = dispatch(fmt, &bytes)                            // per-crate zen decoder -> PixelBuffer (RGB8 if no alpha, else RGBA8)
    ori    = orient_output(read_orientation(&bytes))         // kamadak-exif tag 1..8 -> zenresize OrientOutput
    buf    = zenresize: downscale-to-fit(max) + .with_orientation(ori)   // ONE pass: resize + EXIF rotate/flip
    -> PixelBuffer   // rotation-0 BASE (EXIF already normalized), at the display cap
```

**Buffer type: `zenpixels::PixelBuffer`** (owned `Vec<u8>`, Send+Sync → `Arc`-able, supports
RGB8 and RGBA8 so the deferred-RGBA-for-RGB memory win is preserved). Native output of the zen
decoders and accepted by `zenresize` (which works on the contiguous bytes), so the seam is
zero-conversion. `app/src/main.rs` threads `Arc<zenpixels::PixelBuffer>` through
`Cached`/`Cache`/`ShowSource`/`obtain_base`/both workers; the final Slint copy uses
`PixelBuffer::as_contiguous_bytes()` (+ `copy_to_contiguous_bytes()` fallback) + `width()`/
`height()` into the unchanged `SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice` call.

### Rotation (engine swap only — architecture unchanged)

`zenresize` exposes `OrientOutput` (verified verbatim from `zenresize-0.3.1/src/streaming.rs`):
`None, Rotate90, Rotate180, Rotate270, FlipHorizontal, FlipVertical, Transpose, Transverse` —
**all 8 EXIF orientations, mirrors included** — applied via
`StreamingResize::with_orientation(OrientOutput)`. Dimension swaps on
Rotate90/270/Transpose/Transverse are handled internally.

- **EXIF orientation** maps the kamadak-read tag (1..8) to one `OrientOutput` and is applied in
  the decode pass above (1=None, 2=FlipHorizontal, 3=Rotate180, 4=FlipVertical, 5=Transpose,
  6=Rotate90, 7=Transverse, 8=Rotate270). No manual rot+flip composition.
- **Interactive R/E view rotation** keeps today's design exactly: the cache holds the
  rotation-0 BASE; `rotate_turns(base, turns)` re-derives the displayed buffer from the base
  each time (so rotations never compound). The only change is the *engine*: instead of
  `image::imageops::rotate90/180/270`, run `zenresize` at **identity scale** (out dims = in
  dims) with `OrientOutput::Rotate90/180/270` for `turns` 1/2/3 (turn 0 shares the base `Arc`,
  as today). Lossless for 90° multiples; cheap (operates on the already-downscaled base).

**MUST-VERIFY (LOW, clear fallback):** that `zenresize` at identity scale + `OrientOutput`
produces a clean lossless rotation (a 90° turn is a pure pixel move; the identity resample
should be a no-op, but confirm the linear-light/premult round-trip doesn't perturb bytes). If
it does, drive the rotation via the orientation path without a resample, or accept the
negligible perturbation (it's a view transform on an already-resampled buffer). This does not
affect EXIF orientation (which is fused with a real downscale anyway).

### Components

1. **Format dispatch** — `fn detect_and_decode(bytes) -> Result<PixelBuffer, DecodeError>`.
   `zencodec::ImageFormatRegistry::detect()` for magic-byte format ID, then a small explicit
   `match` to the per-crate decoder (the one-call `zencodecs` dispatch crate is unpublished).
   Phase 1 arms: JPEG/PNG/WebP/GIF. Phase 2: HEIC/HEIF + AVIF. Unknown → `DecodeError::Unsupported`.

2. **Resize + orient via `zenresize`** — replaces `fast_image_resize` **and** `image::imageops`.
   `Filter::Lanczos` (direct Lanczos3-family match), `FitMode::Within(max,max)`,
   `.with_orientation(OrientOutput)`; one pass does downscale + EXIF rotate/flip.
   **Keep PVC's existing `fit_dims` no-upscale guard** (`FitMode::Within` does *not* prevent
   upscaling). Runs on RGB8 (deferred path) or RGBA8 via the matching `PixelDescriptor`.

3. **Error type `DecodeError`** — small enum (`Io`, `Unsupported`, `Decode(String)`) with
   `Display`. Replaces `image::ImageResult`. `main.rs`'s existing `"Can't display {file}: {e}"`
   path consumes it unchanged; HEIC partial-conformance failures surface as `Decode(_)`.

4. **Header-only dimensions** — `display_dimensions` switches from `image::image_dimensions`
   (extension-based) to zen magic-byte probes (`zencodec`/`heic` `ImageInfo::from_bytes`,
   `zenwebp::ImageInfo`); keeps the orientation-driven W/H swap. Works for extension-less and
   HEIC/AVIF files.

**Unchanged code/architecture:** the cache/prefetch worker model; `rotate_turns`'s
base→display re-derivation contract (only its engine changes); the GPU-texture-limit clamp; the
Slint `SharedPixelBuffer` handoff; `ViewState` and `main.slint` (no rotation transform, no
geometry rework). EXIF orientation *reading* stays on `kamadak-exif`.

## Dependencies & feature flags

`crates/decode/Cargo.toml` — **add:**

| Crate | Phase | Features ON | Features OFF / avoided |
|---|---|---|---|
| `zencodec` | 1 | (default traits + `ImageFormatRegistry`) | — |
| `zenpixels` | 1 | `rgb` (for `to_rgba8`/contiguous bytes) | — |
| `zenresize` | 1 | (default; `Filter::Lanczos` + `OrientOutput` at call site) | `avx512` unless benchmarked |
| `zenjpeg` | 1 | `decoder`, `zencodec`, **`parallel`** | encoder-side (`trellis`, `ultrahdr`), `lcms2`(C) |
| `zenpng` | 1 | decode + `zencodec` (+ `parallel` if exposed) | encode, `imagequant`(C) |
| `zenwebp` | 1 | decode + `zencodec` (+ `parallel` if exposed) | encode |
| `zengif` | 1 | decode + `zencodec` | encode |
| `heic` | 2 | `backend-rust`, **`parallel`** | native backends (`backend-mediafoundation` etc.) |
| `zenavif` (standalone `.avif`) | 2 | decode + `zencodec` | `unsafe-asm`, rav1d-safe `asm`/`partial_asm`/`c-ffi` |

**Remove:** `image`, `fast_image_resize` from `crates/decode/Cargo.toml`, `app/Cargo.toml`,
and the workspace `Cargo.toml`. In `app/src/main.rs` delete the `image` dep and its comment;
`Arc<image::RgbaImage>` → `Arc<zenpixels::PixelBuffer>`; `rotate_turns` swaps
`image::imageops` for `zenresize OrientOutput`.

**Kept (non-image-rs):** `kamadak-exif` (EXIF orientation read), `slint`, `glow`,
`i-slint-backend-winit`.

AVIF decoder choice: use the **dedicated `zenavif`** for standalone `.avif` (both `zenavif`
and `heic`'s `av1` feature route AV1 through `rav1d-safe`; `heic av1` targets AV1-in-HEIF).
Confirm against docs.rs at implementation time; `heic av1` is the fallback.

- **rayon is ON** via each decoder's `parallel` feature — the performance motivation (parallel
  single-image decode for fast cold start). Intra-decode parallelism beneath the unchanged
  single-foreground + single-prefetch worker model. rayon's bounded global pool work-steals
  (no thread-per-decode), so it cannot recreate the old blowup. Both workers share the pool; a
  later refinement may scope/cap it if prefetch steals cores from a foreground decode — not a
  launch concern.
- **Exact-pinned versions** (e.g. `zenjpeg = "=0.8.3"`, `heic = "=0.1.6"`,
  `zenpixels = "=0.2.11"` [0.3.0 is yanked], `zenresize = "=0.3.1"`), `Cargo.lock` committed.
  Pre-1.0 + yank history → no surprise minor bumps.
- **No C toolchain** on the default path (avoid `imagequant`, `lcms2`, rav1d-safe asm/ffi).
  `zenresize` is pure-Rust (archmage SIMD: AVX2+FMA/NEON/WASM, scalar fallback); rayon is
  pure-Rust. (`fast_image_resize`/`image` appear only as `zenresize`'s *dev*-deps for its own
  benchmarks — not runtime deps of consumers.)
- **MSRV floor rises to 1.93** (zenjpeg *and* zenresize; zenpixels 1.85 is looser). CI must pin
  ≥1.93; README already states Rust 1.96+. Verify with `act` (memory `verify-ci-with-act-before-push`).
- **Encoder footprint:** where a decoder has no decode-only switch, rely on
  `default-features = false` + the workspace's `lto = true, codegen-units = 1` to
  dead-code-eliminate encoder paths (best-effort, not a feature contract).

After wiring, audit with `cargo tree -e features` and `cargo deny` to confirm no transitive
dep re-introduces a C or non-AGPL-compatible dependency, and that **no `image`/
`fast_image_resize` remains** anywhere in the lock file.

## Integration must-verifies (LOW severity, clear fallbacks)

- **Identity-scale `zenresize` + `OrientOutput` is a clean lossless rotation** for the view-rotate
  path (see Rotation) — else drive orientation without a resample, or accept negligible drift.
- Exact `zenresize` oriented-output API: `StreamingResize::with_orientation(OrientOutput)` vs a
  one-shot helper — confirm against docs.rs; both are a single pass.
- `PixelBuffer::from_vec` arg order is `(data, w, h, descriptor)` and needs a `PixelDescriptor`
  (`RGBA8_SRGB` / `RGB8_SRGB`) — confirm at the call site.
- Construct `PixelBuffer` with a **tight stride** so `as_contiguous_bytes()` returns `Some` for
  the Slint copy — else use `copy_to_contiguous_bytes()` (always works).
- `PixelBuffer` exposes `width()`/`height()` (no single `dimensions()` accessor); `ResizeConfig`
  exposes the output dims (confirm accessor name).

## Phasing

### Phase 1 — engine + processing swap, parity (JPEG/PNG/WebP/GIF)
Replace decode (4 formats), buffer type, resize, and rotate/flip; **remove `image` +
`fast_image_resize`**; fold EXIF orientation into the decode `zenresize` pass; swap
`rotate_turns`'s engine to `zenresize OrientOutput`; swap `main.rs` to `Arc<PixelBuffer>`.
`parallel`/rayon ON. `imageset::SUPPORTED` unchanged. **No `orient.rs`, no Slint/ViewState
changes.** **Done when:** ported rotation/resize tests pass, new fixture-based decode tests
pass, the workspace builds with zero `image`/`fast_image_resize` deps, the full suite is green,
the app shows images correctly incl. R/E rotation and EXIF-oriented photos (manual check),
**and the cold-start benchmark below confirms the parallel-decode win.**

**Cold-start benchmark (gating for Phase 1):** time-to-first-display for a large camera JPEG
(and a large PNG), new path vs the old image-rs path, on **x86_64 and arm64** (Apple Silicon).
Record in the plan/PR. Expect a meaningful x86_64 speedup for JPEGs with DRI restart markers;
arm64 may show a smaller (multi-core-only, scalar-per-core) gain until zenjpeg NEON lands. No
gain on a large JPEG → verify it has restart markers (the parallel path needs them).

### Phase 2 — new formats (HEIC/HEIF + AVIF)
Add `heic` (`backend-rust`, `parallel`) and AVIF (`zenavif`). Extend `imageset::SUPPORTED`
with `heic`, `heif`, `avif`. Port `display_dimensions` probes. **Done when:** HEIC/AVIF
display; undecodable HEICs (heic decodes ~118/162 conformance files) degrade gracefully via the
existing error path; fixture tests cover a real HEIC and AVIF. Add the AGPL/HEVC patent notice.

## Testing

- **Fixtures (decided):** commit one tiny real sample per format under
  `crates/decode/tests/fixtures/` (`*.jpg/.png/.webp/.gif`; Phase 2 adds `*.heic/.avif`).
  Tests assert zen decodes them to known dimensions and sample pixels. Tests real decoders
  against real files and provides the HEIC/AVIF fixtures Phase 2 needs. (Removing `image`'s
  codec features means the old `RgbaImage::save(&png)` test inputs no longer compile.)
- **Orientation tests:** assert each EXIF tag 1–8 maps to the correct `OrientOutput`, and that a
  decoded fixture with a known orientation comes out correctly oriented (dims swap on
  90/270/transpose/transverse) — using a small real EXIF-tagged JPEG fixture.
- **Rotation tests:** `rotate_turns` over the four turn counts on a 2×1 fixture (dims swap on
  1/3), matching the current behavior, against the `zenresize` engine.
- **Resize tests:** rewritten against `zenresize` output — **golden values regenerated**
  (linear-light/alpha-correct resampling is intentionally not byte-identical). Assert
  fits-cap + no-upscale + RGB-deferred path matches RGBA dims.
- **Coverage to add:** unsupported-format → `DecodeError`; graceful failure on a truncated
  file; `display_dimensions` header-probe per format.
- **No sleeping/polling** in any test (PVC dev rule).

## Error handling

- IO/unsupported/decode errors collapse into `DecodeError`, reach the worker, which renders
  `"Can't display {file}: {e}"` and continues (no crash). HEIC partial conformance is a
  `Decode(_)`.
- Truncated/corrupt files must return `Err`, never panic. Set conservative decoder `Limits`
  (max pixels/memory) so a malformed file can't OOM the process.

## Housekeeping

- Add `LICENSE` (AGPL-3.0) + `license = "AGPL-3.0-only"` to the workspace `Cargo.toml`.
- Update `README.md`: concrete imazen crate list + AGPL note; remove the image-rs framing.
- Update `AGENTS.md`: tech-stack/formats line; **correct line 55** (Windows-indexer tag
  writing is realistically JPEG-only — for the future tag workstream; the doc is wrong today).
- Push after each commit (PVC dev rule).

## Risks & mitigations

- **`zenresize` quality is a CHANGE, not a byte-identical swap (LOW):** linear-light +
  premultiplied-alpha; default filter Robidoux. → Select `Filter::Lanczos`; spot-check
  downscales; regenerate golden values; accept non-identical (likely better) output.
- **Identity-scale rotation cleanliness (LOW):** see Rotation must-verify; fallback is benign.
- **Ecosystem maturity / pre-1.0 churn (MEDIUM):** all zen* are 0.x, single-vendor, prerelease
  decoder API, yank history (zenpixels 0.3.0 yanked; zenresize first published 2026-03).
  → Exact-pin versions, commit `Cargo.lock`, fixture/golden tests, verify APIs against docs.rs
  at coding time.
- **HEIC conformance (HIGH, Phase 2):** ~118/162 HEIF files decode; not bit-exact (fine for a
  viewer). → Graceful per-image error path (already present); ship HEIC best-effort.
- **Parallel-decode win is conditional — verify, don't assume (MEDIUM):** zenjpeg `parallel`
  engages only with DRI restart markers + ≥1024 MCU; progressive JPEGs get nothing; ARM is
  scalar today (NEON not yet implemented). HEIC tile-parallel (~2.5×) is less conditional.
  → Measure on real hardware (the Phase-1 benchmark); doesn't change the decision (rayon ON).
- **Dependency-graph growth (LOW):** `zenresize` pulls ~10 transitive pure-Rust crates
  (archmage, magetypes, linear-srgb, zenpixels, zenblend, imgref, rgb, libm, whereat,
  safe_unaligned_simd) vs `fast_image_resize`'s tighter graph — all pure Rust, no C/sys deps.

## Open questions

None blocking. Verify at integration (all LOW–MEDIUM impact): identity-scale orient cleanliness;
exact oriented-output API (`with_orientation` vs one-shot); `from_vec` arg order; pin `zenpixels`
0.2.11 vs re-check at `cargo add`; tight-stride constructor so `as_contiguous_bytes()` is `Some`.
*This document is an engineering assessment, not legal advice; the AGPL-static-linking conclusion
is well-supported but Per may confirm with counsel.*
