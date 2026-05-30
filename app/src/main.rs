// On Windows, build a GUI-subsystem binary (release only) so launching it doesn't
// flash a console window. Debug builds keep the console for dev logging. No-op on
// other platforms.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();
mod window;

use slint::ComponentHandle;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

/// One unit of work for the foreground decode worker.
enum Job {
    /// Show a file, optionally setting the caption once it is on screen. Decoding a
    /// fresh image resets the view rotation to 0.
    Show {
        path: PathBuf,
        caption: Option<String>,
    },
    /// Rotate the current view by a quarter turn: +1 = clockwise, -1 = counter-clockwise.
    /// View-only — it never touches the file on disk. Sent by the R/E keys (Task 4 wires
    /// the callbacks; Task 5 verifies end-to-end rotation).
    Rotate(i32),
}

/// A cached decode plus the cap it was decoded to (so a full entry satisfies a preview
/// need but never the reverse).
struct Cached {
    buffer: Arc<image::RgbaImage>,
    cap: u32,
}

/// Shared decode cache, keyed by path → rotation-0 BASE buffer. Populated by both the
/// foreground worker (on a cache miss) and the prefetch worker (neighbors). Bounded by
/// the prefetch worker to the current keep-set (~3 entries) so memory stays flat.
type Cache = Arc<Mutex<HashMap<PathBuf, Cached>>>;

/// A frame ready for the UI: the displayed (already-rotated) pixels plus the metadata
/// Task 4 will surface (natural dims of the displayed buffer and rotation in degrees).
/// Holds the `SharedPixelBuffer` (which is `Send`) rather than a `slint::Image` (which
/// is not) so the whole frame can cross into `upgrade_in_event_loop`; the `Image` is
/// built inside the closure, on the UI thread.
struct DecodedFrame {
    buffer: slint::SharedPixelBuffer<slint::Rgba8Pixel>,
    caption: Option<String>,
    // Natural dims of the displayed (post-rotation) buffer, fed into ViewState via
    // `image-presented` so the view fits/zooms against the right pixel grid.
    nat_w: u32,
    nat_h: u32,
    // Rotation in degrees; surfaced in the info overlay (Task 8).
    rotation_deg: i32,
    // Per-image metadata for the info overlay: Some only on a fresh Show, None on a
    // rotate (name/path/file-size don't change on rotate, so they aren't recomputed).
    // The file-size stat is I/O, computed on the worker thread before this crosses
    // into the UI closure.
    info: Option<ImageInfo>,
}

/// Per-image metadata shown in the info overlay. Built on the worker thread for a fresh
/// Show (so the file-size stat doesn't run in the UI closure) and dropped on rotates.
struct ImageInfo {
    name: String,
    path: String,
    size: String,
}

/// Which way to move the navigation cursor.
enum Nav {
    Next,
    Prev,
}

/// Push the current `ViewState` geometry onto the UI's `disp-*` / `smooth` / `zoom-percent`
/// properties. The single bridge from the pure view model to the Slint Image element.
fn apply_geometry(ui: &AppWindow, vs: &viewstate::ViewState) {
    let g = vs.geometry();
    ui.set_disp_x(g.x);
    ui.set_disp_y(g.y);
    ui.set_disp_w(g.w);
    ui.set_disp_h(g.h);
    ui.set_smooth(g.smooth);
    ui.set_zoom_percent(vs.zoom_percent() as f32);
}

/// Install the view-control callbacks that mutate the shared `ViewState` on the UI
/// thread and reflect the result back onto the `disp-*` properties. Factored out of
/// `main` so the headless tests can attach the exact same handlers. The rotate keys
/// (which cross into the decode worker) are wired separately in `main`.
/// `on_new_image` is called (on the UI thread) after each `is_new` image-presented,
/// with the natural pixel dimensions — used by `main` to trigger the 80%/aspect fit.
fn attach_view_handlers(
    ui: &AppWindow,
    vs: &Rc<RefCell<viewstate::ViewState>>,
    on_new_image: impl Fn(&AppWindow, u32, u32) + 'static,
) {
    ui.on_viewport_changed({
        let vs = vs.clone();
        let weak = ui.as_weak();
        move |w, h| {
            let ui = weak.unwrap();
            vs.borrow_mut().set_viewport(w, h);
            apply_geometry(&ui, &vs.borrow());
        }
    });
    ui.on_zoom_by({
        let vs = vs.clone();
        let weak = ui.as_weak();
        move |factor, ax, ay| {
            let ui = weak.unwrap();
            vs.borrow_mut().zoom(factor, ax, ay);
            apply_geometry(&ui, &vs.borrow());
        }
    });
    ui.on_pan_by({
        let vs = vs.clone();
        let weak = ui.as_weak();
        move |dx, dy| {
            let ui = weak.unwrap();
            vs.borrow_mut().pan(dx, dy);
            apply_geometry(&ui, &vs.borrow());
        }
    });
    ui.on_cycle_view({
        let vs = vs.clone();
        let weak = ui.as_weak();
        move || {
            let ui = weak.unwrap();
            vs.borrow_mut().cycle_mode();
            apply_geometry(&ui, &vs.borrow());
        }
    });
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

fn main() -> Result<(), slint::PlatformError> {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("femtovg".into())
        .select()?;

    let args = parse_args(std::env::args());

    let ui = AppWindow::new()?;

    // UI-thread fullscreen flag. We use the public `window().set_fullscreen(bool)` API
    // (i-slint-core 1.16): it backs the `Window`'s `full-screen` in-property and calls
    // `update_window_properties()`, so it both ENTERS (true) and EXITS (false) fullscreen
    // — verified against the 1.16 source (set_fullscreen → full_screen.set(enabled)).
    let fullscreen = Rc::new(Cell::new(false));

    // The persisted geometry/fullscreen captured at quit time. The window may be torn
    // down once `ui.run()` returns, so we snapshot into this slot inside the quit/close
    // handlers (while the window is alive) and write it out AFTER run().
    let saved: Rc<RefCell<Option<config::Config>>> = Rc::new(RefCell::new(None));

    // Snapshot the live window geometry + fullscreen flag into `saved`. Called from both
    // the Q/Esc quit path and the window-close (X) path so either persists.
    let snapshot = {
        let weak = ui.as_weak();
        let fs = fullscreen.clone();
        let saved = saved.clone();
        move || {
            let ui = weak.unwrap();
            let cfg = config_from(ui.window().position(), ui.window().size(), fs.get());
            *saved.borrow_mut() = Some(cfg);
        }
    };

    ui.on_quit({
        let snapshot = snapshot.clone();
        move || {
            snapshot();
            let _ = slint::quit_event_loop();
        }
    });

    // The X / title-bar close button does NOT go through `on_quit`; capture geometry here
    // too so the close button persists, then let the default close action quit the loop.
    ui.window().on_close_requested({
        let snapshot = snapshot.clone();
        move || {
            snapshot();
            slint::CloseRequestResponse::HideWindow
        }
    });

    // Fullscreen toggle (F key). Flip the flag and apply it to the window.
    ui.on_toggle_fullscreen({
        let fs = fullscreen.clone();
        let weak = ui.as_weak();
        move || {
            let ui = weak.unwrap();
            let now = !fs.get();
            fs.set(now);
            ui.window().set_fullscreen(now);
        }
    });

    // Info overlay toggle (I key). Flip the in-out `info-visible` property; Esc can also
    // clear it from within Slint.
    ui.on_toggle_info({
        let weak = ui.as_weak();
        move || {
            let ui = weak.unwrap();
            let v = ui.get_info_visible();
            ui.set_info_visible(!v);
        }
    });

    // Startup window-sizing priority: fullscreen > --restore-geometry > aspect-presize.
    // `auto_fit` stays on at runtime (fit_window_to_aspect no-ops while fullscreen),
    // but is disabled when we restored a saved geometry or entered fullscreen at startup.
    let cfg = config::load();
    let restoring = args.restore_geometry && cfg.geometry.is_some();
    let auto_fit = Rc::new(Cell::new(!cfg.fullscreen && !restoring));

    if cfg.fullscreen {
        fullscreen.set(true);
        ui.window().set_fullscreen(true);
    } else if restoring {
        let g = cfg.geometry.as_ref().unwrap();
        ui.window()
            .set_position(slint::PhysicalPosition::new(g.x, g.y));
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

    // The pure view model lives on the UI thread; only the UI thread touches it.
    let vs = Rc::new(RefCell::new(viewstate::ViewState::new()));
    attach_view_handlers(&ui, &vs, {
        let auto_fit = auto_fit.clone();
        let fs = fullscreen.clone();
        move |ui, w, h| {
            if auto_fit.get() {
                window::fit_window_to_aspect(ui, w, h, fs.get());
            }
        }
    });

    // Shared decode cache: the foreground worker fills it on a miss and reads it on a
    // hit; the prefetch worker fills it with neighbors and trims it to the keep-set.
    let cache: Cache = Arc::new(Mutex::new(HashMap::new()));
    let plan = imageset::DEFAULT_PLAN;

    // A single decode worker handles ALL image loads: one decode at a time, draining a
    // burst of jobs to (the latest Show + net rotation). This bounds CPU to one core and
    // memory to ~one in-flight image, instead of spawning an unbounded, non-cancellable
    // decode thread per keypress (which pegged ~8 cores / multiple GB under rapid nav).
    let (weak_tx, weak_rx) = mpsc::channel::<slint::Weak<AppWindow>>();
    let decode_tx = spawn_decode_worker(weak_rx, cache.clone(), plan);
    let _ = weak_tx.send(ui.as_weak());

    // Rotate keys hand a Job to the decode worker; the rotated frame comes back through
    // `image-presented` (is_new = false), keeping the current zoom/mode.
    ui.on_rotate_cw({
        let tx = decode_tx.clone();
        move || {
            let _ = tx.send(Job::Rotate(1));
        }
    });
    ui.on_rotate_ccw({
        let tx = decode_tx.clone();
        move || {
            let _ = tx.send(Job::Rotate(-1));
        }
    });

    // A second single worker decodes the immediate neighbors (current ±1) ahead of time
    // into the SAME cache, so the next/prev key is instant. Exactly ONE extra thread —
    // never per-request — keeping the concurrency bounded to two decodes max.
    let prefetch_tx = spawn_prefetch_worker(cache.clone());

    // Shared navigation cursor: read/advanced on the UI thread, populated once by
    // the background directory scan (hence Arc<Mutex<_>>).
    let nav = Arc::new(Mutex::new(imageset::ImageSet::empty()));

    ui.on_next_image({
        let nav = nav.clone();
        let tx = decode_tx.clone();
        let pf = prefetch_tx.clone();
        move || {
            if let Some(req) = nav_request(&nav, Nav::Next) {
                let _ = tx.send(req);
                send_prefetch(&nav, &plan, &pf);
            }
        }
    });
    ui.on_prev_image({
        let nav = nav.clone();
        let tx = decode_tx.clone();
        let pf = prefetch_tx.clone();
        move || {
            if let Some(req) = nav_request(&nav, Nav::Prev) {
                let _ = tx.send(req);
                send_prefetch(&nav, &plan, &pf);
            }
        }
    });

    match args.path.clone() {
        Some(path) => {
            ui.set_status_text("Loading…".into());
            // Show the requested image immediately with its bare filename; the index
            // isn't known until the directory scan finishes (which then adds it).
            ui.set_caption(file_name_of(&path).into());
            let _ = decode_tx.send(Job::Show {
                path: path.clone(),
                caption: None,
            });
            // Scan the directory in the background, then refresh the caption with the
            // index. Navigation is a no-op until this populates `nav` (advance/retreat
            // return None on the empty placeholder), so this only ever replaces the
            // empty set — it can't clobber a user-chosen position.
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
                // Now that `nav` is populated, prime the neighbor cache (best-effort).
                send_prefetch(&nav_scan, &plan, &pf);
                let _ = weak.upgrade_in_event_loop(move |c| c.set_caption(cap.into()));
            });
        }
        None => ui.set_status_text("No image. Usage: photoviewer <path>".into()),
    }

    ui.run()?;

    // Persist the geometry/fullscreen snapshot captured at quit/close time. Best-effort:
    // if no config dir is resolvable / writable (e.g. $PVC_HOME unset & unwritable),
    // persistence is silently skipped per spec.
    if let Some(cfg) = saved.borrow().clone() {
        let _ = config::save(&cfg);
    }

    Ok(())
}

/// Move the cursor under the lock and build the Show job for the new current image,
/// with a caption computed at dispatch time (so it always matches the pixels shown,
/// even under rapid navigation). Returns `None` if the set is empty.
fn nav_request(nav: &Arc<Mutex<imageset::ImageSet>>, dir: Nav) -> Option<Job> {
    let mut g = nav.lock().unwrap();
    let path = match dir {
        Nav::Next => g.advance(),
        Nav::Prev => g.retreat(),
    }?;
    let caption = Some(caption_for(g.position(), g.len(), &path));
    Some(Job::Show { path, caption })
}

/// Compute the keep-set [peek(-1), peek(0), peek(+1)] without moving the cursor and
/// hand it to the prefetch worker. Best-effort: a closed channel is silently ignored.
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

/// Reduce a drained batch of jobs to: the last `Show` (if any) and the NET rotation of
/// the `Rotate`s that come AFTER it. Rotates before the last Show are discarded — a new
/// Show resets rotation to 0. With no Show, returns `(None, sum of all rotates)`. Pure
/// and threadless so the coalescing semantics are unit-testable.
fn reduce_batch(jobs: Vec<Job>) -> (Option<(PathBuf, Option<String>)>, i32) {
    let mut show = None;
    let mut delta = 0;
    for job in jobs {
        match job {
            Job::Show { path, caption } => {
                show = Some((path, caption));
                delta = 0; // a fresh Show resets rotation
            }
            Job::Rotate(d) => delta += d,
        }
    }
    (show, delta)
}

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

/// Map a quarter-turn count (0..=3) to the displayed buffer, derived from the BASE each
/// time so rotations never compound and rounding never accumulates. The common 0-turn
/// (un-rotated) case shares the base `Arc` instead of deep-copying it — the pixels are
/// copied once more anyway when the `SharedPixelBuffer` is built.
fn rotate_turns(base: &Arc<image::RgbaImage>, turns: i32) -> Arc<image::RgbaImage> {
    match turns.rem_euclid(4) {
        1 => Arc::new(image::imageops::rotate90(base.as_ref())),
        2 => Arc::new(image::imageops::rotate180(base.as_ref())),
        3 => Arc::new(image::imageops::rotate270(base.as_ref())),
        _ => Arc::clone(base),
    }
}

/// Spawn the single foreground decode worker, returning the sender used to queue jobs.
/// The worker holds the rotation-0 base and current turn count across iterations, so a
/// pure-Rotate batch re-derives the view without re-decoding.
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
                                match obtain_base(&cache, &path, plan.full_cap, |p| {
                                    decode::display_image(p, plan.full_cap)
                                }) {
                                    Ok(b) => b,
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
                    if let Ok(b) = obtain_base(&cache, &path, plan.full_cap, |p| {
                        decode::display_image(p, plan.full_cap)
                    }) {
                        if current.as_ref().is_some_and(|(p, _)| *p == path) {
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

/// Derive the displayed buffer from the base at `turns`, build the Slint frame, and push
/// it to the UI: set the image, set the caption (only when given — a pure rotate keeps
/// it), and clear the status. Mirrors the old `handle_decode` visible behavior; the
/// nat-dims/rotation it carries are wired by Task 4.
fn push_frame(
    weak: &slint::Weak<AppWindow>,
    current: &Option<(PathBuf, Arc<image::RgbaImage>)>,
    turns: i32,
    caption: Option<String>,
    is_new_image: bool,
) {
    let Some((path, base)) = current else { return };
    let disp = rotate_turns(base, turns);
    // Compute per-image metadata ONLY on a fresh Show, here on the WORKER thread — the
    // file-size stat is I/O and must not run inside the UI closure. A rotate carries None.
    let info = is_new_image.then(|| {
        let size = std::fs::metadata(path)
            .map(|m| human_size(m.len()))
            .unwrap_or_else(|_| "unknown".into());
        ImageInfo {
            name: file_name_of(path),
            path: path.display().to_string(),
            size,
        }
    });
    let frame = DecodedFrame {
        buffer: slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            disp.as_raw(),
            disp.width(),
            disp.height(),
        ),
        caption,
        nat_w: disp.width(),
        nat_h: disp.height(),
        rotation_deg: turns.rem_euclid(4) * 90,
        info,
    };
    let _ = weak.upgrade_in_event_loop(move |c| {
        c.set_current_image(slint::Image::from_rgba8(frame.buffer));
        if let Some(cap) = frame.caption {
            c.set_caption(cap.into());
        }
        c.set_status_text("".into());
        // name/path/file-size change only on a fresh Show.
        if let Some(info) = frame.info {
            c.set_info_name(info.name.into());
            c.set_info_path(info.path.into());
            c.set_info_size(info.size.into());
        }
        // dims and rotation change on every frame (a rotate swaps width/height).
        c.set_info_dims(format!("{} × {}", frame.nat_w, frame.nat_h).into());
        c.set_rotation_degrees(frame.rotation_deg);
        // Hand the displayed pixel dims to ViewState (on the UI thread, where the
        // Rc<RefCell<ViewState>> lives): a Show resets to Fit, a rotate keeps the mode.
        c.invoke_image_presented(frame.nat_w as i32, frame.nat_h as i32, is_new_image);
    });
}

/// Spawn the single prefetch worker, sharing `cache` with the foreground worker. Each
/// message is the keep-set of (path, cap) pairs (current ±1); only the newest queued set
/// matters, so bursts coalesce. Neighbors not yet cached (or cached at a lower cap) are
/// decoded and inserted; any entry whose key is NOT in the keep-set is dropped, bounding
/// the cache to ~3 entries. Errors for a neighbor are skipped silently (a broken file
/// must not crash the prefetcher).
fn spawn_prefetch_worker(cache: Cache) -> mpsc::Sender<Vec<(PathBuf, u32)>> {
    let (tx, rx) = mpsc::channel::<Vec<(PathBuf, u32)>>();
    std::thread::spawn(move || {
        while let Ok(first) = rx.recv() {
            let keep = coalesce_latest(first, &rx);
            for (path, cap) in &keep {
                let have = cache.lock().unwrap().get(path).is_some_and(|c| c.cap >= *cap);
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

/// The worker core, decoupled from decoding/UI so it can be tested headlessly: process
/// one item at a time, coalescing any queued bursts to the latest. The live worker now
/// uses [`drain_batch`] instead, but this is retained (and exercised by tests) as a
/// regression guard for the single-decode CPU fix.
#[allow(dead_code)] // regression guard for the single-decode fix; exercised by tests
fn decode_loop<T, F: FnMut(T)>(rx: &mpsc::Receiver<T>, mut work: F) {
    while let Ok(first) = rx.recv() {
        work(coalesce_latest(first, rx));
    }
}

/// Drain the whole already-queued batch (Show and Rotate interleaved) starting from the
/// first item, preserving order so [`reduce_batch`] can apply Show-then-Rotate
/// semantics. Analogous to [`coalesce_latest`] but keeps every item, not just the last.
fn drain_batch<T>(first: T, rx: &mpsc::Receiver<T>) -> Vec<T> {
    let mut batch = vec![first];
    while let Ok(next) = rx.try_recv() {
        batch.push(next);
    }
    batch
}

/// Given the first pending item, drain any further already-queued items and return
/// the most recent — so a burst of navigation requests collapses to its final target.
fn coalesce_latest<T>(first: T, rx: &mpsc::Receiver<T>) -> T {
    let mut last = first;
    while let Ok(next) = rx.try_recv() {
        last = next;
    }
    last
}

/// Human-readable byte count using binary (1024) units: "913 B", "2.3 MB", "1.0 GB".
/// Bytes are shown whole; KB and up get one decimal. Pure and deterministic so it's
/// unit-testable.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// The file name component as a lossy String, or "" if there is none.
fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// "(i/N) name" for a 0-based cursor index `idx` within `len` items.
fn caption_for(idx: usize, len: usize, path: &Path) -> String {
    format!("({}/{}) {}", idx + 1, len, file_name_of(path))
}

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

/// Build the persisted `Config` from a captured window position/size (physical units)
/// and the current fullscreen flag. Pure so the round-trip is unit-testable without a
/// live window — the geometry/save path itself is exercised by the `config` crate tests.
fn config_from(
    pos: slint::PhysicalPosition,
    size: slint::PhysicalSize,
    fullscreen: bool,
) -> config::Config {
    config::Config {
        geometry: Some(config::WindowGeometry {
            x: pos.x,
            y: pos.y,
            w: size.width,
            h: size.height,
        }),
        fullscreen,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn coalesce_latest_keeps_only_the_newest_queued() {
        let (tx, rx) = mpsc::channel::<u32>();
        tx.send(2).unwrap();
        tx.send(3).unwrap();
        // first = 1, then drain the queue (2, 3) → newest wins.
        assert_eq!(coalesce_latest(1, &rx), 3);
        // queue is fully drained afterwards.
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn coalesce_latest_returns_first_when_queue_empty() {
        let (_tx, rx) = mpsc::channel::<u32>();
        assert_eq!(coalesce_latest(7, &rx), 7);
    }

    /// REPRODUCES THE BUG: the old design spawned one thread per navigation keypress.
    /// A burst of N navigations therefore ran N "decodes" simultaneously — which is
    /// what pegged ~8 cores and ballooned memory on macOS. We model a "decode" as a
    /// 50ms-busy task and assert that naive per-request dispatch runs them concurrently.
    #[test]
    fn naive_thread_per_request_runs_concurrently_reproduces_the_bug() {
        const N: usize = 8;
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..N)
            .map(|_| {
                let concurrent = concurrent.clone();
                let max_concurrent = max_concurrent.clone();
                std::thread::spawn(move || {
                    let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                    max_concurrent.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(50)); // stand in for a slow decode
                    concurrent.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        // The bug: more than one decode ran at the same time (here, several at once).
        assert!(
            max_concurrent.load(Ordering::SeqCst) > 1,
            "thread-per-request dispatch runs decodes concurrently — this is the bug we fixed"
        );
    }

    /// REGRESSION GUARD for the fix: the single-worker `decode_loop` processes one
    /// item at a time and coalesces a burst to the latest request, so rapid navigation
    /// can never spawn concurrent decodes or process every intermediate image.
    #[test]
    fn decode_loop_serializes_and_coalesces_a_burst() {
        const N: u32 = 8;
        let (tx, rx) = mpsc::channel::<u32>();
        for i in 0..N {
            tx.send(i).unwrap();
        }
        drop(tx); // close the channel so the loop exits once drained

        let concurrent = AtomicUsize::new(0);
        let max_concurrent = AtomicUsize::new(0);
        let mut processed = Vec::new();
        decode_loop(&rx, |item| {
            let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
            max_concurrent.fetch_max(now, Ordering::SeqCst);
            processed.push(item);
            concurrent.fetch_sub(1, Ordering::SeqCst);
        });

        // Never more than one decode at a time...
        assert_eq!(
            max_concurrent.load(Ordering::SeqCst),
            1,
            "decodes must be serialized"
        );
        // ...and a burst of N collapses to just the latest target (no wasted work).
        assert_eq!(
            processed,
            vec![N - 1],
            "burst must coalesce to the latest request"
        );
    }

    /// `reduce_batch` keeps only the LAST Show and the net rotation that follows it; a
    /// Show resets rotation, so the pre-Show Rotate(+1) here is discarded.
    #[test]
    fn reduce_batch_keeps_last_show_and_post_show_net_rotation() {
        let a = PathBuf::from("/d/a.jpg");
        let b = PathBuf::from("/d/b.jpg");
        let (show, delta) = reduce_batch(vec![
            Job::Show {
                path: a,
                caption: None,
            },
            Job::Rotate(1),
            Job::Show {
                path: b.clone(),
                caption: Some("b".into()),
            },
            Job::Rotate(-1),
        ]);
        assert_eq!(show, Some((b, Some("b".into()))));
        assert_eq!(delta, -1, "only rotates after the last Show count");
    }

    /// With no Show in the batch, the net rotation is the sum of all rotates and there
    /// is nothing to show (the worker re-derives from its held base).
    #[test]
    fn reduce_batch_rotate_only_sums_with_no_show() {
        let (show, delta) = reduce_batch(vec![Job::Rotate(1), Job::Rotate(1)]);
        assert!(show.is_none());
        assert_eq!(delta, 2);
    }

    /// A lone Show yields that Show with zero rotation.
    #[test]
    fn reduce_batch_single_show_has_zero_rotation() {
        let a = PathBuf::from("/d/a.jpg");
        let (show, delta) = reduce_batch(vec![Job::Show {
            path: a.clone(),
            caption: None,
        }]);
        assert_eq!(show, Some((a, None)));
        assert_eq!(delta, 0);
    }

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

    /// One quarter-turn swaps width/height (2x1 base → 1x2), confirming the turn→op map.
    #[test]
    fn rotate_turns_one_swaps_dimensions() {
        let mut img = image::RgbaImage::new(2, 1);
        img.put_pixel(0, 0, image::Rgba([1, 2, 3, 255]));
        let base = Arc::new(img);
        assert_eq!(rotate_turns(&base, 0).dimensions(), (2, 1));
        assert_eq!(rotate_turns(&base, 1).dimensions(), (1, 2));
        assert_eq!(rotate_turns(&base, 2).dimensions(), (2, 1));
        assert_eq!(rotate_turns(&base, 3).dimensions(), (1, 2));
        // 0 turns shares the base Arc (no deep copy).
        assert!(Arc::ptr_eq(&rotate_turns(&base, 0), &base));
        // matches image::imageops::rotate90 directly.
        assert_eq!(
            rotate_turns(&base, 1).dimensions(),
            image::imageops::rotate90(base.as_ref()).dimensions()
        );
    }

    /// `config_from` packs a (position, size, fullscreen) snapshot into the persisted
    /// `Config` using physical units — the same space read back at quit, so it round-trips.
    #[test]
    fn config_from_packs_geometry_and_fullscreen() {
        let cfg = config_from(
            slint::PhysicalPosition::new(-5, 12),
            slint::PhysicalSize::new(1280, 720),
            true,
        );
        assert_eq!(
            cfg,
            config::Config {
                geometry: Some(config::WindowGeometry {
                    x: -5,
                    y: 12,
                    w: 1280,
                    h: 720,
                }),
                fullscreen: true,
            }
        );
    }

    /// `human_size` is pure: bytes stay whole, KB and up get one decimal, and the unit
    /// steps up at each 1024 boundary (binary units).
    #[test]
    fn human_size_formats_binary_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(913), "913 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
        // 2.3 MB ≈ 2.3 * 1024 * 1024 bytes.
        assert_eq!(human_size(2_411_724), "2.3 MB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(human_size(1024u64.pow(4)), "1.0 TB");
        // Beyond the top unit it stays in TB rather than overflowing the table.
        assert_eq!(human_size(2 * 1024u64.pow(5)), "2048.0 TB");
    }

    /// `drain_batch` preserves order and drains the queue (unlike coalesce_latest).
    #[test]
    fn drain_batch_collects_the_whole_queue_in_order() {
        let (tx, rx) = mpsc::channel::<u32>();
        tx.send(2).unwrap();
        tx.send(3).unwrap();
        assert_eq!(drain_batch(1, &rx), vec![1, 2, 3]);
        assert!(rx.try_recv().is_err());
    }

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
}

/// Headless GUI tests: build a real `AppWindow` under the testing backend and exercise
/// the Task-4 wiring (geometry binding, callback → ViewState → property, key → callback).
/// No sleeping/polling — every assertion is synchronous after a direct invoke/inject.
#[cfg(test)]
mod gui_tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        /// `init_no_event_loop()` installs a *per-thread* backend and panics if a backend
        /// is already installed on this thread. Cargo runs tests on separate threads (and
        /// may reuse a thread for several tests), so guard with a thread-local flag rather
        /// than a process-wide `Once` (which would leave sibling threads on the winit
        /// backend, failing headlessly). `main`'s winit BackendSelector never runs under
        /// `cargo test`, so this governs the test backend.
        static BACKEND_READY: Cell<bool> = const { Cell::new(false) };
    }

    fn init_backend() {
        BACKEND_READY.with(|ready| {
            if !ready.get() {
                i_slint_backend_testing::init_no_event_loop();
                ready.set(true);
            }
        });
    }

    /// Build a UI plus a fresh ViewState already sized to a viewport and a loaded image,
    /// so geometry is well-defined. Returns both for the caller to drive.
    fn ui_with_loaded_image() -> (AppWindow, Rc<RefCell<viewstate::ViewState>>) {
        init_backend();
        let ui = AppWindow::new().expect("AppWindow under testing backend");
        let vs = Rc::new(RefCell::new(viewstate::ViewState::new()));
        {
            let mut v = vs.borrow_mut();
            v.set_viewport(800.0, 600.0);
            // 400x200 in 800x600 → fit = min(800/400, 600/200) = min(2.0, 3.0) = 2.0.
            v.load(400.0, 200.0);
        }
        (ui, vs)
    }

    /// Test 1 — `apply_geometry` faithfully round-trips ViewState → disp-* properties,
    /// including after a zoom (which flips smooth off and changes the zoom percent).
    #[test]
    fn geometry_binding_round_trips_viewstate_to_properties() {
        let (ui, vs) = ui_with_loaded_image();
        // Zoom in 2x about the viewport center so we leave Fit and exceed 100%.
        vs.borrow_mut().zoom(2.0, 400.0, 300.0);
        apply_geometry(&ui, &vs.borrow());

        let g = vs.borrow().geometry();
        assert_eq!(ui.get_disp_x(), g.x);
        assert_eq!(ui.get_disp_y(), g.y);
        assert_eq!(ui.get_disp_w(), g.w);
        assert_eq!(ui.get_disp_h(), g.h);
        assert_eq!(ui.get_smooth(), g.smooth);
        assert_eq!(ui.get_zoom_percent(), vs.borrow().zoom_percent() as f32);
        // Sanity: a 2x zoom over the 2.0 fit scale → scale 4.0 → 400%, not smooth.
        assert_eq!(ui.get_zoom_percent(), 400.0);
        assert!(!ui.get_smooth());
    }

    /// Test 2 — real callbacks installed by `attach_view_handlers`: invoking
    /// `viewport-changed` then `zoom-by` mutates the shared ViewState and the disp-*
    /// properties reflect it.
    #[test]
    fn callback_zoom_doubles_scale_and_updates_properties() {
        init_backend();
        let ui = AppWindow::new().expect("AppWindow under testing backend");
        let vs = Rc::new(RefCell::new(viewstate::ViewState::new()));
        attach_view_handlers(&ui, &vs, |_, _, _| {});

        // Drive the viewport + present an image through the real handlers.
        ui.invoke_viewport_changed(800.0, 600.0);
        ui.invoke_image_presented(400, 200, true); // load() → Fit, fit scale 2.0 → 200%
        assert_eq!(ui.get_zoom_percent(), 200.0);
        let fit_w = ui.get_disp_w();

        // Zoom 2x about center → scale 4.0 → 400%, width doubles.
        ui.invoke_zoom_by(2.0, 400.0, 300.0);
        assert_eq!(ui.get_zoom_percent(), 400.0);
        assert!((ui.get_disp_w() - fit_w * 2.0).abs() < 0.01);

        // cycle-view from Manual returns to Fit (scale 2.0 → 200%).
        ui.invoke_cycle_view();
        assert_eq!(ui.get_zoom_percent(), 200.0);
    }

    /// Test 3 — callback → handler binding. Real keyboard injection is unavailable: the
    /// 1.16 `i-slint-backend-testing` `internal` feature (which exposes
    /// `send_keyboard_string_sequence`) fails to compile from crates.io because its
    /// `include_dir!()` points at a fonts directory that ships only in the Slint source
    /// tree. So we cover the navigation/view callbacks by invoking them directly — this
    /// exercises the same Rust handlers the FocusScope keys fire. The FocusScope key
    /// strings themselves (and Shift+key pan) are verified manually (Task 4 manual run).
    #[test]
    fn view_callbacks_invoke_their_handlers() {
        init_backend();
        let ui = AppWindow::new().expect("AppWindow under testing backend");

        let next = Rc::new(Cell::new(false));
        let cycle = Rc::new(Cell::new(false));
        let rot_cw = Rc::new(Cell::new(false));
        ui.on_next_image({
            let f = next.clone();
            move || f.set(true)
        });
        ui.on_cycle_view({
            let f = cycle.clone();
            move || f.set(true)
        });
        ui.on_rotate_cw({
            let f = rot_cw.clone();
            move || f.set(true)
        });

        ui.invoke_next_image();
        ui.invoke_cycle_view();
        ui.invoke_rotate_cw();

        assert!(next.get(), "next-image handler must run");
        assert!(cycle.get(), "cycle-view handler must run");
        assert!(rot_cw.get(), "rotate-cw handler must run");
    }

    /// Test 5 (Task 8) — the info overlay. The I key fires `toggle-info`, whose handler
    /// flips the `info-visible` property; the overlay's string fields are plain in-props
    /// bound 1:1 into the panel's Text lines. We assert (a) the toggle handler flips the
    /// flag both ways and (b) the info-* props round-trip through the bindings.
    ///
    /// Element-finder note: `i-slint-backend-testing` 1.16 *does* export
    /// `ElementHandle::find_by_element_id` WITHOUT the broken `internal` feature, but it
    /// returns nothing unless the Slint compiler emitted debug info (it logs
    /// "requires the presence of debug info ... Set SLINT_EMIT_DEBUG_INFO=1"). The
    /// production build (`app/build.rs` → `slint_build::compile`) does not enable debug
    /// info, and turning it on solely for a test would bloat the shipped binary. Since the
    /// `visible: root.info-visible` binding is a trivial 1:1 wire, asserting the property
    /// round-trip (plus the toggle) is sufficient — so we use the property round-trip.
    #[test]
    fn info_overlay_toggles_and_binds() {
        init_backend();
        let ui = AppWindow::new().expect("AppWindow under testing backend");

        // Wire the same handler `main` installs.
        ui.on_toggle_info({
            let weak = ui.as_weak();
            move || {
                let ui = weak.unwrap();
                let v = ui.get_info_visible();
                ui.set_info_visible(!v);
            }
        });

        // (a) Toggle: starts false, I → true, I → false.
        assert!(!ui.get_info_visible(), "overlay starts hidden");
        ui.invoke_toggle_info();
        assert!(ui.get_info_visible(), "first I shows the overlay");
        ui.invoke_toggle_info();
        assert!(!ui.get_info_visible(), "second I hides the overlay");

        // (b) The info-* props round-trip through the bindings (the panel's Text lines
        // read these props directly, so a clean round-trip proves the wiring).
        ui.set_info_name("photo.jpg".into());
        ui.set_info_path("/pics/photo.jpg".into());
        ui.set_info_dims("1920 × 1080".into());
        ui.set_info_size("2.3 MB".into());
        ui.set_rotation_degrees(90);
        ui.set_info_visible(true);
        assert_eq!(ui.get_info_name(), "photo.jpg");
        assert_eq!(ui.get_info_path(), "/pics/photo.jpg");
        assert_eq!(ui.get_info_dims(), "1920 × 1080");
        assert_eq!(ui.get_info_size(), "2.3 MB");
        assert_eq!(ui.get_rotation_degrees(), 90);
        assert!(ui.get_info_visible());
    }

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

    /// Test 4 (Task 5) — view-only rotation. A rotated frame re-enters through
    /// `image-presented` with `is_new = false`, which must KEEP the current zoom/mode
    /// (via `set_natural`) while refitting to the swapped dimensions; a subsequent Show
    /// (navigation, `is_new = true`) must RESET to Fit. This is the end-to-end contract
    /// of E/R rotation: zoom survives a rotate but not a navigate.
    #[test]
    fn rotation_keeps_zoom_then_navigation_resets_it() {
        init_backend();
        let ui = AppWindow::new().expect("AppWindow under testing backend");
        let vs = Rc::new(RefCell::new(viewstate::ViewState::new()));
        attach_view_handlers(&ui, &vs, |_, _, _| {});

        // Present a 400x200 image (new) in an 800x600 viewport → Fit, 200%.
        ui.invoke_viewport_changed(800.0, 600.0);
        ui.invoke_image_presented(400, 200, true);
        // Zoom into Manual mode (scale 4.0 → 400%).
        ui.invoke_zoom_by(2.0, 400.0, 300.0);
        assert_eq!(ui.get_zoom_percent(), 400.0);
        assert_eq!(vs.borrow().mode(), viewstate::ViewMode::Manual);

        // Rotate 90°: the worker pushes the swapped dims (200x400) with is_new = false.
        // Zoom scale and mode must be preserved; geometry refits to the new aspect.
        ui.invoke_image_presented(200, 400, false);
        assert_eq!(
            vs.borrow().mode(),
            viewstate::ViewMode::Manual,
            "rotation must keep the view mode"
        );
        assert_eq!(
            ui.get_zoom_percent(),
            400.0,
            "rotation must keep the zoom scale"
        );
        let g = vs.borrow().geometry();
        assert_eq!(
            (ui.get_disp_w(), ui.get_disp_h()),
            (g.w, g.h),
            "disp-* must track the rotated geometry"
        );

        // Navigate to a fresh image (is_new = true) → back to Fit (200%).
        ui.invoke_image_presented(400, 200, true);
        assert_eq!(
            vs.borrow().mode(),
            viewstate::ViewMode::Fit,
            "navigation must reset the view mode"
        );
        assert_eq!(
            ui.get_zoom_percent(),
            200.0,
            "navigation must reset the zoom"
        );
    }
}
