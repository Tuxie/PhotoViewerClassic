slint::include_modules!();

use slint::ComponentHandle;
use std::path::PathBuf;
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

    // Shared navigation cursor: mutated from UI-thread callbacks, filled by the
    // background directory scan (hence Arc<Mutex<_>>).
    let nav = Arc::new(Mutex::new(imageset::ImageSet::empty()));

    ui.on_next_image({
        let nav = nav.clone();
        let weak = ui.as_weak();
        move || {
            let next = nav.lock().unwrap().advance();
            if let Some(p) = next {
                load_image(weak.clone(), nav.clone(), p);
            }
        }
    });
    ui.on_prev_image({
        let nav = nav.clone();
        let weak = ui.as_weak();
        move || {
            let prev = nav.lock().unwrap().retreat();
            if let Some(p) = prev {
                load_image(weak.clone(), nav.clone(), p);
            }
        }
    });

    match initial {
        Some(path) => {
            ui.set_status_text("Loading…".into());
            // 1) Show the requested image immediately.
            load_image(ui.as_weak(), nav.clone(), path.clone());
            // 2) Scan its directory in the background, then refresh the caption with index.
            let nav_scan = nav.clone();
            let weak = ui.as_weak();
            std::thread::spawn(move || {
                let set = imageset::ImageSet::from_file(&path);
                *nav_scan.lock().unwrap() = set;
                let cap = caption(&nav_scan);
                let _ = weak.upgrade_in_event_loop(move |c| c.set_caption(cap.into()));
            });
        }
        None => ui.set_status_text("No image. Usage: photoviewer <path>".into()),
    }

    ui.run()
}

/// "(i/N) name" for the current cursor, or "" if the set is empty.
fn caption(nav: &Arc<Mutex<imageset::ImageSet>>) -> String {
    let g = nav.lock().unwrap();
    match g.current() {
        Some(p) => {
            let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            format!("({}/{}) {}", g.position() + 1, g.len(), name)
        }
        None => String::new(),
    }
}

/// Decode `path` on a worker thread (single file read -> orient -> downscale) and
/// push the image + caption back to the UI. Errors set the status text instead.
fn load_image(weak: slint::Weak<AppWindow>, nav: Arc<Mutex<imageset::ImageSet>>, path: PathBuf) {
    std::thread::spawn(move || {
        match decode::display_image(&path, MAX_DISPLAY_DIM) {
            Ok(rgba) => {
                let (w, h) = (rgba.width(), rgba.height());
                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    rgba.as_raw(),
                    w,
                    h,
                );
                let cap = caption(&nav);
                let _ = weak.upgrade_in_event_loop(move |c| {
                    c.set_current_image(slint::Image::from_rgba8(buffer));
                    c.set_caption(cap.into());
                    c.set_status_text("".into());
                });
            }
            Err(e) => {
                let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let msg = format!("Can't display {name}: {e}");
                let _ = weak.upgrade_in_event_loop(move |c| c.set_status_text(msg.into()));
            }
        }
    });
}
