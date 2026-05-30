# Zen Codec Decode Migration — Design

**Status:** Approved — not yet implemented
**Date:** 2026-05-30
**Scope:** Replace image-rs *decoding* with the imazen `zen*` family + `heic`. Decode only — tag/metadata writing is explicitly out of scope (separate workstream).

## Context

PhotoViewerClassic (PVC) currently decodes images with `image` 0.25 (features
`jpeg/png/gif/webp`), downscales with `fast_image_resize`, and reads EXIF orientation
with `kamadak-exif` — an all-permissive (MIT/Apache) stack. `README.md` already states
the *intent* to use the "imazen/zen\* family of modules"; this spec makes that concrete.

All decoding is isolated in **`crates/decode/src/lib.rs`** behind a small public seam
(`display_image`, `display_dimensions`, `read_orientation`, `apply_orientation`,
`downscale_to_fit`). `app/src/main.rs` threads **`image::RgbaImage`** (as
`Arc<image::RgbaImage>`) through its cache, prefetch worker, and `rotate_turns` (which
uses `image::imageops::rotate90/180/270`), finally copying into a Slint
`SharedPixelBuffer<Rgba8Pixel>` in `push_frame`. The decode crate is therefore a clean,
single chokepoint, and the migration is well contained.

Verification (2026-05-30, adversarially fact-checked against primary sources) confirmed:
the `zen*`/`heic` crates exist on crates.io, decode all six target formats **pure-Rust
with no C toolchain** on Windows/macOS/Linux, and are **decode-only** (heic) or have
disableable encoders. They are pre-1.0 and single-vendor (Imazen / Lilith River), with a
self-described *prerelease* decoder API — so maturity, not capability, is the dominant
technical risk.

## Licensing (decided)

The real decoders are dual-licensed **`AGPL-3.0-only OR LicenseRef-Imazen-Commercial`**
(`zenjpeg`, `zenpng`, `zenwebp`, `zenavif`, `heic`). The shared trait/buffer crates
(`zencodec`, `zenpixels`) and `zengif` are permissive (Apache-2.0/MIT), but they carry no
decode logic for the formats that matter. Statically linking any AGPL decoder makes the
whole distributed PVC binary AGPL-3.0.

**Decision (Per, 2026-05-30):** PVC will ship under **AGPL-3.0**. No commercial Imazen
license is needed. See user memory `pvc-agpl-license-decision.md`. The §13 network clause
is dormant (PVC is not a network service), but ordinary GPLv3 copyleft applies on
distribution. A patent notice for HEVC/HEIF exists in `heic`'s README — relevant to binary
distribution in some jurisdictions, flagged for Phase 2.

## Goals

- Decode JPEG, PNG, WebP, GIF (Phase 1) and HEIC/HEIF + AVIF (Phase 2) via `zen*`/`heic`.
- Keep `crates/decode`'s public API and behavior identical, so **`app/src/main.rs` does
  not change**.
- **Fast cold start via parallel decode** — this is the primary motivation for the
  migration. Enable the `zen*` single-image parallel decode (`parallel`/rayon) so the
  first image decodes across all cores. Cold-start time-to-first-display is the most
  important benchmark for this app.
- Preserve PVC's *application-level* concurrency model: **single foreground decode worker +
  single prefetch worker; no polling.** rayon operates *inside* a single decode
  (intra-decode parallelism), which is orthogonal to — and complementary with — the
  one-decode-at-a-time worker model. At cold start only the foreground decode runs, so it
  gets every core. (The macOS blowup that motivated the single-worker model was
  thread-*per-navigation-keypress* spawning many whole decodes at once — a different axis
  that the worker model still prevents; rayon does not re-introduce it.)
- Remain pure-Rust with **no C toolchain** on the default build path. (rayon is pure-Rust;
  it does not pull a C toolchain.)
- Keep the codebase honest: real fixture-based decode tests; updated docs.

## Non-goals

- **Tag/keyword writing** (the original request's "tag writing" half). The `zen*` codecs
  are decode-only; writing needs separate permissive crates (`img-parts` + `little_exif`)
  and is realistically JPEG-only for the Windows indexer. Tracked separately as the
  "Plan 3 metadata" workstream; this spec does not touch it.
- *(Decode speed is a primary GOAL, not a non-goal — see Goals. `parallel`/rayon is ON.)*
- **Replacing `fast_image_resize`, `kamadak-exif`, or `image::imageops` rotation.** They
  stay.
- **Removing `image` entirely.** It is retained as the buffer/rotation library (codec
  features off). Dropping it would mean reimplementing rotation on raw `Vec<u8>` and
  re-typing every `Arc<image::RgbaImage>` — more risk, no benefit now.
- **Keeping an image-rs decode fallback.** Clean cut; correctness is guarded by
  fixture-based golden tests, not a dual decode path.

## Architecture

The decode crate is rewritten internally; its public surface is unchanged except for the
error type:

```
display_image(path, max) -> Result<RgbaImage, DecodeError>
    bytes  = fs::read(path)
    fmt    = zencodec::ImageFormatRegistry::detect(&bytes)   // magic bytes, not extension
    pixels = dispatch(fmt, &bytes)                            // per-crate zen decoder -> zenpixels::PixelBuffer
    orient = read_orientation(&bytes)                         // kamadak-exif, unchanged
    -> if no-alpha: PixelBuffer->RgbImage  -> apply_orientation -> downscale_rgb_to_fit -> into_rgba8()
       else:        PixelBuffer->RgbaImage -> apply_orientation -> downscale_to_fit
```

**Buffer type is unchanged: `image::RgbaImage`.** A thin shim converts the zen
`PixelBuffer` (RGBA8 / RGB8 via `to_rgba8()` + `as_contiguous_bytes()`) into
`image::RgbaImage::from_raw(w, h, bytes)` (or `RgbImage` on the no-alpha path) at the
bottom of `display_image`. Everything downstream in `main.rs` — cache (`Cached`/`Cache`),
both workers, `rotate_turns`, `clamp_to_texture_limit`, the Slint copy — is untouched.

### Components

1. **Format dispatch** — `fn detect_and_decode(bytes: &[u8]) -> Result<PixelBuffer, DecodeError>`.
   Uses `zencodec::ImageFormatRegistry::detect()` for magic-byte format ID, then matches to
   the per-crate decoder. (The one-call `zencodecs` plural dispatch crate is unpublished, so
   we dispatch ourselves — a small, explicit `match`.) Phase 1 arms: JPEG/PNG/WebP/GIF.
   Phase 2 arms: HEIC/HEIF + AVIF. An unknown/unsupported format returns
   `DecodeError::Unsupported`.

2. **Pixel shim** — `PixelBuffer -> image::RgbImage | RgbaImage`. Requests RGB8 layout when
   the format/image has no alpha (to keep the deferred-downscale memory win), RGBA8
   otherwise. Conversion is a `from_raw` over the tightly-packed contiguous bytes.

3. **Error type** — `DecodeError`: a small enum (`Io`, `Unsupported`, `Decode(String)`)
   with `Display`. Replaces `image::ImageResult` at the crate boundary. `main.rs`'s existing
   `"Can't display {file}: {e}"` path consumes it unchanged; HEIC partial-conformance
   failures surface here as `Decode(_)` and degrade gracefully (the worker already shows the
   message and continues).

4. **Header-only dimensions** — `display_dimensions` switches from `image::image_dimensions`
   (extension-based) to zen magic-byte probes (`zencodec`/`heic` `ImageInfo::from_bytes`,
   `zenwebp::ImageInfo`). Keeps the existing orientation-driven W/H swap. This is a strict
   improvement: it works for extension-less and HEIC/AVIF files.

**Unchanged code:** `read_orientation`, `apply_orientation` (the tested 1–8 transform
table), `fit_dims`, `downscale_to_fit`, `downscale_rgb_to_fit`, `resize_u8x4`,
`resize_u8x3`. `app/src/main.rs` and the other crates (`viewstate`, `config`, `imageset`
except its `SUPPORTED` list in Phase 2) are unchanged.

## Dependencies & feature flags

The table below lists the crates added to `crates/decode/Cargo.toml` and the feature
flags to enable/disable. Phase 1 crates first, Phase 2 after.

| Crate | Phase | Features ON | Features OFF / avoided |
|---|---|---|---|
| `zencodec` | 1 | (default traits + `ImageFormatRegistry`) | — |
| `zenpixels` | 1 | `rgb` (for `to_rgba8`) | — |
| `zenjpeg` | 1 | `decoder`, `zencodec`, **`parallel`** | encoder-side (`trellis`, `ultrahdr`), `lcms2`(C) |
| `zenpng` | 1 | decode + `zencodec` (+ `parallel` if exposed) | encode, `imagequant`(C) |
| `zenwebp` | 1 | decode + `zencodec` (+ `parallel` if exposed) | encode |
| `zengif` | 1 | decode + `zencodec` | encode |
| `heic` | 2 | `backend-rust`, **`parallel`** | native backends (`backend-mediafoundation` etc.) |
| `zenavif` (standalone `.avif`) | 2 | decode + `zencodec` | `unsafe-asm`, rav1d-safe `asm`/`partial_asm`/`c-ffi` |

AVIF decoder choice: use the **dedicated `zenavif`** crate for standalone `.avif` files
(both `zenavif` and `heic`'s `av1` feature route AV1 through `rav1d-safe`; `heic av1`
specifically targets AV1-in-HEIF containers). Confirm against docs.rs at implementation
time which cleanly handles bare `.avif`; `heic av1` is the fallback if `zenavif` does not.

Workspace `Cargo.toml`: the `image` dependency becomes `default-features = false` with
**no codec features** (retained only for `RgbaImage`/`RgbImage`/`imageops`). `kamadak-exif`
and `fast_image_resize` stay.

- **rayon is ON** via each decoder's `parallel` feature — this is the migration's primary
  motivation (parallel single-image decode for fast cold start). It provides intra-decode
  parallelism beneath the unchanged single-foreground + single-prefetch worker model. rayon
  uses a bounded global pool (it work-steals; it does **not** spawn a thread per decode), so
  it cannot recreate the old thread-per-keypress blowup. Note both workers could decode
  concurrently (foreground current + prefetch neighbor) and would share rayon's pool — fine,
  but a later refinement may scope/cap the pool if prefetch is found to steal cores from a
  foreground decode; not a launch concern.
- **Exact-pinned versions** (e.g. `zenjpeg = "=0.8.3"`, `heic = "=0.1.6"`), `Cargo.lock`
  committed. Pre-1.0 + known yank history → no surprise minor bumps.
- **No C toolchain** on the default path (avoid `imagequant`, `lcms2`, rav1d-safe asm/ffi).
  rayon is pure-Rust and does not change this.
- **MSRV floor rises to 1.93** (zenjpeg). CI must pin ≥1.93; the README already states
  Rust 1.96+, so this is satisfied. Verify with `act` (per memory `verify-ci-with-act-before-push`).
- **Encoder footprint:** where a crate has no decode-only switch, rely on
  `default-features = false` + the workspace's existing `lto = true, codegen-units = 1` to
  dead-code-eliminate unused encoder paths. (Best-effort, not a feature contract — noted as
  a medium-confidence item.)

After wiring, audit the resolved graph with `cargo tree -e features` and `cargo deny` to
confirm no transitive dep silently re-introduces a C or non-AGPL-compatible dependency
under the chosen feature set.

## Phasing

### Phase 1 — engine swap, behavioral parity (JPEG/PNG/WebP/GIF)
Replace decoding for today's four formats, with `parallel`/rayon ON. `imageset::SUPPORTED`
unchanged. The deferred RGB-downscale path is preserved. **Done when:** the decode crate's
orientation/resize tests pass unchanged, new fixture-based decode tests pass against the zen
decoders, the full workspace test suite is green, the app shows the same images as before
(manual check on a sample folder), **and the cold-start benchmark below confirms the
parallel-decode win.** No `main.rs` changes.

**Cold-start benchmark (gating for Phase 1, since speed is the goal):** measure
time-to-first-display for a large camera JPEG (and a large PNG) — comparing the new
`zen* + parallel` path against the current image-rs path — on **both x86_64 and arm64**
(Apple Silicon). Record numbers in the plan/PR. Expectation: meaningful speedup on x86_64
for JPEGs with restart markers; arm64 may show a smaller (multi-core-only, scalar-per-core)
gain until zenjpeg NEON lands. If a large JPEG shows *no* gain, verify it has DRI restart
markers (the parallel path needs them). This benchmark is how we confirm the migration
delivers its primary motivation — not a nice-to-have.

### Phase 2 — new formats (HEIC/HEIF + AVIF)
Add `heic` (`backend-rust`, `parallel`) and AVIF (`zenavif`). Extend `imageset::SUPPORTED`
with `heic`, `heif`, `avif`. Port `display_dimensions` probes for them. **Done when:** HEIC
and AVIF display; undecodable HEICs (heic decodes ~118/162 conformance files) degrade
gracefully via the existing error path; fixture tests cover a real HEIC and AVIF. Add the
AGPL/HEVC patent notice to docs.

## Testing

- **Fixtures (decided):** commit one tiny real sample per format under
  `crates/decode/tests/fixtures/` (`*.jpg/.png/.webp/.gif`; Phase 2 adds `*.heic/.avif`).
  Tests assert zen decodes them to known dimensions and sample pixels. This tests the real
  decoders against real files (more honest than round-tripping `image`'s own encoder) and
  provides the real HEIC/AVIF fixtures Phase 2 needs.
- **Why fixtures are required, not optional:** removing `image`'s codec features means the
  current tests' `RgbaImage::save(&png)` calls no longer compile. The in-memory
  orientation/resize tests (which build `RgbaImage` directly, no encode) stay as-is.
- **Coverage to keep/add:** orientation 1–8 transform map (existing, in-memory);
  downscale-fits-cap (existing, in-memory); per-format decode→dims/pixels (new, fixtures);
  `display_dimensions` header-probe per format (new); unsupported-format → `DecodeError`
  (new); graceful failure on a deliberately truncated file (new).
- **No sleeping/polling** in any test (PVC dev rule).

## Error handling

- Decode/IO/unsupported errors collapse into `DecodeError` and reach the worker, which
  already renders `"Can't display {file}: {e}"` and continues (no crash). HEIC partial
  conformance is just another `Decode(_)`.
- Truncated/corrupt files must return `Err`, never panic. zen decoders expose `Limits`
  (max pixels/memory) — set conservative caps so a malformed file can't OOM the process.

## Housekeeping

- Add `LICENSE` (AGPL-3.0) and `license = "AGPL-3.0-only"` to the workspace `Cargo.toml`
  (currently absent).
- Update `README.md`: the "imazen/zen\*" line becomes the concrete crate list + AGPL note.
- Update `AGENTS.md`: the tech-stack/formats line, and **correct line 55** — Windows-indexer
  tag writing is realistically JPEG-only (relevant to the *future* tag workstream, but the
  doc is wrong today; "stale documentation = bug").
- Push after each commit (PVC dev rule).

## Risks & mitigations

- **Ecosystem maturity (HIGH):** pre-1.0, single-vendor, prerelease decoder API, yank
  history. → Exact-pin versions, commit `Cargo.lock`, fixture-based golden tests per format,
  verify against docs.rs at coding time (API may have shifted).
- **HEIC conformance (HIGH, Phase 2):** ~118/162 HEIF files decode; output not bit-exact
  (fine for a viewer). → Graceful per-image error path (already present); ship HEIC as
  best-effort.
- **Parallel-decode win is conditional (MEDIUM) — verify, don't assume:** zenjpeg's
  `parallel` JPEG path engages only with DRI **restart markers** + ≥1024 MCU; progressive
  JPEGs get no benefit, and **ARM/Apple-Silicon is scalar today** (NEON not yet implemented;
  medium confidence). HEIC tile-parallel (~2.5×) is less conditional. → This does not change
  the decision (rayon ON), but the cold-start gain must be **measured on real target
  hardware (x86_64 and arm64) against the current image-rs path** — see the Phase-1
  benchmark. If a large camera JPEG shows no speedup, check for restart markers.
- **Prefetch vs foreground pool contention (LOW):** both workers share rayon's global pool.
  At cold start only the foreground runs (no contention). → Acceptable at launch; scope/cap
  the pool later only if benchmarks show prefetch stealing cores from a foreground decode.
- **Prerelease API drift (MEDIUM):** exact decoder signatures (`zenjpeg::decoder`,
  `heic::DecoderConfig`) must be confirmed against docs.rs when implementing — the API
  sketches in this doc are indicative, not contractual.

## Open questions

None blocking. To validate during/after implementation: real-world HEIC decode success
rate on an actual photo library; on-target arm64 single-thread decode speed (informational,
not a gate). *This document is an engineering assessment, not legal advice; the
AGPL-static-linking conclusion is well-supported but Per may confirm with counsel.*
