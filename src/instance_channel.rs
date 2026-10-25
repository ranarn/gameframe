//! Command channel from a second `gameframe.exe` to the running instance
//! (issue #70).
//!
//! The running instance owns a hidden **message-only window**; a second launch
//! finds it by class name and posts one registered window message
//! ([`Command`] in `wParam`). A window message is dispatched by *any* message
//! pump on the main thread, so commands arrive even while the modal Settings
//! window (or the zone editor) is running its own loop -- which a posted
//! thread message would not survive.
//!
//! The window procedure never runs commands itself. It either queues them for
//! the main loop ([`drain`]) or, while the main thread is busy in a modal
//! window ([`set_modal`]), handles what can safely be done in place (bringing
//! the open Settings window to the front); see [`route`].
//!
//! Elevation: the receiver allows the message through UIPI with
//! `ChangeWindowMessageFilterEx`, so a normal launch can signal an elevated
//! instance. The sender grants the receiver the right to take the foreground
//! (`AllowSetForegroundWindow`) so Settings can actually come to the front.
//!
//! Adding a command (e.g. `Quit`, issue #71): add a [`Command`] variant with
//! its wire code in [`Command::to_wparam`]/[`Command::from_wparam`], decide
//! its [`route`], and handle it in the main loop.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW, DestroyWindow,
    FindWindowExW, GetWindowThreadProcessId, HWND_MESSAGE, IsIconic, MSGFLT_ALLOW, PostMessageW, RegisterClassW,
    RegisterWindowMessageW, SW_RESTORE, SetForegroundWindow, ShowWindow, UnregisterClassW, WNDCLASSW,
};

/// Window class of the hidden receiver. Same GUID as the single-instance mutex.
const CLASS_NAME: &str = "GameFrame.CommandChannel-5C28E192-0AB5-4FA5-8874-88144D010E3B";
/// Name of the registered window message carrying a [`Command`].
const MESSAGE_NAME: &str = "GameFrame.Command-5C28E192-0AB5-4FA5-8874-88144D010E3B";
/// Title of the Settings window (`settings_ui::run_settings_frame`).
const SETTINGS_TITLE: &str = "GameFrame Settings";

/// How long a sender keeps looking for the running instance's window: the
/// instance takes the mutex slightly before it creates the window.
const SEND_TIMEOUT: Duration = Duration::from_secs(3);
const SEND_POLL: Duration = Duration::from_millis(100);

/// What a second launch can ask the running instance to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Open Settings, or bring the open one to the front.
    OpenSettings,
}

impl Command {
    /// Wire encoding (the message's `wParam`).
    pub fn to_wparam(self) -> usize {
        match self {
            Command::OpenSettings => 1,
        }
    }

    /// Decodes a `wParam`; `None` for an unknown code (e.g. from a newer
    /// build), which the receiver ignores.
    pub fn from_wparam(value: usize) -> Option<Command> {
        match value {
            1 => Some(Command::OpenSettings),
            _ => None,
        }
    }
}

/// What the window procedure does with a received command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Leave it for the main loop ([`drain`]).
    Queue,
    /// The main thread is inside Settings: bring that window to the front.
    FocusSettings,
    /// The main thread is inside another modal window: nothing sensible to do.
    Drop,
}

/// Pure routing decision. `modal` is whether the main thread is currently
/// blocked in a modal window, `settings_open` whether that includes Settings.
pub fn route(command: Command, modal: bool, settings_open: bool) -> Route {
    match command {
        Command::OpenSettings if !modal => Route::Queue,
        Command::OpenSettings if settings_open => Route::FocusSettings,
        Command::OpenSettings => Route::Drop,
    }
}

static QUEUE: Mutex<VecDeque<Command>> = Mutex::new(VecDeque::new());
static MODAL: AtomicBool = AtomicBool::new(false);
static SETTINGS_OPEN: AtomicBool = AtomicBool::new(false);

/// Takes every command received since the last call. Call from the main loop.
pub fn drain() -> Vec<Command> {
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect()
}

/// Marks the main thread as inside a modal window (`settings` = the Settings
/// window) for the lifetime of the returned guard.
#[must_use]
pub struct ModalGuard {
    settings: bool,
}

pub fn enter_modal(settings: bool) -> ModalGuard {
    MODAL.store(true, Ordering::SeqCst);
    SETTINGS_OPEN.store(settings, Ordering::SeqCst);
    ModalGuard { settings }
}

impl Drop for ModalGuard {
    fn drop(&mut self) {
        if self.settings {
            SETTINGS_OPEN.store(false, Ordering::SeqCst);
        }
        MODAL.store(false, Ordering::SeqCst);
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn command_message() -> u32 {
    let name = wide(MESSAGE_NAME);
    // SAFETY: `name` is NUL-terminated and outlives the call.
    unsafe { RegisterWindowMessageW(name.as_ptr()) }
}

/// The running instance's receiver. Dropping it destroys the window.
pub struct Receiver {
    hwnd: HWND,
}

impl Receiver {
    /// Creates the hidden window on the calling thread (the main thread: its
    /// pump must be the one that dispatches the messages). `None` if the
    /// window could not be created; the instance then simply cannot be
    /// signalled.
    pub fn create() -> Option<Receiver> {
        let class = wide(CLASS_NAME);
        // SAFETY: plain Win32 calls with NUL-terminated strings that outlive
        // them; `wndproc` has the required signature and 'static lifetime.
        unsafe {
            let hinstance = GetModuleHandleW(std::ptr::null());
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
            };
            if RegisterClassW(&wc) == 0 {
                log!("instance channel: could not register the window class");
                return None;
            }
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                log!("instance channel: could not create the message window");
                UnregisterClassW(class.as_ptr(), hinstance);
                return None;
            }
            // Let a lower-integrity (non-elevated) sender through UIPI.
            if ChangeWindowMessageFilterEx(hwnd, command_message(), MSGFLT_ALLOW, std::ptr::null_mut()) == 0 {
                log!("instance channel: could not allow the command message through UIPI; an elevated instance cannot be signalled by a normal launch");
            }
            Some(Receiver { hwnd })
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        // SAFETY: `hwnd` is our own window, created on this thread.
        unsafe { DestroyWindow(self.hwnd) };
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == command_message()
        && let Some(command) = Command::from_wparam(wparam)
    {
        let modal = MODAL.load(Ordering::SeqCst);
        let settings_open = SETTINGS_OPEN.load(Ordering::SeqCst);
        match route(command, modal, settings_open) {
            Route::Queue => QUEUE.lock().unwrap_or_else(|e| e.into_inner()).push_back(command),
            Route::FocusSettings => {
                if !focus_settings_window() {
                    log!("instance channel: Settings is open but its window was not found");
                }
            }
            Route::Drop => log!("instance channel: ignoring {command:?} while a modal window is open"),
        }
        return 0;
    }
    // SAFETY: forwards the exact arguments we were called with.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Brings this process's Settings window to the front. `false` if not found.
fn focus_settings_window() -> bool {
    let title = wide(SETTINGS_TITLE);
    // SAFETY: plain Win32 calls; `title` outlives them.
    unsafe {
        let pid = GetCurrentProcessId();
        let mut prev: HWND = std::ptr::null_mut();
        loop {
            let hwnd = FindWindowExW(std::ptr::null_mut(), prev, std::ptr::null(), title.as_ptr());
            if hwnd.is_null() {
                return false;
            }
            let mut owner = 0u32;
            GetWindowThreadProcessId(hwnd, &mut owner);
            if owner == pid {
                if IsIconic(hwnd) != 0 {
                    ShowWindow(hwnd, SW_RESTORE);
                }
                SetForegroundWindow(hwnd);
                return true;
            }
            prev = hwnd;
        }
    }
}

/// Sends `command` to the running instance, waiting briefly for its window to
/// exist. Returns whether the message was posted.
pub fn send(command: Command) -> bool {
    let class = wide(CLASS_NAME);
    let message = command_message();
    let start = Instant::now();
    loop {
        // SAFETY: plain Win32 calls; `class` outlives them.
        unsafe {
            let hwnd = FindWindowExW(HWND_MESSAGE, std::ptr::null_mut(), class.as_ptr(), std::ptr::null());
            if !hwnd.is_null() {
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, &mut pid);
                // Lets the receiver bring Settings to the foreground.
                AllowSetForegroundWindow(pid);
                return PostMessageW(hwnd, message, command.to_wparam(), 0) != 0;
            }
        }
        if start.elapsed() >= SEND_TIMEOUT {
            return false;
        }
        std::thread::sleep(SEND_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_round_trip_through_the_wire_encoding() {
        for command in [Command::OpenSettings] {
            assert_eq!(Command::from_wparam(command.to_wparam()), Some(command));
        }
    }

    #[test]
    fn unknown_wire_codes_are_ignored() {
        assert_eq!(Command::from_wparam(0), None);
        assert_eq!(Command::from_wparam(usize::MAX), None);
    }

    #[test]
    fn open_settings_is_queued_when_the_main_loop_is_free() {
        assert_eq!(route(Command::OpenSettings, false, false), Route::Queue);
    }

    #[test]
    fn open_settings_focuses_the_window_when_settings_is_already_open() {
        assert_eq!(route(Command::OpenSettings, true, true), Route::FocusSettings);
    }

    #[test]
    fn open_settings_is_dropped_inside_another_modal_window() {
        assert_eq!(route(Command::OpenSettings, true, false), Route::Drop);
    }
}
