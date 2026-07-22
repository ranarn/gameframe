//! Thin orchestrator: builds the tray icon/menu ([`crate::tray_ui`]) and
//! then runs the Win32 message loop that keeps it alive, dispatching a
//! click on "Exit" to a clean shutdown. No business logic lives here --
//! "Show Settings" and "Apply to Window" are no-ops, wired to real
//! behavior in later slices.
//!
//! The message loop itself ([`run`]) is OS-facing (pumps real Win32
//! messages) and is not exercised by this crate's automated tests, for
//! the same reason [`crate::tray_ui::Tray::new`] isn't: it needs a live
//! thread message queue that a headless/CI sandbox can't provide.

use crate::tray_ui::{Tray, TrayUiError};
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
            if tray.ids().is_exit(event.id()) {
                // Returning here unwinds back to `run()`, which then
                // drops its owned `tray`, removing the OS tray icon
                // and destroying its hidden window -- the entire
                // clean-shutdown path for the tray UI.
                return;
            }
            // "Show Settings" / "Apply to Window": no-op stubs in this
            // slice (also currently disabled menu items, so in
            // practice they won't even fire).
        }
    }
}
