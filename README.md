# Photo Viewer Classic

A fast, simple, cross-platform desktop photo viewer (Rust + Slint).

## Run

```bash
cargo run -p app -- path/to/image.jpg
```

Shows the image fit-to-window, correctly oriented, almost immediately, then prefetches
the neighbours so next/previous are instant.

### Keyboard

| Key | Action |
|---|---|
| `→` / `L`, `←` / `H` | Next / previous image (natural-sorted directory, wraps) |
| `↑` / `K`, `↓` / `J` | Zoom in / out (toward the centre) |
| `Shift` + `←`/`→`/`↑`/`↓` (or `H`/`L`/`K`/`J`) | Pan left / right / up / down |
| `Z` | Cycle view mode: Fit → 1:1 → last manual zoom |
| `E` / `R` | Rotate counter-clockwise / clockwise (view-only; resets on navigate) |
| `F` | Toggle fullscreen |
| `I` | Toggle the info overlay (name, path, dimensions, file size, zoom %, rotation) |
| `Esc` | Close the info overlay if open, otherwise quit |
| `Q` | Quit |

### Mouse

- **Scroll** to zoom toward the cursor; **left-drag** to pan.
- Hover near the **bottom** for the toolbar (Prev / Next / Rotate L / Rotate R /
  Fullscreen / Exit); hover near the **left / right edge** for the `‹` / `›` nav buttons.

### Persistence

Window geometry and the fullscreen flag are saved to `config.toml` on quit and restored
on the next launch. The config lives in `$PVC_HOME` if set, else
`%APPDATA%\PhotoViewerClassic` on Windows or `~/.config/pvc` elsewhere.

## Build & run on macOS

The only prerequisite beyond Rust is the Xcode Command Line Tools (one-time):

```bash
xcode-select --install
```

Then, from the repo root:

```bash
cargo run -p app --release -- path/to/image.jpg
```

Keys are the same as above. A single OpenGL-deprecation line may print to the
console — that's expected and harmless (macOS deprecated, but still ships,
OpenGL, which the FemtoVG renderer uses).

Interactive view (zoom/pan/rotate/fullscreen), neighbour prefetch, the auto-hiding
chrome, the info overlay, and geometry persistence are in place. Still to come:
tag/rating editing (with Windows-searchable keywords) and HEIC/AVIF decode. See
`docs/superpowers/specs/` and `docs/superpowers/plans/`.

## CI

GitHub Actions builds and tests on Linux, macOS (Apple Silicon), and Windows on
every push and pull request — see `.github/workflows/ci.yml`.
