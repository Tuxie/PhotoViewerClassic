# Photo Viewer Classic — Phase 1 Design

- **Date:** 2026-05-29
- **Status:** Approved (design); pending spec review → implementation plan
- **Scope:** Phase 1 = fast core viewer + tag/rating editing for JPEG/PNG/WebP/GIF

---

## 1. Overview & principles

Photo Viewer Classic (PVC) is a traditional, simple, **fast** cross-platform desktop
photo viewer (Windows 11 / macOS / Linux), built in **Rust + Slint**. Its
distinguishing feature is editing image **tags/keywords** (and **star ratings**) so
they travel with the file and, on Windows, are surfaced by the built-in Search.

**Core principles (from AGENTS.md):**

1. **Extremely fast to start and display the first image** passed as a parameter.
   Anything not strictly required for that first paint (font loading beyond default,
   EXIF extraction, broad format support, directory scan, `tags.txt`) happens
   **asynchronously, after the first frame**.
2. Minimal UI — the photo owns the screen; controls are auto-hiding overlays.
3. Self-contained: a small single binary, **pure-Rust dependencies only** (no
   C/C++/glib), trivial to cross-compile.

**Phase 1 formats:** JPEG, PNG, WebP, GIF (decode + tag/rating write).
**Deferred to phase 2+:** HEIC, AVIF, full EXIF info overlay, and the items in §11.

---

## 2. Key decisions & research findings

This design is grounded in adversarially-verified research (primary Microsoft docs,
format specs, crate sources). The load-bearing findings:

### 2.1 Windows Search only indexes tags for JPEG/TIFF — *not* PNG/WebP/GIF/HEIC/AVIF

Windows keyword search is powered by the `System.Keywords` property, which the indexer
obtains from a per-format **property handler** (`IPropertyStore`). The built-in WIC
"photo property handler" is documented to work **only with TIFF- or JPEG-container
formats**, and the `System.Keywords` metadata policy explicitly lists **"Containers:
JPEG, TIFF"** and nothing else. PNG/GIF/WebP (and HEIC/AVIF) get **no keyword handler**,
so even perfectly-embedded XMP `dc:subject` is invisible to Explorer's Tags column and
to Windows Search for those formats.

- Windows 11 **24H2** made the PNG Details pane *populate* (UI change, from Insider
  build 25987), and its blog even mentions "add keywords" — but this is a UI/handler
  wiring change, **not** a metadata-engine change. Empirically Windows round-trips
  exactly one PNG field (Date Taken → `PNG:CreationTime`); keywords are **not** written,
  read, or Search-indexed. The PNG WIC codec has no `/xmp` query path.
- A **custom property handler** *could* make PNG tags Search-indexed (it's the
  authoritative mechanism, and registering it does trigger re-indexing), but it is a
  native, admin-installed, **machine-wide** COM DLL (two bitness builds, HKLM
  registration, replaces Windows' built-in image handler for *all* apps, owns
  crash/security/uninstall risk). This flatly contradicts PVC's simple/portable/
  cross-platform/pure-Rust goals. **Decision: not in phase 1; documented optional
  later add-on only.** The existing third-party **File Meta** (Dijji) solves PNG via an
  NTFS alternate data stream (not embedded; lost on copy to non-NTFS) — pointed to as a
  power-user option, not emulated.

### 2.2 Tag strategy: embed-only + document the limit (chosen)

- Embed standards-correct tags into **every** format's bytes (pure-Rust, no pixel
  re-encode). JPEG round-trips to Windows Search; all formats interop with
  Lightroom / macOS Finder / digiKam / ExifTool.
- For JPEG, **mirror exactly what Explorer writes**: XMP `dc:subject` **and** IPTC-IIM
  Keywords (in an APP13/8BIM IRB) **and** `MicrosoftPhoto:LastKeyword{XMP,IPTC}`.
- `tags.txt` is purely the **autocomplete dictionary** (as AGENTS.md describes). **No
  in-app tag search UI** in phase 1.
- Surface a clear in-product note that **Windows Search natively finds only JPEG tags**.

### 2.3 Other decisions

| Decision | Choice |
|---|---|
| Navigation set | All supported images in the file's directory, **natural sort**, **wrap-around**, scanned async after first frame |
| Rotation | Persisted **losslessly via EXIF Orientation** (JPEG/PNG-`eXIf`/WebP); **GIF = view-only** (no orientation tag) |
| Animation | GIF / animated WebP: **first frame instant, then loop async** |
| Persistence | **Window geometry + fullscreen** only; view (zoom/pan/rotation) resets per image |
| Star rating | New feature: `xmp:Rating` 0–5 via the same metadata engine; JPEG also EXIF `0x4746` (Windows `System.Rating`, JPEG-only constraint) |
| Context menu | Navigate · Rotate L/R · Zoom toggle · Fullscreen · Edit Tags… · Copy to clipboard · Show in file manager · ★ Rating (0–5/clear) · Quit. **No** Delete / Open-with |
| Dev/test platform | **Linux primary**; validate JPEG→Windows-Search on a Windows 11 VM at milestones |

---

## 3. Architecture

A single self-contained Rust binary, organized as a small workspace so non-UI logic is
unit-testable without a GUI.

| Unit | Responsibility | Key deps | Isolated tests? |
|---|---|---|---|
| `app` (bin) | CLI arg, backend pinning, event-loop wiring, thread orchestration | all | thin glue |
| `imageset` | dir scan, supported-ext filter, natural sort, wrap navigation | std | ✅ pure logic |
| `viewstate` | zoom/pan/rotation + view-mode (fit / 1:1 / last) state machine | — | ✅ pure logic |
| `decode` | RGBA8 decode, orientation apply, downscale-huge, animation frame iterator | `image` | ✅ fixtures |
| `metadata` | read/write tags + rating + EXIF orientation; per-container byte injection | `img-parts`, `xmp-writer`, `little_exif`, `quick-xml` | ✅ round-trip |
| `tagstore` | `tags.txt` load/append + subsequence autocomplete filter | std | ✅ pure logic |
| `config` | `$PVC_HOME` resolution; window-geometry/fullscreen persistence | `serde`, `toml` | ✅ |
| `ui` (`.slint`) | window, image element (zoom/pan/rotate bindings), toolbar, overlays, tag editor, context menu, `FocusScope` keys | `slint` | UI-level |

**Threading model:** one UI thread (Slint event loop) + background workers (thread pool
or dedicated threads + channels) for decode, prefetch, metadata I/O, dir scan. Results
marshalled back via `slint::invoke_from_event_loop` on a `Weak` component handle.
`SharedPixelBuffer<Rgba8Pixel>` is `Send` (build off-thread); `Image::from_rgba8` is
called **inside** the UI-thread closure. **No blocking work on the UI thread, ever.**

**Renderer:** ship `winit + FemtoVG` (OpenGL), pinned deterministically via
`BackendSelector` **before** showing any window (avoid probe-and-fallback latency).
The software renderer is **disqualified** — it cannot scale or rotate images. Slint's
`image-default-formats` is **off** (we decode ourselves).

### Startup pipeline (strategy "A")

1. Parse the path arg (minimal).
2. Pin the backend; create + show the window with an empty canvas.
3. Spawn a worker to decode **only the first file** to RGBA8 (+ read its EXIF
   orientation); push the buffer to the UI via `invoke_from_event_loop`.
4. **After the first frame:** async dir scan + natural sort, `tags.txt` load, neighbor
   prefetch, animation frames, non-default fonts.

*Future enhancement (not phase 1):* progressive preview — decode an embedded
thumbnail/preview for an instant blurry frame, then swap in full-res.

---

## 4. Core viewer behavior

- **Display:** decode → `SharedPixelBuffer` → `Image`. Orientation is **normalized by
  transforming the pixel buffer** (`image::imageops` rotate/flip) per the EXIF
  Orientation value (1–8, including the mirrored variants), so the first paint is
  correctly oriented and the zoom/pan math stays axis-aligned. (Note: Slint 1.16's
  element rotation property is `transform-rotation`, **not** `rotation-angle` — the
  Image-only property was generalized to `transform-*` in 1.14; we avoid relying on it
  for orientation.)
- **Zoom** (`↑`/`K`, `↓`/`J`, scrollwheel): a single `scale` property drives the
  `Image` `width`/`height` (= `natural_size * scale`); derived from one property to
  avoid binding loops (no transform matrix exists in Slint). ~1.25×/step, clamped
  ~0.05×–32×. Scrollwheel zooms toward the cursor; keyboard toward center.
- **Pan** (`Shift`+arrows/`HJKL`, left-drag): `x`/`y` offsets in a clipping container;
  centered when smaller than viewport, clamped when larger.
- **View-mode** (`Z`): state machine **Fit → 1:1 → last manual zoom**. Fit =
  `image-fit: contain` + `image-rendering: smooth`; 1:1/high-zoom = `pixelated`.
- **Rotation** (`E` CCW, `R` CW): rotates the displayed pixel buffer 90° for instant
  feedback, then persists losslessly via the EXIF Orientation tag written async (atomic
  temp-then-rename) with `little_exif` (JPEG→APP1/Exif, WebP→RIFF `EXIF`, PNG→`eXIf`).
  **GIF: view-only**, resets on navigate.
- **Navigation** (`←`/`H` prev, `→`/`L` next, wrap over natural-sorted dir set):
  instant if prefetched, else decode async; **reset view to Fit** for the new image;
  prefetch ±1 (maybe ±2) in the background.
- **Animation:** first frame instant; remaining frames decode off-thread, loop on a
  timer honoring per-frame delays; zoom/pan/rotate apply to frames.
- **Large images:** decode a display-sized downscaled copy for the GPU (guard
  max-texture-size); keep full-res available for 1:1 via source-clip.

---

## 5. Tag/rating subsystem

### 5.1 Metadata engine (write path)

Keywords **and** rating live in **one XMP packet** built with `xmp-writer`
(`dc:subject` rdf:Bag; `xmp:Rating` 0–5), injected into the original bytes with
`img-parts` — **no pixel decode/re-encode**. Other metadata (ICC, EXIF, …) is preserved;
only our packet is added/replaced.

| Format | Mechanism | Risk |
|---|---|---|
| **JPEG** | APP1 XMP segment (sig `http://ns.adobe.com/xap/1.0/\0` + packet), inserted early. **Also** hand-rolled IPTC-IIM Keywords (dataset 2:25) in an APP13/8BIM IRB + `MicrosoftPhoto:LastKeyword{XMP,IPTC}`; rating also EXIF `0x4746`. | Low |
| **PNG** | `iTXt` chunk, keyword `XML:com.adobe.xmp`. (Interop only; not Windows-indexed.) | Low |
| **WebP** | `XMP ` RIFF chunk **plus** manual promotion to extended `VP8X` with the `0x04` XMP flag set (img-parts won't) and fixed RIFF size. | Medium |
| **GIF** | Hand-rolled Application Extension (`XMP Data`+`XMP`) + exact 258-byte magic trailer; GIF89a only. | High |

**Read path** (pre-fill editor): extract `dc:subject` + `xmp:Rating` from any embedded
XMP via `quick-xml` (tolerant of other tools' packets); merge JPEG IPTC keywords as a
fallback.

**Safety (non-negotiable):** write to a temp file → fsync → **re-parse to verify**
(still decodes; our packet present) → **atomic rename** over the original. Any failure →
original untouched, error surfaced. Never mutate the only copy in place. Idempotent
(re-save replaces, never duplicates segments/chunks). Unicode keywords supported.

### 5.2 Tag editor overlay (`T`)

A **custom overlay** (not `PopupWindow`, which can't host the data-bound filtered list):
top-z panel with a **pre-focused** search `TextInput` and a list.

- The list is **two-state**, matching AGENTS.md exactly:
  - **Empty search:** the list is pre-populated with the **file's current tags** (the
    set that will be saved; `Space` removes one).
  - **Non-empty search:** the list shows **all known tags** (from `tags.txt`) filtered by
    case-insensitive **subsequence match** (`abc` → `*a*b*c*`); entries already on the
    file are **checked**, `Space` toggles membership. If nothing matches exactly, an
    **"➕ Add new tag: '…'"** row appears at the top.
- Flow (from AGENTS.md): search pre-focused; typing filters live; `Down`/`Tab` from
  search → into the list; `Up`/`Down` navigates; `Space` toggles a tag (or adds the new
  one); `Tab` → back to search; **`Enter` saves**; **`Esc` cancels** (confirm-discard if
  there were unsaved changes).
- On save: write embedded metadata via §5.1 **and** append brand-new tags to `tags.txt`
  (deduped).

### 5.3 Rating UX

Set via context-menu **★ (0–5 / clear)** submenu; written through the same engine into
`xmp:Rating` (+ EXIF `0x4746` for JPEG). Current rating shown in the info overlay.

---

## 6. UI, overlays & input

- **Window:** normal titled window; image in a clipping container that fills it. `F`
  toggles fullscreen (`window().set_fullscreen`), state persisted.
- **Auto-hiding overlays** (fade in on proximity):
  - **Bottom toolbar** (hover near bottom): `◀ Prev · Next ▶ · ↺ Rotate L · Rotate R ↻
    · ⤢ Fullscreen · ✕ Exit`.
  - **Edge buttons** (hover near left/right): large `‹` / `›` for Previous/Next.
- **Context menu** (`M` or right-click): custom Slint overlay, anchored at cursor
  (right-click) or centered (`M`): Previous · Next · Rotate L · Rotate R · Zoom toggle ·
  Fullscreen · Edit Tags… · Copy to clipboard · Show in file manager · ★ Rating ▸
  (0–5/clear) · Quit. Copy uses `arboard`; "Show in file manager" does the platform
  reveal-and-select (Explorer `/select`, Finder reveal, `xdg-open` dir).
- **Info overlay** (`I`, toggle): **phase 1 shows** filename, full path, dimensions
  (W×H), file size, current zoom %, rotation, ★ rating, tags. **Full EXIF fields
  deferred** to the later EXIF phase.
- **Keyboard:** one top-level `FocusScope`; global shortcuts use `capture-key-pressed`
  so they beat a focused `TextInput`, except when the tag editor is open (keys route to
  it). `KeyEvent` exposes only `text`/`modifiers`/`repeat`; use `modifiers.shift` for the
  pan bindings. Full map per AGENTS.md: `E`/`R` rotate, `←`/`H` & `→`/`L` nav, `↑`/`K` &
  `↓`/`J` zoom, `Shift`+those pan, `Z` view-mode, `F` fullscreen, `I` info, `T` tags,
  `M` menu, `Enter` confirm, `Q` quit.
- **`Esc` semantics** ("Cancel / Back / Quit"): if a menu/editor/overlay is open →
  close/cancel it (tag editor with unsaved changes → confirm-discard); if nothing open →
  quit.
- **Mouse:** scrollwheel zooms toward cursor; left-drag pans; right-click → context
  menu; edge/bottom hover reveals overlays.

---

## 7. Data, persistence & error handling

**`$PVC_HOME`** — honor the env var; else `%APPDATA%\PhotoViewerClassic` (Windows) or
`~/.config/pvc` (Linux/macOS). Created lazily.

- **`tags.txt`** — every tag ever added, one per line, UTF-8. Loaded async after first
  frame into a dedup set. Append-only on save (dedup on read) → concurrency-safe enough
  without locking.
- **`config.toml`** (`serde` + `toml`) — window x/y/w/h + fullscreen. Written on exit
  (debounced on change). Corrupt/missing → defaults.

**CLI:** `pvc <path>` displays immediately + builds the nav set from its directory
(async). `pvc` (no arg) → empty window + hint, still responsive. Multiple args → use the
first file (and its directory) in phase 1.

**Error handling — the app stays alive and never destroys data:**

| Situation | Behavior |
|---|---|
| Missing/invalid path arg | Empty state + hint; app runs |
| Corrupt/unsupported file while navigating | In-place "⚠ Can't display *name*" placeholder; nav continues |
| First-image decode fails | Placeholder + message; navigation still works |
| Metadata **write** failure | Atomic write → original untouched; non-fatal error toast |
| Image file read-only / permission denied | Error toast; original untouched |
| `tags.txt` unreadable/corrupt | Treated as empty, never overwritten (append-only); tagging still works |
| `$PVC_HOME` not writable | Embedded tags still saved; autocomplete won't grow; geometry persistence skipped |
| `config.toml` corrupt | Ignored; defaults |

**Multiple instances:** each launch is its own process/window (no single-instance IPC).
Append-only `tags.txt` and last-writer-wins `config.toml` tolerate concurrency.

---

## 8. Testing strategy (TDD — test first)

**Pure-logic unit tests:** `imageset` (natural sort, ext filter, wrap nav, edge cases),
`viewstate` (zoom clamp/steps, view-mode cycle, pan clamp), `tagstore` (subsequence
filter, dedupe, append), `config` (serde round-trip + corrupt fallback).

**`metadata` (highest value)**, against real per-format fixtures:
- Round-trip: write tags+rating → re-read → sets match.
- **Pixel integrity:** decode before vs after → byte-identical.
- File still decodes (GIF still animates) after write.
- JPEG: both XMP `dc:subject` **and** IPTC-IIM block present; rating in EXIF `0x4746`.
- WebP: `VP8X` present with `0x04` set; round-trip across VP8 / VP8L / animated.
- GIF: **byte-compare** App-Extension + 258-byte trailer vs an ExifTool reference.
- Safety: simulated mid-write failure → original intact; verify-before-replace catches
  a bad output. Idempotence (no duplicate segments). Unicode keywords.

**Integration / cross-platform:** CI builds on Linux + cross-compiles Windows/macOS.
**Windows 11 acceptance gate (manual, VM):** tag a JPEG → Explorer Tags column shows it,
`tags:foo` finds it, rating stars appear. Tracked **cold-start-to-first-frame** budget on
a large JPEG per OS.

---

## 9. Risk register

| Risk | Sev | Mitigation |
|---|---|---|
| GIF 258-byte magic trailer exactness | High | Reference-fixture byte compare; ship GIF last; verify-before-replace |
| WebP `VP8X` `0x04` flag | Med | Manual VP8X patch + reference round-trip (VP8/VP8L/animated) |
| Destroying the user's only copy | High→Low | temp + fsync + verify + atomic rename; never in-place |
| FemtoVG cold-start / GPU init | Med | Pin backend; measure; (software renderer can't scale/rotate) |
| GPU max-texture-size on huge photos | Med | Downscale display copy; full-res only at 1:1 |
| `System.Rating` JPEG-only assumption | Low | Validate at Windows milestone (same handler as keywords) |

---

## 10. Build order within phase 1

Each step independently testable:

1. **Skeleton + fast first image** (CLI → pinned backend → worker decode → oriented
   display; startup-time test) — proves the core principle first.
2. **Navigation + view** (dir scan/sort/wrap/prefetch; zoom/pan/rotate view-only;
   view-mode; large-image downscale).
3. **Persistence + chrome** (config geometry/fullscreen; toolbar + edge buttons; context
   menu; info overlay).
4. **Metadata engine — JPEG first** (XMP+IPTC tags, rating, EXIF-orientation persist;
   atomic+verify) → **Windows acceptance gate**.
5. **Tag editor overlay** (full `T` modal + `tags.txt` autocomplete flow).
6. **Remaining formats** — PNG → WebP (`VP8X`) → GIF (last).
7. **Animation** (GIF / animated WebP looping).
8. **Clipboard copy + show-in-file-manager**.

---

## 11. Explicitly deferred (phase 2+)

HEIC / AVIF decode + ISOBMFF XMP; full EXIF info overlay; optional Windows
property-handler add-on for PNG-in-Search; progressive-preview decode; in-app tag
search; multi-arg/glob nav sets; drag-and-drop; delete / open-with.

---

## 12. Dependencies (all pure-Rust, no native deps)

| Crate | Purpose |
|---|---|
| `slint` (1.16+, `winit`+`FemtoVG`) | GUI / rendering |
| `image` (image-rs, 0.25) | **decode only** (RGBA8) + `imageops` orientation/rotate/downscale + animation frames; never used to re-encode (it strips metadata) |
| `kamadak-exif` (0.6, as `exif`) | cheap EXIF **Orientation** read for correct first paint |
| `img-parts` (0.4) | JPEG/PNG/WebP chunk/segment surgery for XMP injection (no re-encode) |
| `xmp-writer` (0.3) | XMP packet serialization (`dc:subject`, `xmp:Rating`) |
| `little_exif` | EXIF write (orientation + JPEG rating tag) |
| `quick-xml` | read existing XMP packets (extract keywords + rating) |
| `arboard` | clipboard copy |
| `serde` + `toml` | `config.toml` persistence |
| *(hand-rolled, no crate)* | JPEG APP13/8BIM IPTC-IIM block; GIF App-Extension + 258-byte trailer |

**Rejected:** `rexiv2`/`gexiv2`/`exiv2` and Adobe `xmp_toolkit` — C++/glib native deps
break the small-binary / easy-cross-compile goal.

---

## 13. Assumptions to confirm

- Windows `System.Rating` for JPEG behaves like `System.Keywords` (JPEG/TIFF-only,
  reads `xmp:Rating`/EXIF `0x4746`) — to be validated at the Windows milestone.
- A lightweight info overlay (non-EXIF basics) in phase 1 is acceptable (EXIF deferred).
- Number-key (`0`–`5`) rating shortcuts are **not** in phase 1 (rating is context-menu
  only); can be added later if wanted.

---

## 14. References (selected, primary)

- System.Keywords policy (Containers: JPEG, TIFF):
  <https://learn.microsoft.com/en-us/windows/win32/wic/-wic-photoprop-system-keywords>
- WIC photo property handler (TIFF/JPEG-container only):
  <https://learn.microsoft.com/en-us/windows/win32/wic/-wic-integrationregentries>
- Native image metadata queries (PNG has no `/xmp` path):
  <https://learn.microsoft.com/en-us/windows/win32/wic/-wic-native-image-format-metadata-queries>
- Building property handlers (custom handler mechanism):
  <https://learn.microsoft.com/en-us/windows/win32/properties/building-property-handlers-property-handlers>
- File Meta (third-party PNG handler via NTFS ADS):
  <https://github.com/Dijji/FileMeta>
- `img-parts` and `xmp-writer` sources (pure-Rust injection).
