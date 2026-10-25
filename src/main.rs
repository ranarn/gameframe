#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// First, so the `log!` macro is in scope in every module below.
#[macro_use]
mod logging;

mod app;
mod autostart;
mod cursor_overlay;
mod cursor_style;
mod hotkeys;
mod keyboard_hook;
mod launch_watcher;
mod profile_store;
mod region_calc;
mod settings_ui;
mod single_instance;
mod taskbar_recovery;
mod tray_ui;
mod win32;
mod window_filter;
mod zone_editor;
mod zone_overlay;

fn main() {
    let wait = single_instance::is_elevated_relaunch(std::env::args()).then_some(single_instance::RELAUNCH_WAIT);
    if wait.is_some() {
        // Only an elevated relaunch can time out, so only it opens the log
        // this early: the release build has no console, and a timeout would
        // otherwise be lost. A normal second launch leaves the log alone.
        logging::init(&app::config_dir());
    }
    if !single_instance::acquire(wait) {
        return;
    }
    if let Err(e) = app::run() {
        log!("gameframe failed to start: {e}");
        std::process::exit(1);
    }
}
