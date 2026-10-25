//! Command channel from a second `gameframe.exe` to the running instance
//! (issue #70).
//!
//! The running instance owns a hidden **message-only window**; a second launch
//! finds it by class name and posts one registered window message
//! ([`Command`] in `wParam`). A window message is dispatched by *any* message
//! pump on the main thread, so commands arrive even while the modal Settings
//! window is running its own loop -- which a posted thread message would not
//! survive.
//!
//! The window procedure never runs commands itself. It either queues them for
//! the main loop ([`drain`]) or, while Settings is open ([`enter_settings`]),
//! brings that window to the front in place; see [`route`]. Any other blocking
//! handler (zone editor, first-run prompt) is not tracked: commands received
//! meanwhile are queued and handled once the loop is pumping again. Queued
//! commands are de-duplicated, so repeated launches open Settings once.
//!
//! Elevation: the receiver allows the message through UIPI with
//! `ChangeWindowMessageFilterEx`, so a normal launch can signal an elevated
//! instance. The sender grants the receiver the right to take the foreground
//! (`AllowSetForegroundWindow`) so Settings can actually come to the front.
//! Windows may refuse that grant for an elevated target (the result is
//! ignored); Settings then still opens, but may only flash in the taskbar
//! instead of coming to the front. Not verified against an elevated instance.
//!
//! If the instance could not create its window ([`Receiver::create`] returns
//! `None`), a second launch polls for [`SEND_TIMEOUT`] (3 s), logs, and exits.
//!
//! Security: `MSGFLT_ALLOW` lets *any* lower-integrity process in the session
//! post the command. That is harmless for `OpenSettings`, but revisit it
//! before adding `Quit` (#71): a low-integrity process could then quit an
//! elevated instance.
//!
//! Adding a command (e.g. `Quit`, issue #71): add a [`Command`] variant with
//! its wire code in [`Command::to_wparam`]/[`Command::from_wparam`], decide
//! its [`route`], and handle it in the main loop.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::settings_ui::SETTINGS_TITLE;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW,
    DestroyWindow, FindWindowExW, GetWindowThreadProcessId, HWND_MESSAGE, IsIconic, MSGFLT_ALLOW,
    PostMessageW, RegisterClassW, RegisterWindowMessageW, SW_RESTORE, SetForegroundWindow,
    ShowWindow, UnregisterClassW, WNDCLASSW,
};

/// Window class of the hidden receiver. Same GUID as the single-instance mutex.
const CLASS_NAME: &str = "GameFrame.CommandChannel-5C28E192-0AB5-4FA5-8874-88144D010E3B";
/// Name of the registered window message carrying a [`Command`].
const MESSAGE_NAME: &str = "GameFrame.Command-5C28E192-0AB5-4FA5-8874-88144D010E3B";

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
    /// Settings is already open: bring that window to the front.
    FocusSettings,
}

/// Pure routing decision. `settings_open` is whether the main thread is
/// currently blocked inside the Settings window.
pub fn route(command: Command, settings_open: bool) -> Route {
    match command {
        Command::OpenSettings if settings_open => Route::FocusSettings,
        Command::OpenSettings => Route::Queue,
    }
}

/// Adds `command` unless it is already waiting: repeated launches while the
/// main loop is busy must not open Settings once per launch.
fn enqueue(queue: &mut VecDeque<Command>, command: Command) {
    if !queue.contains(&command) {
        queue.push_back(command);
    }
}

static QUEUE: Mutex<VecDeque<Command>> = Mutex::new(VecDeque::new());
static SETTINGS_OPEN: AtomicBool = AtomicBool::new(false);

/// Takes every command received since the last call. Call from the main loop.
pub fn drain() -> Vec<Command> {
    QUEUE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
        .collect()
}

/// Marks the main thread as inside the Settings window for the lifetime of the
/// returned guard.
#[must_use]
pub struct SettingsGuard;

/// Flags Settings as open until the returned guard is dropped.
pub fn enter_settings() -> SettingsGuard {
    SETTINGS_OPEN.store(true, Ordering::SeqCst);
    SettingsGuard
}

impl Drop for SettingsGuard {
    fn drop(&mut self) {
        SETTINGS_OPEN.store(false, Ordering::SeqCst);
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn command_message() -> u32 {
    static MESSAGE: OnceLock<u32> = OnceLock::new();
    *MESSAGE.get_or_init(|| {
        let name = wide(MESSAGE_NAME);
        // SAFETY: `name` is NUL-terminated and outlives the call.
        unsafe { RegisterWindowMessageW(name.as_ptr()) }
    })
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
            if ChangeWindowMessageFilterEx(
                hwnd,
                command_message(),
                MSGFLT_ALLOW,
                std::ptr::null_mut(),
            ) == 0
            {
                log!(
                    "instance channel: could not allow the command message through UIPI; an elevated instance cannot be signalled by a normal launch"
                );
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
        match route(command, SETTINGS_OPEN.load(Ordering::SeqCst)) {
            Route::Queue => enqueue(
                &mut QUEUE.lock().unwrap_or_else(|e| e.into_inner()),
                command,
            ),
            Route::FocusSettings => {
                // Settings is flagged open slightly before its window exists;
                // queue the command rather than lose it.
                if !focus_settings_window() {
                    log!("instance channel: Settings window not created yet; queueing the command");
                    enqueue(
                        &mut QUEUE.lock().unwrap_or_else(|e| e.into_inner()),
                        command,
                    );
                }
            }
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
            let hwnd = FindWindowExW(
                HWND_MESSAGE,
                std::ptr::null_mut(),
                class.as_ptr(),
                std::ptr::null(),
            );
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
        // Extend with every new variant.
        const ALL: &[Command] = &[Command::OpenSettings];
        for &command in ALL {
            assert_eq!(Command::from_wparam(command.to_wparam()), Some(command));
        }
    }

    #[test]
    fn unknown_wire_codes_are_ignored() {
        assert_eq!(Command::from_wparam(0), None);
        assert_eq!(Command::from_wparam(usize::MAX), None);
    }

    #[test]
    fn open_settings_is_queued_when_settings_is_not_open() {
        assert_eq!(route(Command::OpenSettings, false), Route::Queue);
    }

    #[test]
    fn open_settings_focuses_the_window_when_settings_is_already_open() {
        assert_eq!(route(Command::OpenSettings, true), Route::FocusSettings);
    }

    #[test]
    fn repeated_commands_collapse_into_one_queued_entry() {
        let mut queue = VecDeque::new();
        enqueue(&mut queue, Command::OpenSettings);
        enqueue(&mut queue, Command::OpenSettings);
        assert_eq!(queue.len(), 1);
    }
}
