//! Thin orchestrator: builds the tray icon/menu ([`crate::tray_ui`]) and
//! then runs the Win32 message loop that keeps it alive, dispatching a
//! click on "Exit" to a clean shutdown, a click on "Show Settings" to
//! lazily opening (and fully tearing down, on close) the egui Settings
//! window ([`crate::settings_ui`]), a press of the bound borderless hotkey
//! ([`crate::hotkeys`]) to stripping/restoring the foreground window's
//! border via [`crate::win32`], and a press of the bound taskbar-autohide
//! hotkey to toggling taskbar auto-hide via [`crate::win32`]. The two
//! hotkey mechanics are fully independent -- each keeps its own toggle
//! state (`toggle_state` / `taskbar_autohide_state` below), so either can be
//! pressed without the other having been used first. A click on one of the
//! "Apply to Window" submenu's dynamically-listed windows (recognized via
//! [`tray_ui::decode_window_item_id`], distinct from the three fixed
//! top-level menu items) applies the active preset's borderless+reposition
//! treatment to that specific window via
//! [`win32::apply_borderless_to_window`] -- a one-shot action with no
//! remembered restore state, unlike the hotkey path's `toggle_state` (see
//! [`handle_apply_to_window`]).
//!
//! The message loop itself ([`run`]) is OS-facing (pumps real Win32
//! messages) and is not exercised by this crate's automated tests, for
//! the same reason [`crate::tray_ui::Tray::new`] isn't: it needs a live
//! thread message queue that a headless/CI sandbox can't provide.

use crate::hotkeys::{Hotkeys, HotkeyAction};
use crate::profile_store;
use crate::region_calc::resolve_region;
use crate::settings_ui::{self, primary_monitor_geometry};
use crate::tray_ui::{self, Tray, TrayAction, TrayUiError};
use crate::win32::{self, RestoreState, TaskbarAutohideState};
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use std::path::{Path, PathBuf};
use tray_icon::menu::MenuEvent;
use tray_icon::TrayIconEvent;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, TranslateMessage, MSG};

/// Starts GameFrame: shows only the tray icon (no window), registers the
/// bound global hotkeys, and blocks, pumping the Windows message loop both
/// the tray icon and the hotkeys need, until "Exit" is selected. Returns
/// once the tray icon (and its hidden window) has been cleanly torn down.
pub fn run() -> Result<(), TrayUiError> {
    let tray = Tray::new()?;
    let config_dir = config_dir();
    let hotkeys = register_hotkeys(&config_dir);

    run_event_loop(&tray, hotkeys, &config_dir);
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
/// [`GlobalHotKeyEvent::receiver`] by the `global-hotkey` crate, and to tray
/// icon mouse interactions, delivered on [`TrayIconEvent::receiver`] by
/// `tray-icon` -- the latter is what gates the "Apply to Window" submenu's
/// live refresh (see the loop body below).
///
/// `hotkeys` is owned (not borrowed) specifically so the `ShowSettings`
/// branch below can reassign it with a freshly re-registered [`Hotkeys`]
/// after the Settings window closes -- see that branch's comment.
fn run_event_loop(tray: &Tray, mut hotkeys: Option<Hotkeys>, config_dir: &Path) {
    // Remembers the foreground window's pre-toggle style/region across
    // presses of the borderless hotkey -- `None` means the window (if any)
    // is currently in its normal bordered state; `Some` means the last
    // press stripped it and this is what a second press should restore.
    let mut toggle_state: Option<RestoreState> = None;

    // Remembers the taskbar auto-hide toggle's on/off state across presses
    // of its own hotkey -- fully independent of `toggle_state` above (see
    // `win32::TaskbarAutohideState`'s doc comment): neither toggle reads or
    // writes the other's state, so either hotkey works whether or not the
    // other has been pressed yet in this session.
    let mut taskbar_autohide_state = TaskbarAutohideState::default();

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

        // Refreshes the "Apply to Window" submenu's item list only in
        // response to an actual tray-icon mouse interaction (the mouse
        // entering the icon, or a click on it), rather than unconditionally
        // on every pumped message.
        //
        // `Enter` reliably arrives on its own dispatched `WM_MOUSEMOVE`
        // message, strictly before the later `WM_RBUTTONUP` message that
        // opens the context menu (a user must hover onto the tray icon
        // before they can click it) -- so refreshing here, once per
        // `Enter`, completes before the menu is ever shown. `Click` is
        // included too as a defensive fallback (e.g. covers the case where
        // this process only just started pumping messages while the cursor
        // was already resting on the icon, so no `Enter` transition ever
        // fires) even though, per `tray-icon`'s Windows backend, the menu is
        // shown synchronously in the same `WM_RBUTTONUP` handler right after
        // the `Click` event is enqueued -- refreshing on `Click` won't beat
        // that particular menu display, but it does mean the very next
        // open is guaranteed current.
        //
        // This replaces an earlier unconditional "refresh before every
        // dispatched message" approach. The primary, certain reason: `tray-icon`
        // sets a 15ms `WM_TIMER` while the mouse hovers over the icon, which
        // would otherwise have driven a full `EnumWindows` + per-window
        // `GetWindowTextW` + submenu rebuild (real Win32 syscalls) up to ~66
        // times/second just from the cursor resting nearby -- needlessly
        // expensive on this process's one and only message loop. A secondary,
        // less certain consideration: per the documented `GetWindowTextW`
        // contract, a same-process target window causes a `WM_GETTEXT` to be
        // sent via `SendMessage` and can block if that window is stuck,
        // whereas a cross-process target (the common case when enumerating
        // the whole desktop) is documented to read the cached caption text
        // directly instead, specifically so callers don't hang on an
        // unresponsive foreign process -- so the practical hang risk here is
        // narrower than "any hung window on the desktop" (more like "one of
        // GameFrame's own windows getting stuck"), but still worth avoiding
        // at no extra cost. No crate-native "menu about to show" event exists
        // to hook instead (see `Tray::refresh_apply_to_window_menu`'s doc
        // comment).
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if matches!(event, TrayIconEvent::Enter { .. } | TrayIconEvent::Click { .. }) {
                tray.refresh_apply_to_window_menu();
            }
        }

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if let Some(target_hwnd) = tray_ui::decode_window_item_id(event.id()) {
                handle_apply_to_window(config_dir, target_hwnd);
                continue;
            }

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

                    // Settings may have changed the hotkey bindings on
                    // disk (Save persists via `profile_store`) --
                    // re-register from the freshly saved config so a
                    // newly bound/changed/removed hotkey takes effect
                    // immediately, without requiring a full app
                    // restart. Assigning over `hotkeys` drops the old
                    // `Hotkeys` first (its `Drop` impl destroys the old
                    // `GlobalHotKeyManager`'s hidden window, which the OS
                    // uses to automatically unregister every hotkey that
                    // was bound to it) before the new one registers,
                    // so there's no window where both the old and new
                    // bindings are simultaneously held.
                    hotkeys = register_hotkeys(config_dir);
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
            let action = hotkeys.as_ref().and_then(|h| h.action_for(event.id()));
            match action {
                Some(HotkeyAction::ToggleBorderless) => {
                    if handle_toggle_borderless(config_dir, &mut toggle_state) {
                        // GameFrame relaunched itself elevated to complete a
                        // manipulation that failed for lack of permissions;
                        // this (unelevated) instance's job is done.
                        return;
                    }
                }
                Some(HotkeyAction::ToggleTaskbarAutohide) => {
                    handle_toggle_taskbar_autohide(&mut taskbar_autohide_state);
                }
                None => {}
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
/// The hotkey binding itself is re-registered separately, right after the
/// Settings window closes -- see the `ShowSettings` branch in
/// [`run_event_loop`] -- so a changed binding also takes effect without a
/// restart.
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

/// Reacts to a click on one of the "Apply to Window" submenu's
/// dynamically-listed windows ([`tray_ui::decode_window_item_id`]): resolves
/// the active preset's region against the primary monitor (the same pattern
/// as [`handle_toggle_borderless`]) and applies the borderless+reposition
/// treatment to `target_hwnd` -- which may not be the foreground window at
/// all, that's the whole point of this tray-menu path -- via
/// [`win32::apply_borderless_to_window`].
///
/// A one-shot action, not a toggle: there is no remembered restore state for
/// this path (see this module's doc comment for the scope decision), so
/// selecting the same window again simply re-applies the same treatment
/// rather than undoing it.
///
/// Reloads the config from disk on every click, for the same reason
/// [`handle_toggle_borderless`] does.
///
/// Unlike [`handle_toggle_borderless`], a failure here (including an
/// access-denied failure) is only logged, not retried via
/// [`win32::relaunch_elevated`] -- reactive elevation for this path is out
/// of scope for this slice.
fn handle_apply_to_window(config_dir: &Path, target_hwnd: HWND) {
    let config = match profile_store::load(config_dir) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("apply to window: failed to load config: {e}");
            return;
        }
    };

    let Some(preset) = win32::active_preset(&config.presets) else {
        eprintln!("apply to window: no presets configured");
        return;
    };

    let monitor = primary_monitor_geometry();
    let target_region = match resolve_region(preset, &monitor) {
        Ok(region) => region,
        Err(e) => {
            eprintln!("apply to window: failed to resolve the active preset's region: {e}");
            return;
        }
    };

    if let Err(e) = win32::apply_borderless_to_window(target_hwnd, target_region) {
        eprintln!("apply to window failed: {e}");
    }
}

/// Reacts to a press of the taskbar-autohide-toggle hotkey: flips
/// `taskbar_autohide_state` and applies it via
/// [`win32::toggle_taskbar_autohide`]. A failure (e.g. the taskbar window
/// couldn't be located) is logged rather than crashing, matching
/// [`handle_toggle_borderless`]'s error-handling pattern -- unlike that
/// mechanic, this one never needs elevation (`SHAppBarMessage` on the
/// taskbar doesn't require it), so there's no relaunch-elevated path here.
fn handle_toggle_taskbar_autohide(taskbar_autohide_state: &mut TaskbarAutohideState) {
    if let Err(e) = win32::toggle_taskbar_autohide(taskbar_autohide_state) {
        eprintln!("taskbar auto-hide toggle failed: {e}");
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkeys::resolve_bindings;
    use global_hotkey::hotkey::HotKey;

    /// Regression coverage for the bug where a hotkey bound via Settings
    /// didn't take effect until a full app restart: `run_event_loop`'s
    /// `ShowSettings` branch reacts to Settings closing by calling
    /// [`register_hotkeys`] again, which reloads the config from disk and
    /// resolves it into fresh bindings. This test exercises exactly that
    /// reload-and-resolve chain -- load, simulate a Settings save that
    /// changes a binding, reload -- and asserts the new binding is picked
    /// up, i.e. a second read after a save is never stale.
    ///
    /// It deliberately stops short of calling [`register_hotkeys`] (or
    /// [`Hotkeys::register`]) itself: those also drive a real
    /// `GlobalHotKeyManager` OS registration, which -- per `crate::hotkeys`'s
    /// module doc comment -- needs a live Win32 event loop and is not
    /// exercised by this crate's automated tests, the same boundary that
    /// already excludes `run_event_loop` and `Tray::new` from testing here.
    /// Because of that boundary, this test cannot regression-guard the exact
    /// call site of the fix (the `hotkeys = register_hotkeys(config_dir);`
    /// line added to the `ShowSettings` branch) -- only a live message loop
    /// could do that. What it does guard is the reload mechanism that call
    /// site depends on: that reading the config a second time, after a
    /// Settings-style save, surfaces the change rather than returning
    /// stale/cached data.
    #[test]
    fn reloading_config_after_a_settings_style_save_resolves_the_newly_bound_hotkey() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");

        // Fresh install: nothing bound yet.
        let initial = profile_store::load(&config_dir).unwrap();
        assert_eq!(resolve_bindings(&initial.hotkeys).unwrap(), vec![]);

        // Simulates Settings persisting a newly bound hotkey on Save.
        let mut updated = initial.clone();
        updated.hotkeys.toggle_borderless = Some("F4".to_string());
        profile_store::save(&config_dir, &updated).unwrap();

        // Simulates the fix: reloading after Settings closes, instead of
        // continuing to act on the config snapshot loaded at startup.
        let reloaded = profile_store::load(&config_dir).unwrap();
        let reloaded_resolved = resolve_bindings(&reloaded.hotkeys).unwrap();

        assert_eq!(reloaded_resolved.len(), 1);
        assert_eq!(reloaded_resolved[0].action, HotkeyAction::ToggleBorderless);
        assert_eq!(reloaded_resolved[0].hotkey, "F4".parse::<HotKey>().unwrap());
    }
}
