# Photo Viewer Classic

A fast, simple, cross-platform desktop photo viewer (Rust + Slint).

## Run

```bash
cargo run -p app -- path/to/image.jpg
```

- Shows the image fit-to-window, correctly oriented, almost immediately.
- `→` / `L` next image, `←` / `H` previous (natural-sorted directory, wraps).
- `Esc` / `Q` quit.

Phase-1 foundation: display + directory navigation. Interactive zoom/pan/rotate,
the auto-hiding toolbar, tag/rating editing, and HEIC/AVIF come in later plans.
See `docs/superpowers/specs/` and `docs/superpowers/plans/`.
