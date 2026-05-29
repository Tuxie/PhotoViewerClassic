slint::include_modules!();

use slint::ComponentHandle;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

const MAX_DISPLAY_DIM: u32 = 4096;

/// A request for the decode worker: which file to show, and (optionally) the
/// caption to set once it is on screen.
struct DecodeRequest {
    path: PathBuf,
    caption: Option<String>,
}

/// Which way to move the navigation cursor.
enum Nav {
    Next,
    Prev,
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

    // A single decode worker handles ALL image loads: one decode at a time, and a
    // burst of requests is coalesced to the latest. This bounds CPU to one core and
    // memory to ~one in-flight image, instead of spawning an unbounded, non-cancellable
    // decode thread per keypress (which pegged ~8 cores / multiple GB under rapid nav).
    let decode_tx = spawn_decode_worker(ui.as_weak());

    // Shared navigation cursor: read/advanced on the UI thread, populated once by
    // the background directory scan (hence Arc<Mutex<_>>).
    let nav = Arc::new(Mutex::new(imageset::ImageSet::empty()));

    ui.on_next_image({
        let nav = nav.clone();
        let tx = decode_tx.clone();
        move || {
            if let Some(req) = nav_request(&nav, Nav::Next) {
                let _ = tx.send(req);
            }
        }
    });
    ui.on_prev_image({
        let nav = nav.clone();
        let tx = decode_tx.clone();
        move || {
            if let Some(req) = nav_request(&nav, Nav::Prev) {
                let _ = tx.send(req);
            }
        }
    });

    match initial {
        Some(path) => {
            ui.set_status_text("Loading…".into());
            // Show the requested image immediately with its bare filename; the index
            // isn't known until the directory scan finishes (which then adds it).
            ui.set_caption(file_name_of(&path).into());
            let _ = decode_tx.send(DecodeRequest {
                path: path.clone(),
                caption: None,
            });
            // Scan the directory in the background, then refresh the caption with the
            // index. Navigation is a no-op until this populates `nav` (advance/retreat
            // return None on the empty placeholder), so this only ever replaces the
            // empty set — it can't clobber a user-chosen position.
            let nav_scan = nav.clone();
            let weak = ui.as_weak();
            std::thread::spawn(move || {
                let set = imageset::ImageSet::from_file(&path);
                let cap = {
                    let mut g = nav_scan.lock().unwrap();
                    *g = set;
                    caption_for(g.position(), g.len(), &path)
                };
                let _ = weak.upgrade_in_event_loop(move |c| c.set_caption(cap.into()));
            });
        }
        None => ui.set_status_text("No image. Usage: photoviewer <path>".into()),
    }

    ui.run()
}

/// Move the cursor under the lock and build the decode request for the new current
/// image, with a caption computed at dispatch time (so it always matches the pixels
/// shown, even under rapid navigation). Returns `None` if the set is empty.
fn nav_request(nav: &Arc<Mutex<imageset::ImageSet>>, dir: Nav) -> Option<DecodeRequest> {
    let mut g = nav.lock().unwrap();
    let path = match dir {
        Nav::Next => g.advance(),
        Nav::Prev => g.retreat(),
    }?;
    let caption = Some(caption_for(g.position(), g.len(), &path));
    Some(DecodeRequest { path, caption })
}

/// Spawn the single background decode worker, returning the sender used to queue
/// requests. The worker processes one request at a time via [`decode_loop`].
fn spawn_decode_worker(weak: slint::Weak<AppWindow>) -> mpsc::Sender<DecodeRequest> {
    let (tx, rx) = mpsc::channel::<DecodeRequest>();
    std::thread::spawn(move || {
        decode_loop(&rx, move |req| handle_decode(&weak, req));
    });
    tx
}

/// The worker core, decoupled from decoding/UI so it can be tested headlessly:
/// process one item at a time, coalescing any queued bursts to the latest. Returns
/// when all senders have been dropped.
fn decode_loop<T, F: FnMut(T)>(rx: &mpsc::Receiver<T>, mut work: F) {
    while let Ok(first) = rx.recv() {
        work(coalesce_latest(first, rx));
    }
}

/// Decode one request (single file read -> orient -> downscale) and push the
/// resulting image and caption to the UI. `caption == None` leaves the caption
/// untouched (used for the initial load, whose caption is owned by the
/// directory-scan thread). Errors set the status text instead.
fn handle_decode(weak: &slint::Weak<AppWindow>, req: DecodeRequest) {
    match decode::display_image(&req.path, MAX_DISPLAY_DIM) {
        Ok(rgba) => {
            let (w, h) = (rgba.width(), rgba.height());
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                rgba.as_raw(),
                w,
                h,
            );
            let caption = req.caption;
            let _ = weak.upgrade_in_event_loop(move |c| {
                c.set_current_image(slint::Image::from_rgba8(buffer));
                if let Some(cap) = caption {
                    c.set_caption(cap.into());
                }
                c.set_status_text("".into());
            });
        }
        Err(e) => {
            let msg = format!("Can't display {}: {e}", file_name_of(&req.path));
            let _ = weak.upgrade_in_event_loop(move |c| c.set_status_text(msg.into()));
        }
    }
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
}
