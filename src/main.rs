#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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
mod tray_ui;
mod win32;
mod window_filter;
mod zone_editor;
mod zone_overlay;

fn main() {
    if !single_instance::acquire() {
        return;
    }
    if let Err(e) = app::run() {
        eprintln!("gameframe failed to start: {e}");
        std::process::exit(1);
    }
}
