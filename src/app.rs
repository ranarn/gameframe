//! Thin orchestrator: builds the tray icon/menu ([`crate::tray_ui`]) and
//! then runs the Win32 message loop that keeps it alive, dispatching a
//! click on "Exit" to a clean shutdown and a click on "Show Settings" to
//! lazily opening (and fully tearing down, on close) the egui Settings
//! window ([`crate::settings_ui`]). "Apply to Window" remains a no-op --
//! actual window manipulation is out of scope for this slice.
//!
//! The message loop itself ([`run`]) is OS-facing (pumps real Win32
//! messages) and is not exercised by this crate's automated tests, for
//! the same reason [`crate::tray_ui::Tray::new`] isn't: it needs a live
//! thread message queue that a headless/CI sandbox can't provide.

use crate::settings_ui;
use crate::tray_ui::{Tray, TrayAction, TrayUiError};
use std::path::PathBuf;
use tray_icon::menu::MenuEvent;
use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, TranslateMessage, MSG};

/// Starts GameFrame: shows only the tray icon (no window) and blocks,
/// pumping the Windows message loop the tray icon needs, until "Exit"
/// is selected. Returns once the tray icon (and its hidden window) has
/// been cleanly torn down.
pub fn run() -> Result<(), TrayUiError> {
    let tray = Tray::new()?;
    run_event_loop(&tray);
    Ok(())
}

/// Pumps the Win32 message loop on this thread. Each dispatched message
/// may cause the tray/menu to enqueue an event on
/// [`MenuEvent::receiver`]; draining it after every dispatched message
/// lets us react to a click without a separate polling thread.
fn run_event_loop(tray: &Tray) {
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
                    if let Err(e) = open_settings() {
                        eprintln!("failed to open Settings: {e}");
                    }
                }
                Some(TrayAction::ApplyToWindow) => {
                    // No-op in this slice -- actual window manipulation
                    // is out of scope (see issue #5).
                }
                None => {}
            }
        }
    }
}

/// Resolves the real config directory (`%APPDATA%\GameFrame`) and opens the
/// Settings window there. See [`settings_ui::run_settings_window`] for what
/// "opens" entails (blocks until closed, persists on Save).
fn open_settings() -> Result<(), settings_ui::SettingsUiError> {
    let config_dir = config_dir();
    settings_ui::run_settings_window(&config_dir)
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
