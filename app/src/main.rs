slint::include_modules!();

use slint::ComponentHandle;
use std::path::PathBuf;

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

    match initial {
        Some(path) => load_image(ui.as_weak(), path),
        None => ui.set_status_text("No image. Usage: photoviewer <path>".into()),
    }

    ui.run()
}

/// Decode `path` on a worker thread and push the oriented, display-sized image
/// (and a caption) back to the UI thread. Errors set the status text instead.
fn load_image(weak: slint::Weak<AppWindow>, path: PathBuf) {
    std::thread::spawn(move || {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match decode::decode_to_rgba8(&path) {
            Ok(rgba) => {
                let orientation = decode::read_orientation(&path).unwrap_or(1);
                let oriented = decode::apply_orientation(rgba, orientation);
                let display = decode::downscale_to_fit(oriented, 4096);
                let (dw, dh) = (display.width(), display.height());
                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    display.as_raw(),
                    dw,
                    dh,
                );
                let _ = weak.upgrade_in_event_loop(move |c| {
                    c.set_current_image(slint::Image::from_rgba8(buffer));
                    c.set_caption(name.into());
                    c.set_status_text("".into());
                });
            }
            Err(e) => {
                let msg = format!("Can't display {name}: {e}");
                let _ = weak.upgrade_in_event_loop(move |c| c.set_status_text(msg.into()));
            }
        }
    });
}
