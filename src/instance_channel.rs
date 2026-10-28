//! Command channel from a second `gameframe.exe` to the running instance.
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
//! Enqueue fallback: an `OpenSettings` that arrives while Settings is flagged
//! open but its window does not exist yet is queued. When Settings closes
//! ([`SettingsGuard`] drops) such an `OpenSettings` is discarded, so Settings
//! does not reopen by itself; other queued commands (`Quit`) are kept. The
//! main loop then keeps taking batches until the queue is empty
//! ([`dispatch_commands`]), so those commands run right after Settings
//! returns instead of at the next message.
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
//! post a message on the [`Channel::Open`] message. That is harmless for
//! `OpenSettings`, but not for `Quit`, so `Quit` travels on a second,
//! [`Channel::Restricted`] message that is *not* allowed through UIPI, and the
//! receiver drops a `Quit` that arrives on the open message. A low-integrity
//! process therefore cannot quit the instance, and neither can a normal
//! launch quit an elevated instance (the installer's old `taskkill` could not
//! either; it falls back to that after its timeout).
//!
//! `Quit` while Settings is open ([`Route::CloseSettingsAndQueue`]): the window
//! procedure posts `WM_CLOSE` to the Settings window and queues the command;
//! once Settings returns, the main loop sees [`quit_pending`] and exits
//! through the same path as the tray's Exit item. Unsaved Settings edits are
//! discarded, as with the window's close button.
//!
//! The egui windows that block the main thread (Settings and the zone editor
//! overlay, from the first-run prompt or from Settings) also poll
//! [`quit_pending`] every [`QUIT_POLL`] and close themselves on a queued
//! `Quit`; the overlay ends like Cancel (restoring the taskbar), Settings does
//! not reopen afterwards, and the main loop checks [`quit_pending`] before it
//! starts pumping. This also covers a `Quit` that arrives before the Settings
//! window exists.
//!
//! The installer (`installer/gameframe.iss`) sends `Quit` itself from Pascal,
//! using the class name, restricted message name and `wParam` code defined
//! here; keep them in sync. (An old build has no receiver window, so the
//! installer's send is a no-op there and it falls back to `taskkill`.)
//!
//! Adding a command: add a [`Command`] variant with its wire code in
//! [`Command::to_wparam`]/[`Command::from_wparam`], pick its [`Channel`],
//! decide its [`route`], and handle it in the main loop.

use crate::wide::encode_wide;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW,
    DestroyWindow, FindWindowExW, GetWindowThreadProcessId, HWND_MESSAGE, IsIconic, MSGFLT_ALLOW,
    PostMessageW, RegisterClassW, RegisterWindowMessageW, SW_RESTORE, SetForegroundWindow,
    ShowWindow, UnregisterClassW, WM_CLOSE, WM_ENDSESSION, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};

/// Title of the Settings window (`crate::settings_ui` creates it with this
/// title); [`find_settings_window`] finds the window by it.
pub(crate) const SETTINGS_TITLE: &str = "GameFrame Settings";

/// Window class of the hidden receiver. Same GUID as the single-instance mutex.
const CLASS_NAME: &str = "GameFrame.CommandChannel-5C28E192-0AB5-4FA5-8874-88144D010E3B";
/// Name of the registered window message carrying a [`Command`].
const MESSAGE_NAME: &str = "GameFrame.Command-5C28E192-0AB5-4FA5-8874-88144D010E3B";
/// Name of the registered window message carrying a [`Channel::Restricted`]
/// command. The installer's Pascal code hard-codes this name.
const RESTRICTED_MESSAGE_NAME: &str =
    "GameFrame.RestrictedCommand-5C28E192-0AB5-4FA5-8874-88144D010E3B";

/// How long a sender keeps looking for the running instance's window: the
/// instance takes the mutex slightly before it creates the window.
const SEND_TIMEOUT: Duration = Duration::from_secs(3);
const SEND_POLL: Duration = Duration::from_millis(100);

/// How often a blocking egui window re-checks [`quit_pending`] while idle.
pub const QUIT_POLL: Duration = Duration::from_millis(200);

/// What a second launch can ask the running instance to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Open Settings, or bring the open one to the front.
    OpenSettings,
    /// Exit exactly as the tray's Exit item does (closing Settings first if it
    /// is open). Sent by the installer and, later, the updater.
    Quit,
}

/// Which registered window message carries a command. They differ in UIPI
/// treatment, see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Allowed through UIPI (`MSGFLT_ALLOW`).
    Open,
    /// Not filtered: only a sender at the instance's own integrity level (or
    /// higher) gets through.
    Restricted,
}

impl Command {
    /// Wire encoding (the message's `wParam`). The installer's Pascal code
    /// (`installer/gameframe.iss`) hard-codes `Quit`'s value.
    pub fn to_wparam(self) -> usize {
        match self {
            Command::OpenSettings => 1,
            Command::Quit => 2,
        }
    }

    /// Decodes a `wParam`; `None` for an unknown code (e.g. from a newer
    /// build), which the receiver ignores.
    pub fn from_wparam(value: usize) -> Option<Command> {
        match value {
            1 => Some(Command::OpenSettings),
            2 => Some(Command::Quit),
            _ => None,
        }
    }

    /// The message this command travels on.
    pub fn channel(self) -> Channel {
        match self {
            Command::OpenSettings => Channel::Open,
            Command::Quit => Channel::Restricted,
        }
    }

    /// Decodes a command received on `channel`. A command arriving on the
    /// wrong channel is rejected: `Quit` must not be accepted from the
    /// UIPI-open message.
    pub fn from_message(channel: Channel, wparam: usize) -> Option<Command> {
        Command::from_wparam(wparam).filter(|c| c.channel() == channel)
    }
}

/// What the window procedure does with a received command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Leave it for the main loop ([`drain`]).
    Queue,
    /// Settings is already open: bring that window to the front.
    FocusSettings,
    /// Settings is open: close it, and queue the command so the main loop
    /// acts on it as soon as Settings has returned.
    CloseSettingsAndQueue,
}

/// Pure routing decision. `settings_open` is whether the main thread is
/// currently blocked inside the Settings window.
pub fn route(command: Command, settings_open: bool) -> Route {
    match command {
        Command::OpenSettings if settings_open => Route::FocusSettings,
        Command::OpenSettings => Route::Queue,
        Command::Quit if settings_open => Route::CloseSettingsAndQueue,
        Command::Quit => Route::Queue,
    }
}

/// Whether a drained batch asks the main loop to exit. Quit wins over every
/// other command in the batch (no point opening Settings first).
pub fn wants_quit(commands: &[Command]) -> bool {
    commands.contains(&Command::Quit)
}

/// Adds `command` unless it is already waiting: repeated launches while the
/// main loop is busy must not open Settings once per launch.
fn enqueue(queue: &mut VecDeque<Command>, command: Command) {
    if !queue.contains(&command) {
        queue.push_back(command);
    }
}

/// Drops a queued `OpenSettings`, keeping every other command in order. Run
/// when Settings closes: an `OpenSettings` that arrived while it was open (or
/// opening) was already answered by that window, and must not reopen it.
fn discard_open_settings(queue: &mut VecDeque<Command>) {
    queue.retain(|&c| c != Command::OpenSettings);
}

/// The main loop's command handling: takes batches via `take` until one is
/// empty, so commands queued while `open_settings` blocked are handled right
/// after it returns, without waiting for another message. Returns `true` when
/// the loop must exit (a `Quit`, which wins over everything else).
pub fn dispatch_commands(
    mut take: impl FnMut() -> Vec<Command>,
    mut open_settings: impl FnMut(),
) -> bool {
    loop {
        let commands = take();
        if commands.is_empty() {
            return false;
        }
        if wants_quit(&commands) {
            return true;
        }
        for command in commands {
            match command {
                Command::OpenSettings => open_settings(),
                // Unreachable after `wants_quit`; exit rather than ignore.
                Command::Quit => return true,
            }
        }
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

/// Whether a `Quit` is waiting in the queue. The main loop checks this after
/// Settings returns, since a `Quit` received while Settings was open arrives
/// in the queue without any further message to wake the loop.
pub fn quit_pending() -> bool {
    QUEUE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&Command::Quit)
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
        // The queue lock is held for the discard, so it is atomic with respect
        // to the window procedure's enqueue.
        discard_open_settings(&mut QUEUE.lock().unwrap_or_else(|e| e.into_inner()));
    }
}

fn command_message() -> u32 {
    static MESSAGE: OnceLock<u32> = OnceLock::new();
    *MESSAGE.get_or_init(|| register_message(MESSAGE_NAME))
}

fn restricted_message() -> u32 {
    static MESSAGE: OnceLock<u32> = OnceLock::new();
    *MESSAGE.get_or_init(|| register_message(RESTRICTED_MESSAGE_NAME))
}

fn register_message(name: &str) -> u32 {
    let name = encode_wide(name);
    // SAFETY: `name` is NUL-terminated and outlives the call.
    unsafe { RegisterWindowMessageW(name.as_ptr()) }
}

fn message_for(channel: Channel) -> u32 {
    match channel {
        Channel::Open => command_message(),
        Channel::Restricted => restricted_message(),
    }
}

/// The running instance's receiver. Dropping it destroys the window.
pub struct Receiver {
    hwnd: HWND,
    /// Never-shown top-level window for `WM_ENDSESSION`; null if it could not
    /// be created.
    session_hwnd: HWND,
}

impl Receiver {
    /// Creates the hidden window on the calling thread (the main thread: its
    /// pump must be the one that dispatches the messages). `None` if the
    /// window could not be created; the instance then simply cannot be
    /// signalled.
    pub fn create() -> Option<Receiver> {
        let class = encode_wide(CLASS_NAME);
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
            let session_hwnd = create_session_window(hinstance);
            Some(Receiver { hwnd, session_hwnd })
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        // SAFETY: `hwnd` is our own window, created on this thread.
        unsafe {
            DestroyWindow(self.hwnd);
            if !self.session_hwnd.is_null() {
                DestroyWindow(self.session_hwnd);
                let class = encode_wide(SESSION_CLASS_NAME);
                UnregisterClassW(class.as_ptr(), GetModuleHandleW(std::ptr::null()));
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let channel = if msg == command_message() {
        Some(Channel::Open)
    } else if msg == restricted_message() {
        Some(Channel::Restricted)
    } else {
        None
    };
    if let Some(channel) = channel {
        // Unknown or wrong-channel commands are dropped, but the message is
        // still ours: never hand it to DefWindowProc.
        let Some(command) = Command::from_message(channel, wparam) else {
            return 0;
        };
        match route(command, SETTINGS_OPEN.load(Ordering::SeqCst)) {
            Route::Queue => enqueue(
                &mut QUEUE.lock().unwrap_or_else(|e| e.into_inner()),
                command,
            ),
            Route::CloseSettingsAndQueue => {
                enqueue(
                    &mut QUEUE.lock().unwrap_or_else(|e| e.into_inner()),
                    command,
                );
                // If Settings has no window (not created yet, or the zone
                // editor is running), its frame loop / the overlay polls
                // `quit_pending` instead.
                close_settings_window();
            }
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

/// Window class of the never-shown top-level window that receives
/// `WM_ENDSESSION`.
const SESSION_CLASS_NAME: &str = "GameFrame.SessionEnd-5C28E192-0AB5-4FA5-8874-88144D010E3B";

/// Window procedure of the session-end window. The system sends
/// `WM_QUERYENDSESSION`/`WM_ENDSESSION` to top-level windows only, never to a
/// message-only window, hence this separate window. `WM_QUERYENDSESSION` is
/// left to `DefWindowProc` (allow the end).
unsafe extern "system" fn session_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_ENDSESSION && should_restore_on_end_session(wparam) {
        // The process may be ended as soon as this returns: restore now, the
        // same way the tray's Exit item does, and clear the marker.
        //
        // A sent WM_ENDSESSION can be dispatched from inside a cross-process
        // `SendMessage` of an in-flight hide (appbar state write). That is
        // acceptable: the process is ending, and if the hide then finishes it
        // has already written the marker, so the next start recovers. No lock
        // is held across an OS call in the hide/restore paths, so this cannot
        // deadlock.
        end_session_restore();
        return 0;
    }
    // SAFETY: forwards the exact arguments we were called with.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// The restore a session end performs. Tests run it for real: the
/// OS-facing steps underneath are faked out in `taskbar_recovery`.
fn end_session_restore() {
    log!("session ending: restoring the taskbar");
    crate::taskbar::restore_all_for_exit();
}

/// Creates the never-shown top-level window (`WS_POPUP`, `WS_EX_TOOLWINDOW`,
/// no `ShowWindow` ever): an unshown window has no taskbar button and is not
/// in Alt+Tab. Must run on the main thread so its pump dispatches the message.
fn create_session_window(hinstance: windows_sys::Win32::Foundation::HINSTANCE) -> HWND {
    let class = encode_wide(SESSION_CLASS_NAME);
    // SAFETY: plain Win32 calls with NUL-terminated strings that outlive them;
    // `session_wndproc` has the required signature.
    unsafe {
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(session_wndproc),
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
            log!("instance channel: could not register the session-end window class");
            return std::ptr::null_mut();
        }
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            class.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            log!("instance channel: could not create the session-end window");
            UnregisterClassW(class.as_ptr(), hinstance);
        }
        hwnd
    }
}

/// Whether a `WM_ENDSESSION` makes the app restore the taskbar: only when the
/// session really ends (`wParam` TRUE), not when a `WM_QUERYENDSESSION` was
/// vetoed by another program and the end is cancelled.
pub fn should_restore_on_end_session(wparam: usize) -> bool {
    wparam != 0
}

/// This process's Settings window, if it exists yet.
pub(crate) fn find_settings_window() -> Option<HWND> {
    let title = encode_wide(SETTINGS_TITLE);
    // SAFETY: plain Win32 calls; `title` outlives them.
    unsafe {
        let pid = GetCurrentProcessId();
        let mut prev: HWND = std::ptr::null_mut();
        loop {
            let hwnd = FindWindowExW(std::ptr::null_mut(), prev, std::ptr::null(), title.as_ptr());
            if hwnd.is_null() {
                return None;
            }
            let mut owner = 0u32;
            GetWindowThreadProcessId(hwnd, &mut owner);
            if owner == pid {
                return Some(hwnd);
            }
            prev = hwnd;
        }
    }
}

/// Brings this process's Settings window to the front. `false` if not found.
pub fn focus_settings_window() -> bool {
    let Some(hwnd) = find_settings_window() else {
        return false;
    };
    // SAFETY: plain Win32 calls on a window handle just looked up.
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd);
    }
    true
}

/// This process's command window (owned by the thread that created the
/// [`Receiver`]), if it exists. Lets [`crate::watchdog`] ping the main thread.
/// Windows of another GameFrame process (an elevated relaunch overlapping the
/// old instance) share the class name and are skipped.
pub fn command_window() -> Option<HWND> {
    let class = encode_wide(CLASS_NAME);
    // SAFETY: plain Win32 calls; `class` outlives them.
    unsafe {
        let pid = GetCurrentProcessId();
        let mut prev: HWND = std::ptr::null_mut();
        loop {
            let hwnd = FindWindowExW(HWND_MESSAGE, prev, class.as_ptr(), std::ptr::null());
            if hwnd.is_null() {
                return None;
            }
            let mut owner = 0u32;
            GetWindowThreadProcessId(hwnd, &mut owner);
            if owner == pid {
                return Some(hwnd);
            }
            prev = hwnd;
        }
    }
}

/// Queues a `Quit` from inside this process (the tray's Exit item or an
/// elevated relaunch while Settings is open): the Settings loop closes itself
/// on [`quit_pending`] and the main loop then exits.
pub fn queue_quit() {
    enqueue(
        &mut QUEUE.lock().unwrap_or_else(|e| e.into_inner()),
        Command::Quit,
    );
}

/// Asks this process's Settings window to close, as its close button would
/// (the Settings loop then returns and discards unsaved edits). `false` if not
/// found.
fn close_settings_window() -> bool {
    let Some(hwnd) = find_settings_window() else {
        return false;
    };
    // SAFETY: posting to a window handle just looked up; a stale handle only
    // makes the post fail.
    unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) != 0 }
}

/// Sends `command` to the running instance, waiting briefly for its window to
/// exist. Returns whether the message was posted.
pub fn send(command: Command) -> bool {
    send_within(command, SEND_TIMEOUT)
}

/// Like [`send`], but looks for the instance's window for at most `timeout`
/// (`Duration::ZERO` = one attempt, for callers where "not running" is the
/// normal case and must not stall).
pub fn send_within(command: Command, timeout: Duration) -> bool {
    let class = encode_wide(CLASS_NAME);
    let message = message_for(command.channel());
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
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(SEND_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_session_restores_only_when_the_session_really_ends() {
        assert!(should_restore_on_end_session(1));
        assert!(!should_restore_on_end_session(0));
    }

    /// Destroys the session window and unregisters its class on drop.
    struct SessionWindow(HWND);
    impl Drop for SessionWindow {
        fn drop(&mut self) {
            // SAFETY: our own window, created on this thread.
            unsafe {
                DestroyWindow(self.0);
                let class = encode_wide(SESSION_CLASS_NAME);
                UnregisterClassW(class.as_ptr(), GetModuleHandleW(std::ptr::null()));
            }
        }
    }

    #[test]
    fn session_window_restores_on_a_real_end_session_only() {
        use crate::taskbar_recovery::{MARKER_FILE, init, mark_hidden};
        use crate::win32::TaskbarHideRestore;
        use windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW;
        struct RemoveDir(std::path::PathBuf);
        impl Drop for RemoveDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = std::env::temp_dir().join(format!("gameframe-endsession-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveDir(dir.clone());
        // `taskbar_recovery::init` sets a process-wide OnceLock (first call
        // wins, never reset). This is the only test that calls it; a second
        // one must share this directory (e.g. via a helper) or it will write
        // its marker here.
        init(&dir);
        let marker = dir.join(MARKER_FILE);
        let _ = mark_hidden(TaskbarHideRestore::FullyHidden);
        assert!(marker.exists(), "hiding must write the recovery marker");
        // SAFETY: plain Win32 calls on a window created and destroyed here.
        unsafe {
            let hwnd = create_session_window(GetModuleHandleW(std::ptr::null()));
            assert!(!hwnd.is_null(), "session window must be creatable");
            let _guard = SessionWindow(hwnd);
            // A cancelled end (wParam FALSE) leaves the marker alone.
            SendMessageW(hwnd, WM_ENDSESSION, 0, 0);
            assert!(marker.exists(), "a cancelled end must not restore");
            SendMessageW(hwnd, WM_ENDSESSION, 1, 0);
            assert!(!marker.exists(), "a real end must clear the marker");
        }
    }

    #[test]
    fn commands_round_trip_through_the_wire_encoding() {
        // Extend with every new variant.
        const ALL: &[Command] = &[Command::OpenSettings, Command::Quit];
        for &command in ALL {
            assert_eq!(Command::from_wparam(command.to_wparam()), Some(command));
            assert_eq!(
                Command::from_message(command.channel(), command.to_wparam()),
                Some(command)
            );
        }
    }

    /// Value of `#define <name> "<value>"` in the installer script.
    fn iss_define(name: &str) -> String {
        let script = include_str!("../installer/gameframe.iss");
        let prefix = format!("#define {name} ");
        let line = script
            .lines()
            .find_map(|l| l.strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("no #define {name} in gameframe.iss"));
        line.trim().to_string()
    }

    #[test]
    fn quit_wire_code_matches_the_installer() {
        assert_eq!(
            iss_define("QuitWParam"),
            format!("\"{}\"", Command::Quit.to_wparam())
        );
    }

    #[test]
    fn installer_names_match_the_ones_registered_here() {
        let guid = iss_define("AppGuid");
        let guid = guid.trim_matches('"');
        let expect = |define: &str, prefix: &str, actual: &str| {
            assert_eq!(
                iss_define(define),
                format!("\"{prefix}\" + AppGuid"),
                "{define} form"
            );
            assert_eq!(actual, format!("{prefix}{guid}"), "{define} value");
        };
        expect(
            "MutexName",
            "Local\\GameFrame-",
            crate::single_instance::MUTEX_NAME,
        );
        expect("CommandClass", "GameFrame.CommandChannel-", CLASS_NAME);
        expect(
            "QuitMessage",
            "GameFrame.RestrictedCommand-",
            RESTRICTED_MESSAGE_NAME,
        );
    }

    #[test]
    fn quit_is_rejected_on_the_uipi_open_channel() {
        assert_eq!(Command::Quit.channel(), Channel::Restricted);
        assert_eq!(
            Command::from_message(Channel::Open, Command::Quit.to_wparam()),
            None
        );
    }

    #[test]
    fn quit_is_queued_when_settings_is_not_open() {
        assert_eq!(route(Command::Quit, false), Route::Queue);
    }

    #[test]
    fn quit_closes_settings_and_is_queued_when_settings_is_open() {
        assert_eq!(route(Command::Quit, true), Route::CloseSettingsAndQueue);
    }

    #[test]
    fn a_batch_with_quit_wants_quit_even_after_open_settings() {
        assert!(wants_quit(&[Command::OpenSettings, Command::Quit]));
        assert!(!wants_quit(&[Command::OpenSettings]));
        assert!(!wants_quit(&[]));
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
    fn open_settings_queued_while_settings_was_open_is_dropped_but_quit_is_kept() {
        let mut queue = VecDeque::new();
        enqueue(&mut queue, Command::OpenSettings);
        enqueue(&mut queue, Command::Quit);
        discard_open_settings(&mut queue);
        assert_eq!(Vec::from(queue), vec![Command::Quit]);
    }

    #[test]
    fn commands_queued_during_settings_run_once_it_returns_without_another_message() {
        use std::cell::RefCell;
        let queue = RefCell::new(VecDeque::from([Command::OpenSettings]));
        let take = || queue.borrow_mut().drain(..).collect::<Vec<_>>();
        let mut opened = 0;
        let quit = dispatch_commands(take, || {
            opened += 1;
            // Queued while Settings is open and still queued when it returns.
            let mut q = queue.borrow_mut();
            enqueue(&mut q, Command::Quit);
        });
        assert!(quit);
        assert_eq!(opened, 1);
    }

    #[test]
    fn dropping_the_settings_guard_discards_open_settings_but_keeps_quit() {
        // The only test that touches the global queue; reset it afterwards.
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                QUEUE.lock().unwrap_or_else(|e| e.into_inner()).clear();
                SETTINGS_OPEN.store(false, Ordering::SeqCst);
            }
        }
        let _reset = Reset;

        let guard = enter_settings();
        {
            let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
            enqueue(&mut q, Command::OpenSettings);
            enqueue(&mut q, Command::Quit);
        }
        drop(guard);

        assert!(!SETTINGS_OPEN.load(Ordering::SeqCst));
        assert_eq!(drain(), vec![Command::Quit]);
    }

    #[test]
    fn dispatch_stops_when_the_queue_is_empty() {
        let mut batches = vec![vec![], vec![Command::OpenSettings]];
        let mut opened = 0;
        assert!(!dispatch_commands(
            || batches.pop().unwrap(),
            || opened += 1
        ));
        assert_eq!(opened, 1);
    }

    #[test]
    fn repeated_commands_collapse_into_one_queued_entry() {
        let mut queue = VecDeque::new();
        enqueue(&mut queue, Command::OpenSettings);
        enqueue(&mut queue, Command::OpenSettings);
        assert_eq!(queue.len(), 1);
    }
}
