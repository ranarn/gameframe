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
    logging::init(&config_dir);
    // The tray process writes the config file while this window is open.
    profile_store::disable_cache();
    let link = args.linked.then(connect_stdio);
    let code = match args.kind {
        ChildKind::Settings => run_settings(&config_dir, link),
        ChildKind::ZoneEditor => run_zone_editor(&config_dir),
    };
    logging::flush();
    code
}

fn run_settings(config_dir: &Path, link: Option<&'static ChildLink>) -> i32 {
    let (hotkey_reports, game_cursor) = link.and_then(ChildLink::wait_init).unwrap_or_default();
    if let Some(image) = game_cursor {
        crate::cursor_overlay::set_last_game_cursor(image);
    }
    let mut on_tick = |context| link.and_then(|link| link.tick(context));
    // Without a link there is no tray process to re-apply the Zone.
    let mut reapply = |_games: &[profile_store::TrackedGame]| {
        link.map(ChildLink::request_reapply).unwrap_or_default()
    };
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
    match zone_overlay::run_zone_editor_overlay(None, mode, &mut || {}) {
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
