//! The Settings window itself: viewport sizing, the eframe window per
//! session and the exit the user chose.

use super::{
    SettingsApp, SettingsState, SettingsUiError, UpdateCheck, UpdateInstall, apply_saved_changes,
    persist_zone,
};
use crate::autostart;
use crate::hotkeys::BindingReport;
use crate::instance_channel::SETTINGS_TITLE;
use crate::profile_store;
use crate::profile_store::Config;
use crate::region_calc::MonitorGeometry;
use crate::zone_editor::pixels_to_points;
use crate::zone_overlay;
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

/// The Settings window's preferred inner size in points; [`settings_viewport`]
/// shrinks it to fit a small work area and centres it.
const SETTINGS_WINDOW_SIZE: [f32; 2] = [560.0, 800.0];

/// Points reserved below the window's inner size for the title bar and frame,
/// so the whole outer window fits in the work area.
const SETTINGS_WINDOW_CHROME: f32 = 48.0;

/// Smallest height the Settings window is shrunk to, unless the work area is
/// smaller still.
const SETTINGS_WINDOW_MIN_HEIGHT: f32 = 300.0;

/// The Settings window's inner size and top-left position, both in egui
/// points.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SettingsViewport {
    size: [f32; 2],
    position: [f32; 2],
}

/// Computes the Settings window's size and position in egui points so it opens
/// fully inside `work_area` (physical pixels, excluding the taskbar) at the
/// monitor scale factor `scale`. The fixed [`SETTINGS_WINDOW_SIZE`] is in
/// points and is shrunk to fit; the window is centered in the work area. Pure
/// arithmetic, no OS dependency.
fn settings_viewport(work_area: &MonitorGeometry, scale: f32) -> SettingsViewport {
    let area_x = pixels_to_points(work_area.x, scale);
    let area_y = pixels_to_points(work_area.y, scale);
    let area_width = pixels_to_points(work_area.width, scale);
    let area_height = pixels_to_points(work_area.height, scale);

    let [wanted_width, wanted_height] = SETTINGS_WINDOW_SIZE;
    let max_height = (area_height - SETTINGS_WINDOW_CHROME).max(0.0);
    let width = wanted_width.min(area_width);
    let height = wanted_height
        .min(max_height)
        .max(SETTINGS_WINDOW_MIN_HEIGHT.min(max_height));

    SettingsViewport {
        size: [width, height],
        position: [
            area_x + (area_width - width) / 2.0,
            area_y + (area_height - height - SETTINGS_WINDOW_CHROME).max(0.0) / 2.0,
        ],
    }
}

/// The primary monitor's work area (the monitor minus the taskbar) in physical
/// pixels, falling back to the full monitor if the query fails.
///
/// OS-facing -- not exercised by the automated tests.
fn primary_work_area() -> MonitorGeometry {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETWORKAREA, SystemParametersInfoW};

    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // Safety: `SPI_GETWORKAREA` writes one `RECT` through the pointer, which
    // outlives the call.
    let ok =
        unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut rect as *mut RECT).cast(), 0) };
    if ok == 0 || rect.right <= rect.left || rect.bottom <= rect.top {
        log!("settings: SPI_GETWORKAREA failed; using the full monitor");
        return crate::win32::primary_monitor_geometry();
    }
    MonitorGeometry {
        x: rect.left,
        y: rect.top,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
    }
}

/// What closed the Settings window: a completed Save (carrying the
/// [`Config`] to persist), a Cancel/close-without-saving, or a request to
/// open the zone-editor overlay for the Zone (carrying the current
/// in-progress [`SettingsState`], plus this session's current pending
/// live-re-apply status messages ([`SettingsApp::pending_status_messages`])
/// so neither is lost while the overlay is open).
///
/// [`run_settings_window`] loops on this: `EditZone` re-opens the
/// Settings window (with the overlay's result, and the carried-forward
/// status messages, folded back in) rather than ending the session, since
/// opening the zone editor is not itself a decision to close Settings.
pub(super) enum SettingsExit {
    /// The config to persist, plus the "Start with Windows" choice (which
    /// lives in the registry, not in [`Config`]).
    /// The `bool` after the config is [`SettingsState::cursor_enabled_seen`].
    Saved(Config, bool, bool),
    Cancelled,
    /// Carries the in-progress [`SettingsState`] plus this session's current
    /// pending live-re-apply status messages ([`SettingsApp::pending_status_messages`])
    /// -- so a message surfaced by a Zone commit made
    /// just before "Edit Zone" was clicked isn't lost when this
    /// session's `SettingsApp` is torn down; [`run_settings_window`] carries
    /// it forward as the next session's initial status messages.
    EditZone(SettingsState, Vec<String>),
}

/// Which window's frame loop is ticking the app's event handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TickContext {
    /// The Settings window is open.
    Settings,
    /// Settings is waiting for the next key to bind; hotkey presses are dropped.
    SettingsCapturing,
    /// The zone-editor overlay is open (Settings is closed meanwhile).
    ZoneEditor,
}

/// Opens the Settings window, blocking until it's closed, then returns.
///
/// Loads the current config from `config_dir` (via [`profile_store::load`]),
/// lets the user edit the Zone and hotkey bindings, and -- only if the user
/// clicks "Save" -- persists the fields the user changed via
/// [`profile_store::update`] (see [`apply_saved_changes`]) before returning.
/// Closing the window any other way (the "Cancel" button, the window's own
/// close button) discards the edits -- except the Zone, which Save/Cancel
/// don't govern: saving in the zone editor overlay (and "Clear") persists it
/// immediately, so the disk and the windows it was live-applied to never
/// disagree.
///
/// Clicking "Draw Zone"/"Edit Zone" opens the zone-editor overlay
/// ([`zone_overlay::run_zone_editor_overlay`]) *sequentially* rather than
/// nesting it inside the Settings window's own event loop -- egui/winit
/// don't support one native event loop starting another while it's still
/// running. Concretely: the Settings window closes, the overlay runs to
/// completion, its result (if any) is folded into the in-progress state,
/// and Settings re-opens with that updated state -- the same
/// close-then-reopen sequencing, since the overlay and Settings each own a
/// native event loop of this child process.
///
/// Also threads the non-blocking live-re-apply status messages across that
/// same close-then-reopen boundary: `status_messages` starts empty, is
/// replaced wholesale by whatever [`SettingsExit::EditZone`] carried out of
/// the just-closed session (either its untouched carried-forward messages,
/// or a fresh set if the overlay's save triggered a live re-apply), and is
/// handed to the next [`run_settings_frame`]
/// call as its `initial_status_messages` -- so a status line surfaced right
/// before "Edit Zone" was clicked survives the round trip through
/// the overlay instead of vanishing with the torn-down `SettingsApp`.
///
/// Runs in the short-lived `--settings` child process (see
/// [`crate::child_main`]), so the GPU driver dies with the window instead of
/// staying loaded in the tray process (#186). `on_tick` is how the tray process
/// is kept informed, and `reapply` live re-applies the saved Zone to the
/// running tracked games and returns the status lines for the ones that failed:
/// both go to the tray process, which owns the per-window state (it reads the
/// games from the config file itself).
///
/// OS/GPU-facing: constructs a real egui/wgpu context for the lifetime of
/// each iteration and fully drops it before the next (or before returning).
/// Not exercised by this crate's automated tests -- see module docs.
pub fn run_settings_window(
    config_dir: &Path,
    hotkey_reports: &[BindingReport],
    on_tick: &mut dyn FnMut(TickContext) -> Option<bool>,
    reapply: &mut dyn FnMut() -> Vec<String>,
) -> Result<(), SettingsUiError> {
    let config = profile_store::load(config_dir).map_err(SettingsUiError::Config)?;
    let mut state = SettingsState::from_config(&config);
    // What Save compares the user's edits against (see [`apply_saved_changes`]).
    let opened = config;
    state.set_hotkey_reports(hotkey_reports);
    let started_with_windows = autostart::is_enabled();
    state.start_with_windows = started_with_windows;
    let monitor = crate::win32::primary_monitor_geometry();
    let mut status_messages: Vec<String> = Vec::new();

    loop {
        match run_settings_frame(config_dir, state, monitor, status_messages, &mut *on_tick)? {
            SettingsExit::Saved(config, start_with_windows, cursor_seen) => {
                // Keep what the tray and hotkeys changed on disk while
                // Settings was open. The merge and the write run under the
                // store's cross-process lock (the tray process writes the
                // file too), and the read behind it always goes to the file.
                // A failure (re-read or write) is surfaced rather than
                // retried unmerged outside that lock: if the disk cannot be
                // read it can hardly be written.
                let saved = profile_store::update(config_dir, |on_disk| {
                    apply_saved_changes(&opened, config, cursor_seen, on_disk);
                })
                .map_err(SettingsUiError::Config);
                // Even after a failed write the tray process must not trust
                // its cached copy.
                crate::settings_link::config_changed();
                saved?;
                // Only touch the registry on an actual change, so saving
                // unrelated settings never re-points an existing entry at
                // whichever exe happens to be running (e.g. a dev build).
                if start_with_windows != started_with_windows {
                    autostart::set_enabled(start_with_windows)
                        .map_err(SettingsUiError::Autostart)?;
                }
                return Ok(());
            }
            SettingsExit::Cancelled => return Ok(()),
            SettingsExit::EditZone(mut latest_state, latest_status_messages) => {
                // Carried forward as-is unless the overlay actually commits a
                // new zone below (a live re-apply only fires on an accepted
                // save) -- cancelling/failing to open the overlay is not
                // itself a commit, so whatever status messages were already
                // showing must survive it.
                status_messages = latest_status_messages;
                // The overlay keeps the tray and hotkeys working too; a
                // game-cursor toggle made meanwhile is folded into the state.
                let mut cursor_toggled = None;
                // Before the overlay opens: the taskbar hotkey must be held
                // back from the first moment, not from the first tick.
                crate::settings_link::report_context(TickContext::ZoneEditor);
                let mut tick = || {
                    if let Some(enabled) = on_tick(TickContext::ZoneEditor) {
                        cursor_toggled = Some(enabled);
                    }
                };
                let overlay = zone_overlay::run_zone_editor_overlay(
                    latest_state.zone,
                    latest_state.taskbar_hide_mode,
                    &mut tick,
                );
                if let Some(enabled) = cursor_toggled {
                    latest_state.apply_external_cursor_enabled(enabled);
                }
                match overlay {
                    Ok(Some(zone)) => {
                        // Persist first, then apply: the Zone on disk (read by
                        // the drift and launch re-apply paths) must never
                        // disagree with the Zone the windows are placed in.
                        match persist_zone(config_dir, Some(zone)) {
                            Ok(_) => {
                                latest_state.set_zone(zone);
                                status_messages = reapply();
                            }
                            Err(e) => {
                                log!("failed to save the zone: {e}");
                                status_messages = vec![format!("couldn't save the Zone: {e}")];
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => log!("failed to open the zone editor overlay: {e}"),
                }
                // A Quit that arrived during the overlay ended it; do not
                // reopen Settings, let the main loop exit.
                if crate::instance_channel::quit_pending() {
                    return Ok(());
                }
                state = latest_state;
            }
        }
    }
}

/// Runs a single Settings window session to completion (see
/// [`run_settings_window`]'s doc comment for why this is a separate,
/// re-enterable step rather than one `eframe::run_native` call for the
/// whole function).
///
/// `initial_status_messages` seeds the new [`SettingsApp`]'s
/// [`SettingsApp::pending_status_messages`] -- carried over from
/// the previous session's [`SettingsExit::EditZone`], or empty on the
/// very first call from [`run_settings_window`]. Any close other than an
/// explicit Save, Cancel, or "Draw Zone"/"Edit Zone" click -- concretely, the
/// window's own close button -- is treated as [`SettingsExit::Cancelled`]:
/// this function's documented "discards the edits" contract for any
/// non-Save close.
fn run_settings_frame(
    config_dir: &Path,
    state: SettingsState,
    monitor: MonitorGeometry,
    initial_status_messages: Vec<String>,
    on_tick: &mut dyn FnMut(TickContext) -> Option<bool>,
) -> Result<SettingsExit, SettingsUiError> {
    let exit: Rc<RefCell<Option<SettingsExit>>> = Rc::new(RefCell::new(None));
    let exit_for_app = Rc::clone(&exit);
    let config_dir = config_dir.to_path_buf();

    // Window geometry is not persisted, so the window opens centered in the
    // work area every time, sized in points via the monitor's scale factor.
    let viewport = settings_viewport(&primary_work_area(), zone_overlay::primary_monitor_scale());
    let native_options = eframe::NativeOptions {
        persist_window: false,
        viewport: egui::ViewportBuilder::default()
            .with_title(SETTINGS_TITLE)
            .with_inner_size(viewport.size)
            .with_position(viewport.position)
            .with_icon(window_icon()),
        ..Default::default()
    };

    eframe::run_native(
        SETTINGS_TITLE,
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(SettingsApp {
                config_dir,
                state,
                monitor,
                exit: exit_for_app,
                pending_status_messages: initial_status_messages,
                keyboard_hook: None,
                cursor_previews: None,
                update_check: UpdateCheck::Idle,
                update_install: UpdateInstall::Idle,
                on_tick,
            }))
        }),
    )
    .map_err(SettingsUiError::Window)?;

    // A close via the window's own close button (rather than Save/Cancel/Edit
    // zone) leaves `exit` empty -- treat that the same as an explicit
    // Cancel, matching this function's documented "discards the edits"
    // contract for any non-Save close.
    Ok(exit.borrow_mut().take().unwrap_or(SettingsExit::Cancelled))
}

/// GameFrame's corner-bracket mark from [`gameframe::icon`], rendered at
/// 256x256 as recommended by [`egui::IconData`]'s own doc comment, for the
/// Settings window's title-bar/taskbar icon.
fn window_icon() -> egui::IconData {
    const SIZE: u32 = 256;
    egui::IconData {
        rgba: gameframe::icon::mark_rgba(SIZE),
        width: SIZE,
        height: SIZE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work_area_1440p() -> MonitorGeometry {
        // 2560x1440 minus a 48 px taskbar at the bottom.
        MonitorGeometry {
            x: 0,
            y: 0,
            width: 2560,
            height: 1392,
        }
    }

    /// The whole outer window (inner size plus chrome) lies inside the work
    /// area, in points.
    fn assert_fits(view: SettingsViewport, work_area: &MonitorGeometry, scale: f32) {
        let area_x = pixels_to_points(work_area.x, scale);
        let area_y = pixels_to_points(work_area.y, scale);
        let area_w = pixels_to_points(work_area.width, scale);
        let area_h = pixels_to_points(work_area.height, scale);
        assert!(view.position[0] >= area_x - 0.01, "{view:?}");
        assert!(view.position[1] >= area_y - 0.01, "{view:?}");
        assert!(
            view.position[0] + view.size[0] <= area_x + area_w + 0.01,
            "{view:?}"
        );
        assert!(
            view.position[1] + view.size[1] + SETTINGS_WINDOW_CHROME <= area_y + area_h + 0.01,
            "{view:?}"
        );
    }

    #[test]
    fn settings_viewport_at_100_percent_keeps_the_full_size_and_centers() {
        let view = settings_viewport(&work_area_1440p(), 1.0);

        assert_eq!(view.size, [560.0, 800.0]);
        assert_eq!(view.position, [1000.0, 272.0]);
        assert_fits(view, &work_area_1440p(), 1.0);
    }

    #[test]
    fn settings_viewport_fits_the_work_area_at_125_150_and_200_percent() {
        for scale in [1.25_f32, 1.5, 2.0] {
            let view = settings_viewport(&work_area_1440p(), scale);
            assert_fits(view, &work_area_1440p(), scale);
        }
    }

    #[test]
    fn settings_viewport_fits_at_150_percent_and_shrinks_at_200_percent_on_1440p() {
        // 1392 px / 1.5 = 928 pt of work area; 800 pt + chrome still fits.
        let view = settings_viewport(&work_area_1440p(), 1.5);
        assert_eq!(view.size, [560.0, 800.0]);

        // At 200 %: 696 pt of work area -> height shrinks to 648.
        let view = settings_viewport(&work_area_1440p(), 2.0);
        assert_eq!(view.size, [560.0, 648.0]);
    }

    #[test]
    fn settings_viewport_accounts_for_a_work_area_with_a_non_zero_origin() {
        // Taskbar on the left and top.
        let work_area = MonitorGeometry {
            x: 100,
            y: 50,
            width: 1820,
            height: 1030,
        };
        let view = settings_viewport(&work_area, 1.25);
        assert!(view.position[0] >= 80.0);
        assert!(view.position[1] >= 40.0);
        assert_fits(view, &work_area, 1.25);
    }

    #[test]
    fn settings_viewport_clamps_to_a_work_area_smaller_than_the_window() {
        let work_area = MonitorGeometry {
            x: 0,
            y: 0,
            width: 400,
            height: 250,
        };
        let view = settings_viewport(&work_area, 1.0);
        assert_eq!(view.size[0], 400.0);
        assert_fits(view, &work_area, 1.0);
    }
}
