mod app;
mod hotkeys;
mod launch_watcher;
mod profile_store;
mod region_calc;
mod settings_ui;
mod tray_ui;
mod win32;
mod window_filter;
mod zone_editor;
mod zone_overlay;

fn main() {
    if let Err(e) = app::run() {
        eprintln!("gameframe failed to start: {e}");
        std::process::exit(1);
    }
}
