//! Thin orchestrator: builds the tray icon/menu ([`crate::tray_ui`]) and
//! then runs the Win32 message loop that keeps it alive, dispatching a
//! click on "Exit" to a clean shutdown, a click on "Show Settings" to
//! lazily opening (and fully tearing down, on close) the egui Settings
//! window ([`crate::settings_ui`]), and a press of the bound borderless
//! hotkey ([`crate::hotkeys`]) to stripping/restoring the foreground
//! window's border via [`crate::win32`]. "Apply to Window" remains a
//! no-op -- it was never given a mechanic of its own; the tray menu item
//! predates the hotkey-driven mechanic this issue adds.
//!
//! The message loop itself ([`run`]) is OS-facing (pumps real Win32
//! messages) and is not exercised by this crate's automated tests, for
//! the same reason [`crate::tray_ui::Tray::new`] isn't: it needs a live
//! thread message queue that a headless/CI sandbox can't provide.

use crate::hotkeys::{Hotkeys, HotkeyAction};
use crate::profile_store;
use crate::region_calc::resolve_region;
use crate::settings_ui::{self, primary_monitor_geometry};
use crate::tray_ui::{Tray, TrayAction, TrayUiError};
use crate::win32::{self, RestoreState};
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use std::path::{Path, PathBuf};
use tray_icon::menu::MenuEvent;
use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, TranslateMessage, MSG};

/// Starts GameFrame: shows only the tray icon (no window), registers the
/// bound global hotkeys, and blocks, pumping the Windows message loop both
/// the tray icon and the hotkeys need, until "Exit" is selected. Returns
/// once the tray icon (and its hidden window) has been cleanly torn down.
pub fn run() -> Result<(), TrayUiError> {
    let tray = Tray::new()?;
    let config_dir = config_dir();
    let hotkeys = register_hotkeys(&config_dir);

    run_event_loop(&tray, hotkeys.as_ref(), &config_dir);
    Ok(())
}

/// Loads the config and registers its bound hotkeys with the OS. A failure
/// at either step (a malformed config file, an OS registration failure --
/// e.g. another application already holds the exact key combination) is
/// reported to stderr rather than treated as fatal: GameFrame still starts
/// with no hotkey wired up, matching the fresh-install "nothing bound yet"
/// case where pressing keys is simply inert.
fn register_hotkeys(config_dir: &Path) -> Option<Hotkeys> {
    let config = match profile_store::load(config_dir) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("failed to load config for hotkey registration: {e}");
            return None;
        }
    };

    match Hotkeys::register(&config.hotkeys) {
        Ok(hotkeys) => Some(hotkeys),
        Err(e) => {
            eprintln!("failed to register global hotkeys: {e}");
            None
        }
    }
}

/// Pumps the Win32 message loop on this thread. Each dispatched message
/// may cause the tray/menu to enqueue an event on
/// [`MenuEvent::receiver`]; draining it after every dispatched message
/// lets us react to a click without a separate polling thread. The same
/// pattern applies to global hotkey presses, delivered on
/// [`GlobalHotKeyEvent::receiver`] by the `global-hotkey` crate.
fn run_event_loop(tray: &Tray, hotkeys: Option<&Hotkeys>, config_dir: &Path) {
    // Remembers the foreground window's pre-toggle style/region across
    // presses of the borderless hotkey -- `None` means the window (if any)
    // is currently in its normal bordered state; `Some` means the last
    // press stripped it and this is what a second press should restore.
    let mut toggle_state: Option<RestoreState> = None;

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        // 0 (WM_QUIT) or -1 (error) both mean: stop pumping.
        let ret = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if ret <= 0 {
            return;
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            match tray.ids().action_for(event.id()) {
                Some(TrayAction::Exit) => {
                    // Returning here unwinds back to `run()`, which then
                    // drops its owned `tray`, removing the OS tray icon
                    // and destroying its hidden window -- the entire
                    // clean-shutdown path for the tray UI.
                    return;
                }
                Some(TrayAction::ShowSettings) => {
                    // Synchronous/modal-style for this MVP: opening
                    // Settings blocks this thread until the Settings
                    // window closes, at which point the egui/wgpu
                    // context it used is fully dropped and we resume
                    // pumping tray messages right where we left off.
                    if let Err(e) = open_settings(config_dir) {
                        eprintln!("failed to open Settings: {e}");
                    }
                }
                Some(TrayAction::ApplyToWindow) => {
                    // No-op -- this tray menu item has never had a
                    // mechanic of its own; the hotkey-driven toggle below
                    // is what issue #6 actually wires up.
                }
                None => {}
            }
        }

        while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            // Only act on the key-down edge; the crate also emits a
            // `Released` event on key-up (see `global_hotkey`'s Windows
            // implementation), which would otherwise double-toggle.
            if event.state() != HotKeyState::Pressed {
                continue;
            }

            // With no hotkey bound (fresh install) `hotkeys` is `None`
            // (nothing was registered, so no event can arrive for it
            // anyway) or resolves to no action for `event.id()` --
            // either way, nothing happens here and nothing panics.
            let action = hotkeys.and_then(|h| h.action_for(event.id()));
            if let Some(HotkeyAction::ToggleBorderless) = action {
                if handle_toggle_borderless(config_dir, &mut toggle_state) {
                    // GameFrame relaunched itself elevated to complete a
                    // manipulation that failed for lack of permissions;
                    // this (unelevated) instance's job is done.
                    return;
                }
            }
        }
    }
}

/// Reacts to a press of the borderless-toggle hotkey: resolves the active
/// preset's region against the primary monitor and applies (or reverts)
/// the border-strip via [`win32::toggle_borderless_on_foreground_window`].
///
/// Returns `true` if GameFrame just relaunched itself elevated and this
/// process should exit (see [`win32::relaunch_elevated`]); `false` in every
/// other case (success, or a non-elevation failure that was just logged).
///
/// Reloads the config from disk on every press (rather than caching it
/// across the whole run) so a preset added/edited via Settings since the
/// last press is picked up immediately -- the trade-off is a small,
/// infrequent disk read on a user-initiated action, which is cheap.
/// Re-registering the hotkey binding itself if it's changed via Settings is
/// out of scope for this slice; that takes a restart to pick up.
fn handle_toggle_borderless(config_dir: &Path, toggle_state: &mut Option<RestoreState>) -> bool {
    let config = match profile_store::load(config_dir) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("borderless toggle: failed to load config: {e}");
            return false;
        }
    };

    let Some(preset) = win32::active_preset(&config.presets) else {
        eprintln!("borderless toggle: no presets configured");
        return false;
    };

    let monitor = primary_monitor_geometry();
    let target_region = match resolve_region(preset, &monitor) {
        Ok(region) => region,
        Err(e) => {
            eprintln!("borderless toggle: failed to resolve the active preset's region: {e}");
            return false;
        }
    };

    if let Err(e) = win32::toggle_borderless_on_foreground_window(toggle_state, target_region) {
        if e.is_access_denied() {
            eprintln!("borderless toggle failed (access denied); requesting elevation");
            match win32::relaunch_elevated() {
                Ok(()) => return true,
                Err(relaunch_err) => eprintln!("failed to relaunch elevated: {relaunch_err}"),
            }
        } else {
            eprintln!("borderless toggle failed: {e}");
        }
    }

    false
}

/// Opens the Settings window at `config_dir`. See
/// [`settings_ui::run_settings_window`] for what "opens" entails (blocks
/// until closed, persists on Save).
fn open_settings(config_dir: &Path) -> Result<(), settings_ui::SettingsUiError> {
    settings_ui::run_settings_window(config_dir)
}

/// The real `%APPDATA%\GameFrame` directory GameFrame persists its config
/// under. Falls back to a `GameFrame` directory relative to the current
/// working directory in the (in-practice-never-on-real-Windows) case where
/// the `APPDATA` environment variable isn't set, rather than panicking.
fn config_dir() -> PathBuf {
    match std::env::var("APPDATA") {
        Ok(appdata) => PathBuf::from(appdata).join("GameFrame"),
        Err(_) => PathBuf::from("GameFrame"),
    }
}
