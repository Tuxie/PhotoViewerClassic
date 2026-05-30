slint::include_modules!();

use slint::ComponentHandle;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

const MAX_DISPLAY_DIM: u32 = 4096;

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

/// Shared decode cache, keyed by path → rotation-0 BASE buffer. Populated by both the
/// foreground worker (on a cache miss) and the prefetch worker (neighbors). Bounded by
/// the prefetch worker to the current keep-set (~3 entries) so memory stays flat.
type Cache = Arc<Mutex<HashMap<PathBuf, Arc<image::RgbaImage>>>>;

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
    // rotation in degrees; surfaced in the info overlay by Task 8.
    #[allow(dead_code)]
    rotation_deg: i32,
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
fn attach_view_handlers(ui: &AppWindow, vs: &Rc<RefCell<viewstate::ViewState>>) {
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
        }
    });
}

fn main() -> Result<(), slint::PlatformError> {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("femtovg".into())
        .select()?;

    let initial: Option<PathBuf> = std::env::args_os().nth(1).map(PathBuf::from);

    let ui = AppWindow::new()?;
    ui.on_quit(|| {
        let _ = slint::quit_event_loop();
    });

    // The pure view model lives on the UI thread; only the UI thread touches it.
    let vs = Rc::new(RefCell::new(viewstate::ViewState::new()));
    attach_view_handlers(&ui, &vs);

    // Shared decode cache: the foreground worker fills it on a miss and reads it on a
    // hit; the prefetch worker fills it with neighbors and trims it to the keep-set.
    let cache: Cache = Arc::new(Mutex::new(HashMap::new()));

    // A single decode worker handles ALL image loads: one decode at a time, draining a
    // burst of jobs to (the latest Show + net rotation). This bounds CPU to one core and
    // memory to ~one in-flight image, instead of spawning an unbounded, non-cancellable
    // decode thread per keypress (which pegged ~8 cores / multiple GB under rapid nav).
    let decode_tx = spawn_decode_worker(ui.as_weak(), cache.clone());

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
                send_prefetch(&nav, &pf);
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
                send_prefetch(&nav, &pf);
            }
        }
    });

    match initial {
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
                send_prefetch(&nav_scan, &pf);
                let _ = weak.upgrade_in_event_loop(move |c| c.set_caption(cap.into()));
            });
        }
        None => ui.set_status_text("No image. Usage: photoviewer <path>".into()),
    }

    ui.run()
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
fn send_prefetch(nav: &Arc<Mutex<imageset::ImageSet>>, pf: &mpsc::Sender<Vec<PathBuf>>) {
    let keep: Vec<PathBuf> = {
        let g = nav.lock().unwrap();
        [g.peek(-1), g.peek(0), g.peek(1)]
            .into_iter()
            .flatten()
            .collect()
    };
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

/// Obtain the rotation-0 BASE buffer for `path`: return the cached `Arc` on a hit
/// (cloning the handle, no decode), else `decode` it, cache it, and return it. Split out
/// from the worker so the cache-hit/miss behavior is testable with an injected decoder.
fn obtain_base<E>(
    cache: &Cache,
    path: &Path,
    decode: impl FnOnce(&Path) -> Result<image::RgbaImage, E>,
) -> Result<Arc<image::RgbaImage>, E> {
    if let Some(hit) = cache.lock().unwrap().get(path).cloned() {
        return Ok(hit);
    }
    let base = Arc::new(decode(path)?);
    cache
        .lock()
        .unwrap()
        .insert(path.to_path_buf(), base.clone());
    Ok(base)
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
fn spawn_decode_worker(weak: slint::Weak<AppWindow>, cache: Cache) -> mpsc::Sender<Job> {
    let (tx, rx) = mpsc::channel::<Job>();
    std::thread::spawn(move || {
        // BASE buffer (rotation 0) of the current image, and the applied turn count.
        let mut current: Option<(PathBuf, Arc<image::RgbaImage>)> = None;
        let mut turns: i32 = 0;
        while let Ok(first) = rx.recv() {
            let (show, delta) = reduce_batch(drain_batch(first, &rx));

            // A Show swaps in a fresh base and resets rotation; its decode error is the
            // only thing that can short-circuit this iteration.
            if let Some((path, caption)) = show {
                match obtain_base(&cache, &path, |p| decode::display_image(p, MAX_DISPLAY_DIM)) {
                    Ok(base) => {
                        current = Some((path, base));
                        turns = 0;
                        push_frame(&weak, &current, turns, caption, true);
                    }
                    Err(e) => {
                        let msg = format!("Can't display {}: {e}", file_name_of(&path));
                        let _ = weak.upgrade_in_event_loop(move |c| c.set_status_text(msg.into()));
                        continue;
                    }
                }
            }

            // Apply the net rotation to whatever base we have (keeping the caption).
            if delta != 0 && current.is_some() {
                turns = (turns + delta).rem_euclid(4);
                push_frame(&weak, &current, turns, None, false);
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
    let Some((_, base)) = current else { return };
    let disp = rotate_turns(base, turns);
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
    };
    let _ = weak.upgrade_in_event_loop(move |c| {
        c.set_current_image(slint::Image::from_rgba8(frame.buffer));
        if let Some(cap) = frame.caption {
            c.set_caption(cap.into());
        }
        c.set_status_text("".into());
        // Hand the displayed pixel dims to ViewState (on the UI thread, where the
        // Rc<RefCell<ViewState>> lives): a Show resets to Fit, a rotate keeps the mode.
        c.invoke_image_presented(frame.nat_w as i32, frame.nat_h as i32, is_new_image);
    });
}

/// Spawn the single prefetch worker, sharing `cache` with the foreground worker. Each
/// message is the keep-set of paths (current ±1); only the newest queued set matters, so
/// bursts coalesce. Neighbors not yet cached are decoded and inserted; any entry whose
/// key is NOT in the keep-set is dropped, bounding the cache to ~3 entries. Errors for a
/// neighbor are skipped silently (a broken file must not crash the prefetcher).
fn spawn_prefetch_worker(cache: Cache) -> mpsc::Sender<Vec<PathBuf>> {
    let (tx, rx) = mpsc::channel::<Vec<PathBuf>>();
    std::thread::spawn(move || {
        while let Ok(first) = rx.recv() {
            let keep = coalesce_latest(first, &rx);
            for path in &keep {
                let cached = cache.lock().unwrap().contains_key(path);
                if cached {
                    continue;
                }
                if let Ok(rgba) = decode::display_image(path, MAX_DISPLAY_DIM) {
                    cache.lock().unwrap().insert(path.clone(), Arc::new(rgba));
                }
            }
            // Trim anything outside the keep-set to bound memory.
            cache.lock().unwrap().retain(|k, _| keep.contains(k));
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

    /// On a cache HIT, `obtain_base` returns the cached Arc and never calls the decoder.
    /// On a MISS it calls the decoder once and caches the result for next time.
    #[test]
    fn obtain_base_uses_cache_on_hit_and_decodes_on_miss() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let cache: Cache = Arc::new(Mutex::new(HashMap::new()));
        let hit_path = PathBuf::from("/d/hit.jpg");
        let miss_path = PathBuf::from("/d/miss.jpg");

        // Pre-seed the cache with a tiny 1x1 buffer for the hit path.
        let seeded = Arc::new(image::RgbaImage::new(1, 1));
        cache
            .lock()
            .unwrap()
            .insert(hit_path.clone(), seeded.clone());

        let calls = AtomicUsize::new(0);
        let decode = |_p: &Path| -> Result<image::RgbaImage, std::io::Error> {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(image::RgbaImage::new(2, 2))
        };

        // HIT: same Arc, decoder untouched.
        let got = obtain_base(&cache, &hit_path, &decode).unwrap();
        assert!(Arc::ptr_eq(&got, &seeded), "must return the cached Arc");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "hit must not decode");

        // MISS: decoder runs once and the result is inserted into the cache.
        let got = obtain_base(&cache, &miss_path, &decode).unwrap();
        assert_eq!(got.dimensions(), (2, 2));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "miss must decode once");
        assert!(
            cache.lock().unwrap().contains_key(&miss_path),
            "miss result must be cached"
        );

        // Second call for the now-cached path must NOT decode again.
        let _ = obtain_base(&cache, &miss_path, &decode).unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "second hit must not decode"
        );
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

    /// `drain_batch` preserves order and drains the queue (unlike coalesce_latest).
    #[test]
    fn drain_batch_collects_the_whole_queue_in_order() {
        let (tx, rx) = mpsc::channel::<u32>();
        tx.send(2).unwrap();
        tx.send(3).unwrap();
        assert_eq!(drain_batch(1, &rx), vec![1, 2, 3]);
        assert!(rx.try_recv().is_err());
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
        attach_view_handlers(&ui, &vs);

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
}
