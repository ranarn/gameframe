#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// First, so the `log!` macro is in scope in every module below.
#[macro_use]
mod logging;

mod about_ui;
mod app;
mod autostart;
mod cursor_overlay;
mod cursor_style;
mod hotkeys;
mod http;
mod instance_channel;
mod keyboard_hook;
mod launch_watcher;
mod message_box;
mod profile_store;
mod region_calc;
mod settings_ui;
mod single_instance;
mod taskbar;
mod taskbar_recovery;
mod tray_ui;
mod update_install;
mod update_ui;
mod updater;
mod version;
mod watchdog;
mod win32;
mod window_filter;
mod zone_editor;
mod zone_overlay;
mod zone_taskbar;

fn main() {
    if single_instance::is_quit_request(std::env::args()) {
        // Never takes the instance: just asks the running one to exit. One
        // attempt only, so "not running" returns at once.
        logging::init(&app::config_dir());
        if instance_channel::send_within(instance_channel::Command::Quit, std::time::Duration::ZERO)
        {
            log!("--quit: asked the running instance to quit");
        } else {
            log!("--quit: no running instance reachable");
        }
        return;
    }
    let wait = single_instance::is_elevated_relaunch(std::env::args())
        .then_some(single_instance::RELAUNCH_WAIT);
    if wait.is_some() {
        // Only an elevated relaunch can time out, so it opens the log this
        // early: the release build has no console, and a timeout would
        // otherwise be lost. (A normal second launch opens it only to log
        // its handoff, below.)
        logging::init(&app::config_dir());
    }
    let (owned, note) = single_instance::acquire(wait);
    if let Some(note) = note {
        // Logged here, not in `acquire`: a normal launch hasn't opened the log
        // yet at that point. `init` is idempotent, so the paths below and
        // `app::run` can call it again.
        logging::init(&app::config_dir());
        log!("{note}");
    }
    if !owned {
        // A normal second launch hands off to the running instance; an elevated
        // relaunch that timed out has nobody to talk to.
        if wait.is_none() {
            logging::init(&app::config_dir());
            if instance_channel::send(instance_channel::Command::OpenSettings) {
                log!("second launch: asked the running instance to open Settings; exiting");
            } else {
                log!("second launch: could not reach the running instance; exiting");
            }
        }
        logging::flush();
        return;
    }
    // Flushed whatever `run` returns, so no exit of the running instance loses
    // pending repeat counts (a `--quit` request logs nothing that repeats).
    let result = app::run();
    logging::flush();
    if let Err(e) = result {
        log!("gameframe failed to start: {e}");
        std::process::exit(1);
    }
}
