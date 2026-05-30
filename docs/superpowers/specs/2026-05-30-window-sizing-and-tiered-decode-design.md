# Window sizing & two-tier decode — Design

**Goal:** Two improvements to Photo Viewer Classic, building on the v0.1.1 release:

1. **Window sizing** — the window always opens (and re-fits on navigation) to **80% of
   the desktop, at the displayed image's aspect ratio**, so the image fills the window
   with no letterboxing. An opt-in `--restore-geometry` CLI flag instead restores the
   last saved window size+position. Fullscreen state is always restored.
2. **Two-tier decode + tiered prefetch** — the **current** image is shown fast (a
   size-capped preview) and then **upgraded to full native resolution** for true 1:1
   pixel-peeping. Neighbors are prefetched: the immediate prev/next at full resolution
   (instant full-quality navigation), and the next-after-that as a cheap capped preview
   that upgrades to full when viewed.

Non-goals (explicitly deferred): tiled rendering for images larger than the GPU texture
limit; persisting the cache plan to a config file (the plan is hardcoded but structured
for that later); a tag/rating subsystem (Plan 3).

---

## Constraints & key decisions

- **GPU texture ceiling.** The displayed image is a single femtovg/OpenGL texture;
  `GL_MAX_TEXTURE_SIZE` is 16384 on essentially all GPUs of the last decade. The "full"
  decode is therefore capped at **16384** per side — true native for anything up to
  ~270 MP, covering 60+ MP photos (~9504×6336) with no downscale and real 1:1 zoom.
  Images larger than 16384 on a side are downscaled to fit (tiling is a future feature).
- **Decode stays single-threaded & bounded.** One foreground worker + one prefetch
  worker, as today. The prefetch does more work (full decodes for the inner neighbors)
  but never spawns per-request threads and never holds the cache lock across a decode.
- **Pure-Rust, no native deps.** `jpeg-decoder` (for DCT scaling) and
  `i-slint-backend-winit` (for monitor size / maximized state) are both pure-Rust and
  version-matched to Slint 1.16.
- **Hardcode now, configure later.** Cache tiers live in a `PrefetchPlan` struct with a
  hardcoded default; wiring it to CLI/config is a later, mechanical change.

---

## Component 1 — Decode crate additions (`crates/decode`)

Add `jpeg-decoder` (direct dep) for DCT-scale decoding. Two new public functions; the
existing `display_image(path, max)` is unchanged (it is the full-decode-at-a-cap path).

```rust
/// Header-only natural display dimensions (post-EXIF-orientation), without decoding
/// pixels. Used to size the window before the first decode. Returns None on error.
pub fn display_dimensions(path: &Path) -> Option<(u32, u32)>;

/// Fast, size-capped "preview" decode. For JPEG, uses jpeg-decoder's DCT scaling to
/// decode at the nearest 1/1,1/2,1/4,1/8 division >= `target`, then orient + downscale
/// to `target`. For non-JPEG (or CMYK / unexpected pixel formats) it falls back to the
/// full path `display_image(path, target)`. Always <= `target` on both sides.
pub fn display_image_fast(path: &Path, target: u32) -> image::ImageResult<RgbaImage>;
```

- `display_dimensions`: `image::image_dimensions(path)` (reads only the header) +
  `read_orientation(path)`; swap W/H when orientation ∈ {5,6,7,8}.
- `display_image_fast` (JPEG path): `jpeg_decoder::Decoder::scale(target, target)` →
  `decode()` → build `RgbaImage` from the reported pixel format (`L8` → gray→RGBA;
  `RGB24` → +opaque alpha; anything else, e.g. `CMYK32` → return `Err` so the caller
  falls back) → `apply_orientation` → `downscale_to_fit(_, target)`.
- The existing SIMD `downscale_to_fit` (fast_image_resize, Lanczos3) is reused.

**Tests:** `display_dimensions` orientation swap (5..8 swap, 1..4 don't); a synthetic
JPEG decoded via `display_image_fast` is `<= target` and approximately the requested
scale; non-JPEG input routes to the full path and matches `display_image`.

---

## Component 2 — Tiered cache & prefetch plan (`crates/imageset` + `app`)

```rust
/// How many neighbors to keep cached and at what quality. Hardcoded for now; every
/// field is a future CLI/config knob.
pub struct PrefetchPlan {
    pub full_cap: u32,      // 16384  (≈ native; GPU texture ceiling)
    pub preview_cap: u32,   // 4096
    pub behind_full: usize,    // 1  → prev 1 at full
    pub ahead_full: usize,     // 1  → next 1 at full
    pub behind_preview: usize, // 0
    pub ahead_preview: usize,  // 1  → next 2 at preview
}
const DEFAULT_PLAN: PrefetchPlan = /* the values above */;
```

`PrefetchPlan` yields, for the current cursor, a set of `(offset, target_cap)`:
- offsets `-behind_full..=ahead_full` → `full_cap`,
- the extra `behind_preview` before and `ahead_preview` after → `preview_cap`.

For the default: `{-1: full, 0: full, +1: full, +2: preview}`. A pure
`plan.targets() -> Vec<(isize, u32)>` is unit-tested (correct offsets and caps,
no duplicate offsets, the full band wins if a band would overlap).

**Cache becomes quality-aware:**
```rust
struct Cached { buffer: Arc<image::RgbaImage>, cap: u32 } // cap = the dim it was decoded to
type Cache = Arc<Mutex<HashMap<PathBuf, Cached>>>;
```
"Has it at >= target" means `cache.get(path).map_or(false, |c| c.cap >= target)` (a full
entry satisfies a preview need; never downgrade).

**Prefetch worker** (still one thread, coalescing to the latest cursor): receives the
keep-set as `Vec<(PathBuf, u32 target_cap)>`. For each, if not cached at >= target,
decode at `target` (`display_image_fast` for preview targets is fine; full targets use
`display_image(path, full_cap)`) and insert. Then `retain` the cache to the keep-set
paths. The lock is never held across a decode (unchanged invariant). `ImageSet::peek`
already gives neighbor paths without moving the cursor.

---

## Component 3 — Foreground worker: preview → full upgrade (`app`)

On `Job::Show { path, caption }` (cache `current` base = the full buffer of the shown
image, `turns` reset):

1. **Fast paint.** Obtain a buffer to show immediately and set it as the `current` base
   (so rotation works even during the preview window):
   - cache hit (any cap) → that buffer;
   - else JPEG → `display_image_fast(path, preview_cap)`;
   - else (non-JPEG cold) → `display_image(path, full_cap)` directly (no interim).
   Push the frame `is_new = true` (this also drives window resize, Component 4).
2. **Skip if stale.** Drain the queue; if a newer `Show` is pending, abandon the upgrade
   and process the newer job (no wasted full decode).
3. **Upgrade to full.** If the shown buffer's cap `< full_cap`, `display_image(path,
   full_cap)`, store it in the cache as `cap = full_cap`, replace the `current` base with
   it, and push the swap `is_new = false` (`ViewState::set_natural` keeps zoom/pan/mode).
   A full cache hit needs no upgrade — it is already the full `current` base.

`Job::Rotate` is unchanged (re-derives from the current full base). After any `Show`,
the nav handler sends the new keep-set to the prefetch worker (Component 2).

Net behavior: prev1/current/next1 are prefetched full → navigating to them is **instant
and full**; next2 shows its capped preview then upgrades; the cold first image (and
out-of-window jumps) get the JPEG DCT preview then upgrade.

---

## Component 4 — Window sizing (`app/src/window.rs`, new module)

Reaches the underlying winit window via `i-slint-backend-winit`'s `WinitWindowAccessor`:

```rust
fn monitor_size(ui: &AppWindow) -> Option<(PhysicalSize<u32>, PhysicalPosition<i32>)>;
//   winit: window.current_monitor() -> size() + position()
fn is_maximized(ui: &AppWindow) -> bool;

/// Largest rect with ratio `aspect` (w/h) fitting in 0.8 × monitor, centered on that
/// monitor (accounts for the monitor's position for multi-monitor). Pure + tested.
fn fit_80(aspect: f32, mon_size: PhysicalSize<u32>, mon_pos: PhysicalPosition<i32>)
    -> (PhysicalSize<u32>, PhysicalPosition<i32>);

/// If the window is windowed (not fullscreen, not maximized), size+center it to
/// `fit_80` for the given aspect. No-op when fullscreen/maximized or monitor unknown.
fn fit_window_to_aspect(ui: &AppWindow, aspect_w: u32, aspect_h: u32, fullscreen: bool);
```

- **Startup** (in `main`, before `run()`), in priority order:
  1. config `fullscreen` → `set_fullscreen(true)` (no sizing).
  2. else `--restore-geometry` **and** saved geometry present → restore saved size+pos.
  3. else → `display_dimensions(first_path)` → `fit_window_to_aspect`. If dimensions or
     monitor are unavailable, leave the default preferred size.
- **On navigation:** the `image-presented` handler, when `is_new == true` and not
  fullscreen, calls `fit_window_to_aspect(ui, nat_w, nat_h, fullscreen)`. Rotation
  (`is_new == false`) does not resize. Maximized windows are detected and left alone.
  The resize fires `viewport-changed` → `ViewState::set_viewport` → `apply_geometry`,
  which re-fits the image — no feedback loop (the resize is not re-triggered by the
  viewport change).

Window control lives in its own module so `main.rs` does not grow further.

---

## Component 5 — CLI parsing (`app`)

Tiny hand-rolled parse (no new dep): the first non-`--` argument is the image path;
recognized flags: `--restore-geometry`. Unknown `--flags` are ignored with a one-line
stderr note. (Structured as a small `Args { path, restore_geometry }` so adding flags —
e.g. future cache-plan overrides — is mechanical.)

---

## Data flow (navigation, the common case)

```
→/L pressed
  └─ nav_request: advance cursor, build Show{path}              [UI thread]
  └─ decode_tx.send(Show)         ──────────────►  foreground worker
  └─ send_prefetch(plan.targets)  ──────────────►  prefetch worker
foreground worker:
  cache hit at full (prev1/next1 were prefetched full)
     → push frame is_new=true  ──upgrade_in_event_loop──►  UI:
                                                            set image,
                                                            image-presented(w,h,true)
  (no upgrade needed; already full)
UI image-presented(is_new=true):
  ViewState.load(w,h);  fit_window_to_aspect(ui,w,h);  apply_geometry
prefetch worker:
  ensure {-1:full, 0:full, +1:full, +2:preview} cached; trim to keep-set
```

For next2 / cold: the first push is a preview (`is_new=true`), a second push upgrades to
full (`is_new=false`, zoom preserved).

---

## Error handling

- `display_dimensions` / `monitor_size` `None` → keep the default window size (no crash,
  no flash beyond the default).
- `display_image_fast` DCT failure or CMYK → fall back to `display_image` (full).
- Image > 16384 per side → downscaled to 16384 by `display_image(_, full_cap)`.
- `WinitWindowAccessor` unavailable (non-winit backend) → sizing is skipped gracefully.
- A GPU with `MAX_TEXTURE_SIZE` < 16384 could reject a very large texture; out of scope
  here (noted as a limitation; future: query the limit or retry at a lower cap).

---

## Testing

- **Pure unit tests:** `fit_80` (aspect wider/narrower than the 0.8 box → height- vs
  width-limited; centering; monitor offset); `display_dimensions` orientation swap;
  `PrefetchPlan::targets` (offsets + caps, full band precedence); the "has at >= cap"
  cache predicate.
- **Decode tests:** `display_image_fast` on a synthetic JPEG (`<= target`, approx scale,
  RGBA output, orientation applied); non-JPEG routes to the full path.
- **Worker logic:** factor the show-decision (preview source selection; skip-on-newer;
  upgrade-needed) into pure helpers and test them.
- **Headless GUI (i-slint-backend-testing):** existing wiring tests stay green; the
  `image-presented(is_new=true)` path still loads + re-fits geometry.
- **Manual (cannot be headless):** real window opens at 80%/aspect; resizes to aspect on
  nav; left alone when maximized/fullscreen; `--restore-geometry` restores saved
  geometry; first JPEG shows a fast preview that visibly sharpens; 60 MP photo zooms to
  true 1:1; prev1/next1 navigation is instantly full-quality.

---

## Known limitations (acceptable for this round)

- **16384 per-side full cap** (GPU texture limit). Images beyond that are downscaled;
  true pixel-peeping past 16384 needs tiled rendering (future).
- **Resize-on-nav re-centers** the window; mixed portrait/landscape sequences will see
  the window jump. This is the chosen "always 80%/aspect, centered" behavior.
- **Memory:** ~3 full + 1 preview buffer (~765 MB for 60 MP, ~330 MB for 24 MP).
  Bounded by `PrefetchPlan`; tunable down later.
- **GPUs with `MAX_TEXTURE_SIZE` < 16384** (rare/old) may fail to display the largest
  images; no runtime fallback yet.
- The cache may transiently hold a full buffer for a slot whose target is only a preview
  (an entry that slid from next1 to next2); bounded by the keep-set size, never grows
  beyond it.
