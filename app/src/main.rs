slint::include_modules!();

use slint::ComponentHandle;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MAX_DISPLAY_DIM: u32 = 4096;

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

    // Shared navigation cursor: read/advanced on the UI thread, populated once by
    // the background directory scan (hence Arc<Mutex<_>>).
    let nav = Arc::new(Mutex::new(imageset::ImageSet::empty()));

    ui.on_next_image({
        let nav = nav.clone();
        let weak = ui.as_weak();
        move || {
            // Advance and compute the caption while holding the lock, so the caption
            // always matches the image this worker loads — even if the user navigates
            // again before the decode finishes.
            let (path, cap) = {
                let mut g = nav.lock().unwrap();
                match g.advance() {
                    Some(p) => {
                        let cap = caption_for(g.position(), g.len(), &p);
                        (Some(p), cap)
                    }
                    None => (None, String::new()),
                }
            };
            if let Some(p) = path {
                load_image(weak.clone(), Some(cap), p);
            }
        }
    });
    ui.on_prev_image({
        let nav = nav.clone();
        let weak = ui.as_weak();
        move || {
            let (path, cap) = {
                let mut g = nav.lock().unwrap();
                match g.retreat() {
                    Some(p) => {
                        let cap = caption_for(g.position(), g.len(), &p);
                        (Some(p), cap)
                    }
                    None => (None, String::new()),
                }
            };
            if let Some(p) = path {
                load_image(weak.clone(), Some(cap), p);
            }
        }
    });

    match initial {
        Some(path) => {
            ui.set_status_text("Loading…".into());
            // Show the requested image immediately with its bare filename; the index
            // isn't known until the directory scan finishes (which then adds it).
            ui.set_caption(file_name_of(&path).into());
            load_image(ui.as_weak(), None, path.clone());
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

/// Decode `path` on a worker thread (single file read -> orient -> downscale) and
/// push the image back to the UI. `caption` is `Some` when the caller has a
/// dispatch-time caption to set (so it always matches the pixels shown); `None`
/// leaves the caption untouched (used for the initial load, whose caption is owned
/// by the directory-scan thread). Errors set the status text instead.
///
/// Note: under very rapid navigation, decodes can complete out of dispatch order,
/// so the last-*completed* image wins (not necessarily the last-*requested*). A
/// sequence guard / prefetch to enforce request order is deferred to a later plan.
fn load_image(weak: slint::Weak<AppWindow>, caption: Option<String>, path: PathBuf) {
    std::thread::spawn(move || {
        match decode::display_image(&path, MAX_DISPLAY_DIM) {
            Ok(rgba) => {
                let (w, h) = (rgba.width(), rgba.height());
                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    rgba.as_raw(),
                    w,
                    h,
                );
                let _ = weak.upgrade_in_event_loop(move |c| {
                    c.set_current_image(slint::Image::from_rgba8(buffer));
                    if let Some(cap) = caption {
                        c.set_caption(cap.into());
                    }
                    c.set_status_text("".into());
                });
            }
            Err(e) => {
                let msg = format!("Can't display {}: {e}", file_name_of(&path));
                let _ = weak.upgrade_in_event_loop(move |c| c.set_status_text(msg.into()));
            }
        }
    });
}
