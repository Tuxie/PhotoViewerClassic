slint::include_modules!();

use slint::ComponentHandle;

fn main() -> Result<(), slint::PlatformError> {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("femtovg".into())
        .select()?;

    let ui = AppWindow::new()?;
    ui.on_quit(|| {
        let _ = slint::quit_event_loop();
    });
    ui.run()
}
