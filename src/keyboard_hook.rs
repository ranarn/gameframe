//! Low-level keyboard hook that suppresses the physical Ctrl+Escape
//! combination while a Settings hotkey capture is in progress, so Windows
//! doesn't steal it for the Start menu before GameFrame's own window ever
//! sees the key-down event.
//!
//! Split the same way as `crate::win32`/`crate::hotkeys`:
//! - [`should_suppress`] is the pure decision -- given a virtual-key code and
//!   whether Ctrl is currently held, should this keystroke be swallowed --
//!   fully unit tested below.
//! - [`take_suppressed_ctrl_escape`] is the pure poll-and-reset over a small
//!   piece of global state ([`keyboard_hook_proc`] can never deliver
//!   Ctrl+Escape to the app any other way -- see its doc comment) -- also
//!   unit tested below.
//! - [`KeyboardHookGuard`] is the OS-facing RAII wrapper: installs a real
//!   `WH_KEYBOARD_LL` hook on construction and uninstalls it via `Drop`,
//!   mirroring `hotkeys::Hotkeys`'/`zone_overlay::TaskbarRestoreGuard`'s
//!   existing Drop-based lifecycle pattern. A headless/CI sandbox can't
//!   provide a live global keyboard hook, so this is NOT exercised by this
//!   crate's automated tests -- same pure/OS-facing split convention as the
//!   rest of the crate.

#[cfg(test)]
use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;

/// Decides whether a physical key-down (`vk_code`, as read from
/// `KBDLLHOOKSTRUCT::vkCode`) should be swallowed by the low-level keyboard
/// hook rather than passed through to the rest of the system -- i.e. whether
/// this is the physical Ctrl+Escape combination Windows would otherwise
/// intercept for the Start menu.
///
/// Only Escape held together with Ctrl is suppressed; Escape alone (no
/// modifier) or any other key (even with Ctrl held) passes through
/// unaffected -- suppressing anything broader would risk breaking normal
/// typing/hotkey behavior outside of capture mode.
///
/// Pure decision logic -- no OS call, no real hook -- so it's unit testable
/// in complete isolation from the live `WH_KEYBOARD_LL` hook that calls it.
pub fn should_suppress(vk_code: u32, ctrl_held: bool) -> bool {
    const VK_ESCAPE_CODE: u32 = 0x1B;
    vk_code == VK_ESCAPE_CODE && ctrl_held
}

use std::sync::atomic::{AtomicBool, Ordering};

/// Set by [`keyboard_hook_proc`] the instant it swallows a physical
/// Ctrl+Escape press, so [`SettingsApp::sync_keyboard_hook`](crate::settings_ui)
/// (running on the very same UI thread that installed the hook, per
/// `SetWindowsHookExW`'s documented contract) can complete the capture
/// directly instead of waiting for an `egui::Event::Key` that will never
/// arrive for this specific combo -- the hook swallows Ctrl+Escape before
/// Windows ever turns it into a window message, so it never reaches egui's
/// normal event pipeline at all.
///
/// Plain global state, not per-hook-instance: safe here because this app
/// never has more than one `KeyboardHookGuard` (and therefore more than one
/// installed hook) alive at a time -- `SettingsApp` owns at most one.
static CTRL_ESCAPE_SUPPRESSED: AtomicBool = AtomicBool::new(false);

/// Polls and clears the flag [`keyboard_hook_proc`] sets when it last
/// swallowed a physical Ctrl+Escape press. Returns `true` at most once per
/// press -- callers should treat a `true` result as "the user just pressed
/// Ctrl+Escape, complete the capture now."
///
/// A thin, deliberately dumb bridge over the global flag -- unit tested here
/// since it's plain `Ordering`/`AtomicBool` logic with no real OS call
/// involved; the *decision* of what to do with the result
/// (`SettingsState::complete_capture_with_ctrl_escape_if_suppressed`) is a
/// separate, pure function in `settings_ui`, tested there.
pub fn take_suppressed_ctrl_escape() -> bool {
    CTRL_ESCAPE_SUPPRESSED.swap(false, Ordering::SeqCst)
}

use windows_sys::Win32::Foundation::{HMODULE, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, SetWindowsHookExW, UnhookWindowsHookEx,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

/// The `WH_KEYBOARD_LL` hook procedure: inspects every physical key-down
/// system-wide and swallows it (returns a nonzero `LRESULT` without calling
/// [`CallNextHookEx`]) exactly when [`should_suppress`] says to -- everything
/// else is passed through to [`CallNextHookEx`] untouched, so normal typing
/// and every other hotkey keep working exactly as before while a capture is
/// active.
///
/// Ctrl+Escape can arrive as either `WM_KEYDOWN` or `WM_SYSKEYDOWN`
/// depending on what else is held, so both are checked. Whether Ctrl is
/// currently held is read via `GetAsyncKeyState` -- `KBDLLHOOKSTRUCT` itself
/// carries only the key that triggered this callback, not the full modifier
/// state.
///
/// Safety: called by the OS on the thread that installed the hook, with
/// `lparam` pointing to a valid `KBDLLHOOKSTRUCT` whenever `wparam` is a
/// keyboard message (guaranteed by `SetWindowsHookExW`'s documented
/// `WH_KEYBOARD_LL` contract) -- exactly the case the `code == HC_ACTION`
/// branch below reads it under.
unsafe extern "system" fn keyboard_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code as u32 == HC_ACTION && (wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN) {
        // Safety: see this function's doc comment -- `lparam` is a valid
        // `KBDLLHOOKSTRUCT` pointer for the duration of this call.
        let info = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        // Safety: `GetAsyncKeyState` takes a plain integer and performs no
        // writes.
        let ctrl_held = unsafe { GetAsyncKeyState(VK_CONTROL as i32) } as u16 & 0x8000 != 0;
        if should_suppress(info.vkCode, ctrl_held) {
            // Signal `SettingsApp::sync_keyboard_hook` directly -- this
            // keystroke is swallowed here and will never arrive as an
            // `egui::Event::Key`, so polling this flag is the only way the
            // capture can ever observe it (see `take_suppressed_ctrl_escape`'s
            // doc comment).
            CTRL_ESCAPE_SUPPRESSED.store(true, Ordering::SeqCst);
            return 1;
        }
    }
    // Safety: `code`/`wparam`/`lparam` are passed through unmodified exactly
    // as received; the first argument (the hook handle) is documented as
    // ignored by `CallNextHookEx`, so a null handle is valid here.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

/// RAII guard owning a live `WH_KEYBOARD_LL` hook installation: [`install`](Self::install)
/// registers the hook, `Drop` unregisters it -- mirroring `hotkeys::Hotkeys`'
/// and `zone_overlay::TaskbarRestoreGuard`'s existing Drop-based lifecycle
/// pattern, so the hook is torn down reliably on every exit path (successful
/// capture, explicit cancel, or the Settings window closing) without a
/// caller needing to remember a manual teardown call.
///
/// OS-facing: installs a real system-wide keyboard hook. Not exercised by
/// this crate's automated tests -- see this module's doc comment.
pub struct KeyboardHookGuard {
    hook: HHOOK,
}

impl KeyboardHookGuard {
    /// Installs the low-level keyboard hook. Returns `None` if
    /// `SetWindowsHookExW` fails (e.g. under some restricted/sandboxed
    /// environments) -- callers treat a failed install as "no suppression
    /// this session" rather than a hard error, since the rest of Settings'
    /// hotkey-capture UI works fine without it (only the Start-menu-steal
    /// races the OS on, not this app's own capture logic).
    pub fn install() -> Option<Self> {
        // Safety: `std::ptr::null()` is `GetModuleHandleW`'s documented way
        // to resolve the calling process's own module handle.
        let hmodule: HMODULE = unsafe { GetModuleHandleW(std::ptr::null()) };
        // Safety: `keyboard_hook_proc` matches `HOOKPROC`'s required
        // signature; `hmodule` is a valid handle to this process's own
        // module, and `WH_KEYBOARD_LL` with `dwthreadid == 0` is documented
        // to install a system-wide low-level keyboard hook.
        let hook =
            unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), hmodule, 0) };
        if hook.is_null() {
            None
        } else {
            Some(Self { hook })
        }
    }
}

impl Drop for KeyboardHookGuard {
    // Note: this `Drop` impl does NOT run if the process aborts (this crate's
    // release profile is `panic = "abort"`), so a mid-capture abort would skip
    // `UnhookWindowsHookEx` here. Not a real leak in practice: Windows itself
    // unregisters every hook owned by a process the moment that process
    // terminates, abort or not -- so there is nothing left dangling for a
    // future reader to worry about.
    fn drop(&mut self) {
        // Safety: `self.hook` was returned by a successful `SetWindowsHookExW`
        // call in `install` and has not been unhooked before (this is the
        // only place that unhooks it, and `Drop` runs at most once).
        unsafe {
            UnhookWindowsHookEx(self.hook);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_suppress_ctrl_escape() {
        assert!(should_suppress(VK_ESCAPE as u32, true));
    }

    #[test]
    fn should_not_suppress_plain_escape_with_no_modifier_held() {
        assert!(!should_suppress(VK_ESCAPE as u32, false));
    }

    #[test]
    fn should_not_suppress_ctrl_held_with_a_different_key() {
        // 'B' (0x42) with Ctrl held -- must never be swallowed, only
        // Ctrl+Escape is special-cased.
        assert!(!should_suppress(0x42, true));
    }

    // Both cases share the global flag, so they run sequentially in one test.
    #[test]
    fn take_suppressed_ctrl_escape_is_false_until_set_then_true_once() {
        CTRL_ESCAPE_SUPPRESSED.store(false, Ordering::SeqCst);
        assert!(!take_suppressed_ctrl_escape());

        CTRL_ESCAPE_SUPPRESSED.store(true, Ordering::SeqCst);
        assert!(take_suppressed_ctrl_escape());
        assert!(!take_suppressed_ctrl_escape());
    }
}
