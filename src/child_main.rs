//! Entry point of the short-lived child processes, `gameframe.exe --settings`
//! and `--zone-editor` (see [`crate::settings_link`] for why they exist).
//!
//! A child shows one window, tells the tray process what it needs to know over
//! the link, and exits when the window closes, which unloads the GPU driver
//! with it (#186). OS/GPU-facing: checked manually.

use crate::settings_link::{
    ChildArgs, ChildKind, ChildLink, ChildMsg, EXIT_ZONE_SAVED, connect_stdio,
};
use crate::{app, logging, profile_store, settings_ui, zone_overlay};
use std::path::Path;

/// Runs the window `args` asks for and returns the process exit code.
pub fn run(args: ChildArgs) -> i32 {
    let config_dir = app::config_dir();
    // Same log file as the tray process: tag the lines of this one.
    logging::set_process_tag(match args.kind {
        ChildKind::Settings => "settings",
        ChildKind::ZoneEditor => "zone-editor",
    });
    logging::init(&config_dir);
    // The zone editor can hide the taskbar, and a kill or abort here would
    // leave it hidden: keep a marker of this process's own (the tray process
    // restores from it when this process ends) and restore on a panic.
    crate::taskbar_recovery::init_child(&config_dir);
    crate::taskbar_recovery::install_panic_hook();
    // The tray process writes the config file while this window is open.
    profile_store::disable_cache();
    start_hang_watchdog(&config_dir);
    let link = args.linked.then(connect_stdio);
    let code = match args.kind {
        ChildKind::Settings => run_settings(&config_dir, link),
        ChildKind::ZoneEditor => run_zone_editor(&config_dir),
    };
    // Safety net: whatever this process still has hidden, it shows again.
    crate::taskbar::restore_all_for_exit();
    logging::flush();
    code
}

/// Starts the hang watchdog (#128) when the saved `log_ui_hangs` asks for it. The
/// tray process's watchdog only sees that this window is unresponsive, not where
/// it stalled; this one logs the breadcrumb, in the tagged lines of this process.
/// If the config is unreadable it starts, as in the tray process (a diagnostic
/// fails on).
fn start_hang_watchdog(config_dir: &Path) {
    let enabled = match profile_store::load(config_dir) {
        Ok(config) => config.log_ui_hangs,
        Err(e) => {
            log!("hang watchdog: could not read the config: {e}");
            true
        }
    };
    crate::watchdog::set_enabled_in_child(enabled);
}

fn run_settings(config_dir: &Path, link: Option<&'static ChildLink>) -> i32 {
    let (hotkey_reports, game_cursor) = link.and_then(ChildLink::wait_init).unwrap_or_default();
    if let Some(image) = game_cursor {
        crate::cursor_overlay::set_last_game_cursor(image);
    }
    let mut on_tick = |context| {
        crate::watchdog::step("settings: tick (link)");
        let enabled = link.and_then(|link| link.tick(context));
        // Back in Settings' frame loop (rendering, window messages): a stall
        // from here until the next tick happened in Settings itself (#128).
        crate::watchdog::step("settings: frame loop");
        enabled
    };
    // Without a link there is no tray process to re-apply the Zone.
    let mut reapply = || {
        crate::watchdog::step("settings: waiting for the live re-apply");
        link.map(ChildLink::request_reapply).unwrap_or_default()
    };
    crate::watchdog::step("settings: opening window");
    // Window and GPU setup can take a while and are not a hang: idle until
    // Settings' first `ui()` beats.
    crate::watchdog::idle();
    match settings_ui::run_settings_window(config_dir, &hotkey_reports, &mut on_tick, &mut reapply)
    {
        Ok(()) => 0,
        Err(e) => {
            log!("failed to open Settings: {e}");
            match link {
                Some(link) => link.send(&ChildMsg::Notify(e.notice().to_string())),
                None => crate::notice::notify_blocking(e.notice()),
            }
            1
        }
    }
}

/// The first-run Zone editor: saves the Zone itself, then exits with
/// [`EXIT_ZONE_SAVED`] so the tray process can tell the user what comes next.
fn run_zone_editor(config_dir: &Path) -> i32 {
    let mode = match profile_store::load(config_dir) {
        Ok(config) => config.taskbar_hide_mode,
        Err(e) => {
            log!("failed to load config for the zone editor: {e}");
            return 1;
        }
    };
    // No tray or hotkeys exist yet at first run, so there is nothing to tick;
    // the link only delivers a close request (through `quit_pending`).
    crate::watchdog::step("zone editor: opening window");
    crate::watchdog::idle();
    match zone_overlay::run_zone_editor_overlay(None, mode, &mut || {
        crate::watchdog::step("zone editor: frame loop");
    }) {
        Ok(Some(zone)) => match settings_ui::persist_zone(config_dir, Some(zone)) {
            Ok(_) => EXIT_ZONE_SAVED,
            Err(e) => {
                log!("failed to save the zone: {e}");
                1
            }
        },
        Ok(None) => 0,
        Err(e) => {
            log!("failed to open the zone editor overlay: {e}");
            1
        }
    }
}
