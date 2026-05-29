# Photo Viewer Classic

A fast, simple, cross-platform desktop photo viewer (Rust + Slint).

## Run

```bash
cargo run -p app -- path/to/image.jpg
```

- Shows the image fit-to-window, correctly oriented, almost immediately.
- `→` / `L` next image, `←` / `H` previous (natural-sorted directory, wraps).
- `Esc` / `Q` quit.

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

Phase-1 foundation: display + directory navigation. Interactive zoom/pan/rotate,
the auto-hiding toolbar, tag/rating editing, and HEIC/AVIF come in later plans.
See `docs/superpowers/specs/` and `docs/superpowers/plans/`.

## CI

GitHub Actions builds and tests on Linux, macOS (Apple Silicon), and Windows on
every push and pull request — see `.github/workflows/ci.yml`.
