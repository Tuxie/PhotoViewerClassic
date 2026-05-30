# Window sizing & tiered decode — Design (rev. 2)

**Goal:** Two improvements to Photo Viewer Classic, building on the v0.1.1 release:

1. **Window sizing** — the window always opens (and re-fits on navigation) to **80% of
   the desktop, at the displayed image's aspect ratio**, so the image fills the window
   with no letterboxing. An opt-in `--restore-geometry` CLI flag instead restores the
   last saved window size+position. Fullscreen state is always restored.
2. **Tiered decode + tiered prefetch** — the **current** image (and its immediate
   neighbours) are cached at **full native resolution up to the GPU texture limit** for
   true 1:1 pixel-peeping; the next-after-next is cached as a cheap 4096 preview that is
   upgraded to full when it becomes a near neighbour. All decodes use the existing
   `display_image` (full decode + SIMD downscale) — **no DCT / no new decode dependency**.
3. **Cold-start latency** — three reductions to time-to-first-display of the file named on
   the command line: start its decode before UI setup, defer RGBA expansion on decodes
   that downscale, and pre-size the window to the image's aspect before `run()`.

**This revision (rev. 2) changes from the first draft:**
- **Drops `jpeg-decoder` and the DCT-scaled preview.** The JPEG **entropy (Huffman) decode
  is the floor and DCT scaling does not reduce it**, so the preview tier is produced by the
  existing full-decode-then-downscale path instead. No new image-decoding dependency.
- **Fixes window-sizing lifecycle:** the winit window does not exist before `run()`, so
  monitor-based sizing happens on the **first `image-presented(is_new=true)`**, not in
  `main()` before `run()`.
- **Clamps the full cap to the real GPU limit** (`min(16384, GL_MAX_TEXTURE_SIZE)`) —
  femtovg renders an oversize texture **silently black**, not as an error.
- Adds the three cold-start reductions above.

Non-goals (explicitly deferred): **tiled rendering** for images larger than the GPU texture
limit (those are downscaled to the cap — i.e. true 1:1 only up to the GPU max, by design);
a DCT / partial-decode fast path (rejected — entropy decode dominates); persisting the cache
plan to config (the plan is a hardcoded struct, structured for that later); a tag/rating
subsystem (Plan 3).

---

## Constraints & key decisions

- **GPU texture ceiling.** The displayed image is a single femtovg/OpenGL texture.
  `GL_MAX_TEXTURE_SIZE` is **16384 on mainstream desktop GPUs** (NVIDIA/AMD dedicated and
  recent Intel/Apple integrated), covering 60+ MP photos (~9504×6336) at true 1:1 — but it
  is only **~8192 on older Intel iGPUs, the Mesa software rasterizer, and some Linux/VM
  stacks** (~66% of Linux supports 16384 vs ~96% of Windows). Crucially, femtovg **never
  queries `GL_MAX_TEXTURE_SIZE` and never checks `glGetError`** — an oversize texture is
  created with `GL_INVALID_VALUE` and **renders black/blank, with no panic and no error**.
  Therefore `full_cap = min(16384, GL_MAX_TEXTURE_SIZE)`, queried once at renderer setup;
  images larger than `full_cap` are downscaled to it (no tiling).
- **Decode stays single-threaded & bounded.** One foreground worker + one prefetch worker,
  as today. The foreground decodes the current image at `full_cap`; the prefetch decodes
  each neighbour at its tier's cap. Neither spawns per-request threads, and the cache lock
  is never held across a decode.
- **No DCT, no new decode dependency.** Previews are full-decode-then-downscale via the
  existing `display_image` + SIMD `fast_image_resize`. The entropy decode is the
  unavoidable floor; DCT scaling does not reduce it, so it isn't worth `jpeg-decoder`.
- **New pure-Rust deps:** `i-slint-backend-winit` (pinned `=1.16.1`, version-matched to
  Slint, for monitor size; winit types come from its `pub use winit` re-export) and `glow`
  (matched to femtovg's version, to read `GL_MAX_TEXTURE_SIZE` via the rendering notifier).
  **No `jpeg-decoder`.**
- **Hardcode now, configure later.** Cache tiers live in a `PrefetchPlan` struct with a
  hardcoded default; wiring it to CLI/config is a later, mechanical change.

---

## Component 1 — Decode crate additions (`crates/decode`)

Two changes; the existing public `display_image(path, max)` keeps its signature and output.

```rust
/// Header-only natural display dimensions (post-EXIF-orientation), without decoding
/// pixels. Used to size the window before the first decode. Returns None on error.
pub fn display_dimensions(path: &Path) -> Option<(u32, u32)>;
```

- `display_dimensions`: `image::image_dimensions(path)` (reads only the header) +
  `read_orientation(path)`; swap W/H when orientation ∈ {5,6,7,8} (the 90°/270° transpose
  cases — matches the existing `apply_orientation`). **Caveat:** `image_dimensions` guesses
  format by **file extension** (unlike the decode path's magic-byte sniffing); on a
  missing/wrong extension it returns `None` and the caller falls back to the default window
  size. Acceptable (no crash).

- `display_image(path, cap)` is optimised to **defer RGBA expansion**: decode in the
  decoder's native channel layout (RGB for JPEG), apply EXIF orientation and SIMD-downscale
  **in that layout**, and expand to RGBA8 **only after** downscaling. This avoids allocating
  and copying a full **native-resolution** RGBA buffer on every decode that downscales
  (~240 MB at 60 MP). Images already within `cap` (e.g. the current image at `full_cap` when
  native ≤ cap) expand to RGBA at native size as today — no downscale, so no saving and no
  regression. `apply_orientation` and the resize helper gain a non-RGBA path (both
  `image::imageops` and `fast_image_resize` support RGB/`U8x3`). Output stays `RgbaImage`.

- **No** `display_image_fast` and **no** `jpeg-decoder`.

**Tests:** `display_dimensions` orientation swap (5..8 swap, 1..4 don't); the deferred-RGBA
`display_image` yields pixels equal to the naive decode→to_rgba8→orient→downscale path
(within Lanczos tolerance), is `<= cap` on both sides, and applies orientation; existing
`display_image` tests stay green.

---

## Component 2 — Tiered cache & prefetch plan (`crates/imageset` + `app`)

```rust
/// How many neighbours to keep cached and at what quality. Hardcoded for now; every
/// field is a future CLI/config knob, so the cache-window size is trivially configurable.
pub struct PrefetchPlan {
    pub full_cap: u32,         // 16384 ceiling, clamped to GL_MAX_TEXTURE_SIZE at runtime
    pub preview_cap: u32,      // 4096
    pub behind_full: usize,    // 1  → prev1 at full
    pub ahead_full: usize,     // 1  → next1 at full
    pub behind_preview: usize, // 0
    pub ahead_preview: usize,  // 1  → next2 at preview
}
const DEFAULT_PLAN: PrefetchPlan = /* the values above */;
```

`PrefetchPlan::targets() -> Vec<(isize, u32)>` yields, for the current cursor:
- offsets `-behind_full..=ahead_full` → `full_cap`,
- the extra `behind_preview` before and `ahead_preview` after → `preview_cap`.

For the default: `{-1: full, 0: full, +1: full, +2: preview}`. Pure and unit-tested
(correct offsets and caps, no duplicate offsets, the full band wins if bands overlap).

**Cache becomes quality-aware:**
```rust
struct Cached { buffer: Arc<image::RgbaImage>, cap: u32 } // cap = the dim it was decoded to
type Cache = Arc<Mutex<HashMap<PathBuf, Cached>>>;
```
"Has it at >= target" means `cache.get(path).map_or(false, |c| c.cap >= target)` (a full
entry satisfies a preview need; never downgrade).

**Prefetch worker** (still one thread, coalescing to the latest keep-set): receives the
keep-set as `Vec<(PathBuf, u32 target_cap)>`. For each, if not cached at `>= target`, decode
at `target` via `display_image(path, target)` and insert `Cached { cap: target }`. Then
`retain` the cache to the keep-set paths. The lock is never held across a decode. The
preview→full **upgrade of a rising neighbour happens here**: on a forward step, the old
`next2` (preview, 4096) becomes `next1` (full target) → its `cap < target` → re-decoded at
`full_cap`. `ImageSet::peek` already gives neighbour paths without moving the cursor.

---

## Component 3 — Foreground worker: show + preview→full upgrade (`app`)

On `Job::Show { path, caption }` (current base = the shown buffer; `turns` reset to 0):

1. **Resolve the shown buffer:**
   - **cache hit at `full_cap`** → use it as the current base; push `is_new = true`. Done —
     no upgrade. (The common case: `prev1`/`next1` were prefetched full.)
   - **cache hit at preview only** → use the 4096 preview as the current base; push
     `is_new = true`; then go to step 3.
   - **cache miss** (the cold first image, or prefetch far behind) → decode at `full_cap`;
     current base; push `is_new = true`. Done — **one decode**, no placeholder, no second
     decode ("fastest real frame").
2. **Skip if stale.** Before upgrading, drain the queue; if a newer `Show` is pending,
   abandon the upgrade and process the newer job (no wasted full decode).
3. **Upgrade (preview-hit case only).** Decode at `full_cap`, insert `Cached { full_cap }`,
   replace the current base, and push `is_new = false` **routed through the existing
   `push_frame(&weak, &current, turns, None, false)` so the current `turns` is re-applied** —
   a rotation performed during the preview window must survive the swap (the verified
   rotation trap: a naive push of the raw full buffer would snap rotation back to 0°).
   `ViewState::set_natural` keeps zoom/pan/mode.

`Job::Rotate` is unchanged (re-derives from the current full base). After any `Show`, the nav
handler sends the new keep-set (`plan.targets` mapped through `peek`) to the prefetch worker.

Net: `prev1`/current/`next1` are full → navigating to them is **instant and full**; `next2`
shows its 4096 preview then upgrades; the cold first image is a single full decode.

---

## Component 4 — Window sizing (`app/src/window.rs`, new module)

Reaches the underlying winit window via `i-slint-backend-winit`'s `WinitWindowAccessor`
(added as a direct `=1.16.1` dep — it is presently only transitive; winit types via its
`pub use winit`). Window control lives in its own module so `main.rs` doesn't grow further.

```rust
fn monitor_size(ui: &AppWindow) -> Option<(PhysicalSize<u32>, PhysicalPosition<i32>)>;
//   ui.window().with_winit_window(|w| w.current_monitor().map(|m| (m.size(), m.position()))).flatten()
fn is_maximized(ui: &AppWindow) -> bool;          // slint's own ui.window().is_maximized() — no winit round-trip

/// Largest rect with ratio `aspect` (w/h) fitting in 0.8 × monitor, centered on that
/// monitor (mon_pos + offset; positions may be negative on multi-monitor — keep signed). Pure + tested.
fn fit_80(aspect: f32, mon_size: PhysicalSize<u32>, mon_pos: PhysicalPosition<i32>)
    -> (PhysicalSize<u32>, PhysicalPosition<i32>);

/// If windowed (not fullscreen, not maximized) and the monitor is known, size+center the
/// window to `fit_80` for the given aspect, applying via SLINT types (manual winit→slint
/// conversion: slint::PhysicalSize::new(w,h) / slint::PhysicalPosition::new(x,y)). No-op otherwise.
fn fit_window_to_aspect(ui: &AppWindow, aspect_w: u32, aspect_h: u32, fullscreen: bool);
```

**Lifecycle (the linchpin fix).** In Slint 1.16 + winit the window is created lazily during
the event loop's `resumed` phase — so **before `run()` the winit window does not exist** and
`current_monitor()` returns `None`. (`set_size`/`set_position`/`set_fullscreen` work pre-run
only because Slint buffers them into `WindowAttributes`; that write-buffer does **not** make
monitor reads work.) Therefore:

- **Before `run()` (monitor-independent only):**
  1. `config.fullscreen` → `set_fullscreen(true)` (no sizing).
  2. else `--restore-geometry` **and** saved geometry present → restore saved size+pos
     (buffered — safe).
  3. else (cold-start opt) → read `display_dimensions(first_path)` and buffer a window size
     at the image's **aspect** (using the default preferred height as the reference), so the
     window opens at the right *shape*. Header read only — no monitor needed. On `None`,
     leave the default preferred size.
- **On the first `image-presented(is_new=true)`** (and every subsequent one — gated by an
  `auto_fit` flag, NOT a one-shot guard, so it re-fits on each navigation; `auto_fit` is off
  when starting fullscreen or restoring saved geometry):
  if not `config.fullscreen`, not restored-geometry, and not `is_maximized` → call
  `fit_window_to_aspect` for the precise **80% of monitor**. The window is now live, so the
  monitor and `scale_factor` are real. This is the natural single site — navigation already
  routes resize through `image-presented`.
- **On navigation:** `image-presented(is_new == true)` (and not fullscreen) re-fits via
  `fit_window_to_aspect`. Rotation (`is_new == false`) does **not** resize. The resize fires
  `viewport-changed` → `set_viewport` → `apply_geometry`, which re-fits the image — **no
  feedback loop** (`apply_geometry` never resizes the window). A brief double-fit (fit to the
  old viewport, then re-fit to the new) is possible on a portrait↔landscape step — cosmetic.

---

## Component 5 — CLI parsing (`app`)

Tiny hand-rolled parse (no new dep): the first non-`--` argument is the image path;
recognized flags: `--restore-geometry`. Unknown `--flags` are ignored with a one-line
stderr note. Structured as `Args { path, restore_geometry }` so adding flags is mechanical.

**Behaviour change vs today:** the app currently *always* restores saved geometry. The new
default is fit-to-80%/aspect, and restore is opt-in via `--restore-geometry`. Geometry is
**still saved at quit regardless** (only the *restore* is gated, so the flag works on the
next launch); `config.fullscreen` is always restored. Priority: fullscreen > restore-flag >
80%/aspect fit.

---

## Cold-start latency (cross-cutting)

1. **Start the decode before UI setup.** Dispatch the initial `Job::Show` to the foreground
   worker **before** backend init / `AppWindow::new`, so the file read + decode overlap UI
   construction. The worker is spawned without the UI handle; the `Weak<AppWindow>` is
   delivered to it via a one-shot once the window is constructed; the worker **decodes
   exactly once** and pushes when both the buffer and the handle are ready (no double
   decode). Magnitude: modest — it overlaps only a few ms of setup, and the entropy-decode
   floor dominates; larger benefit on big images.
2. **Defer RGBA expansion** (Component 1) on any decode that downscales (previews, oversized
   images) — saves a full native-resolution RGBA alloc+copy.
3. **Aspect pre-size** (Component 4) — opens the window at the right shape to cut the
   first-paint resize/reflow blip.

## GL_MAX_TEXTURE_SIZE clamp

At renderer setup, via `ui.window().set_rendering_notifier(...)` on
`RenderingState::RenderingSetup`, build a `glow` context over the provided `GraphicsAPI` and
read `GL_MAX_TEXTURE_SIZE` into a shared cell. Decoders use `full_cap = min(16384, gl_max)`.
Because the cold decode may start before the GL context exists, the **texture-build step
clamps the displayed buffer to the known GL limit as a backstop** — an oversize buffer is
downscaled (cheap, from the already-decoded buffer) rather than rendered black.

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
  ensure {-1:full, 0:full, +1:full, +2:preview} cached; trim to keep-set;
  upgrade the new next1 (was next2 preview) to full
```

For the cold first image: a single `full_cap` decode, pushed `is_new=true`. For a landed-on
preview neighbour (prefetch behind): preview push (`is_new=true`), then a full upgrade
(`is_new=false`, zoom/rotation preserved).

---

## Error handling

- `display_dimensions` / `monitor_size` `None` → keep the default window size (no crash).
- Oversize texture → **backstop downscale to `GL_MAX`** at texture-build (not a silent black
  frame).
- Image > `full_cap` → downscaled to `full_cap` by `display_image` (no tiling).
- `WinitWindowAccessor` unavailable (window not yet live / non-winit backend) → sizing is
  skipped gracefully.
- `GL_MAX` not yet known at the first decode → decode optimistically at 16384 + push-time
  backstop clamp.

---

## Testing

- **Pure unit tests:** `fit_80` (aspect wider/narrower than the 0.8 box → height- vs
  width-limited; centering; monitor offset; negative positions); `display_dimensions`
  orientation swap; `PrefetchPlan::targets` (offsets + caps, full-band precedence, no dups);
  the "has at >= cap" cache predicate; CLI `Args` parse; startup-decision priority
  (fullscreen > restore > fit).
- **Decode tests:** deferred-RGBA `display_image` equals the naive path (within tolerance),
  is `<= cap`, orientation applied; existing tests stay green.
- **Worker logic (pure helpers):** show-source selection (full hit / preview hit / miss);
  skip-on-newer; the upgrade re-applies `turns` (Show(preview) → Rotate(+1) → upgrade ⇒
  pushed dims == `rotate90(full).dimensions()`).
- **Headless GUI (`i-slint-backend-testing`):** existing wiring stays green; the
  `image-presented(is_new=true)` path loads + re-fits geometry; the one-shot first-fit guard
  fires once.
- **Manual (cannot be headless):** window opens at 80%/aspect, and at the right *aspect
  immediately* (pre-size); resizes to aspect on nav; left alone when maximized/fullscreen;
  `--restore-geometry` restores saved geometry; 60 MP photo zooms to true 1:1; prev1/next1
  navigation is instantly full-quality; next2 shows a preview that sharpens; on an 8192-cap
  GPU a >8192 image shows **downscaled, not black**; fit-view sharpness of a large photo is
  acceptable.

---

## Known limitations (acceptable for this round)

- **No tiling beyond the GPU texture limit.** Images larger than `full_cap`
  (`= min(16384, GL_MAX_TEXTURE_SIZE)`) are downscaled — true 1:1 only up to the GPU max, by
  design.
- **`full_cap` clamped to `GL_MAX_TEXTURE_SIZE`.** On 8192-cap GPUs (older Intel iGPU,
  Mesa-software, some Linux/VM) large photos display downscaled rather than at 16384.
- **Resize-on-nav re-centers** the window; mixed portrait/landscape sequences will see the
  window jump. This is the chosen "always 80%/aspect, centered" behaviour.
- **Memory (heap image buffers):** steady-state ~765 MB / 60 MP, ~330 MB / 24 MP (3 full +
  1 preview); **realistic peak ~1.3 GB / 60 MP** (transient upgrade buffer + a rotate copy +
  the Slint `clone_from_slice` of the displayed frame, co-resident); **~1 GB per buffer at
  the 16384 cap**. Slint's own GPU/CPU texture copy is additional. Bounded by `PrefetchPlan`;
  tunable down later.
- **Fit-view of the full-res current image is GPU-minified** by femtovg (no mipmaps), so
  high-frequency content can alias. Accepted; a CPU-downscaled fit buffer is a future option
  if it proves objectionable.
- **Cold-start decode-early** overlaps only a few ms of setup; the entropy-decode floor is
  irreducible without a different decoder (rejected this round).
