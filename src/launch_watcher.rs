//! Detects a tracked game launching while GameFrame is idle in the tray via
//! a `SetWinEventHook(WINEVENT_OUTOFCONTEXT)` registration on
//! `EVENT_SYSTEM_FOREGROUND`, and reacts by applying that game's preset and
//! taskbar-auto-hide state the same way the hotkey/tray "Apply to Window"
//! paths do -- with no hotkey press required. See issue #32 and
//! `docs/research/idle-game-launch-detection.md` for the comparison
//! rationale behind choosing this mechanism over a toolhelp poll loop or a
//! WMI process-creation subscription.
//!
//! # Registration
//!
//! [`register`] must be called once, on the same thread that runs
//! [`crate::app::run_event_loop`]'s `GetMessageW` pump, **before** that
//! thread enters the pump -- per the official docs, "the client thread that
//! calls `SetWinEventHook` must have a message loop." GameFrame's tray-idle
//! thread already satisfies this, so `WINEVENT_OUTOFCONTEXT` delivers
//! events as ordinary messages pumped through the loop that already exists
//! for the tray icon/hotkeys -- no dedicated hidden message-only window or
//! second thread is needed. [`crate::app::run`] calls [`register`] before
//! [`crate::app::run_event_loop`] and [`unregister`] after it returns (the
//! "Exit" path), for clean teardown -- not strictly load-bearing since the
//! process exits right after, but matching this codebase's existing care
//! around explicit teardown (see [`crate::hotkeys::Hotkeys`]'s `Drop` impl
//! unregistering hotkeys).
//!
//! # No user-data parameter
//!
//! The callback signature Windows requires
//! (`unsafe extern "system" fn(HWINEVENTHOOK, u32, HWND, i32, i32, u32,
//! u32)`) has no user-data/lparam parameter, unlike `EnumWindows`'s
//! callback (see [`crate::win32::enum_windows_callback`]) -- so it cannot
//! receive the config directory as a captured closure or a passed-in
//! pointer. Instead, the config directory (which never changes during the
//! process's lifetime) is stashed once, in [`register`], into a
//! process-wide [`OnceLock`], and read back by the callback on every event.
//! The config itself is deliberately *not* cached anywhere -- the callback
//! reloads it fresh from disk on every foreground-change event, matching
//! this codebase's existing "reload from disk on every trigger" convention
//! (see [`crate::app::handle_toggle_borderless`]/
//! [`crate::app::handle_apply_to_window`]'s own doc comments).
//!
//! # Feature-flag correction
//!
//! `docs/research/idle-game-launch-detection.md` (and this issue's own
//! brief) assumed `SetWinEventHook`/`UnhookWinEvent`/`HWINEVENTHOOK` live in
//! `windows_sys::Win32::UI::WindowsAndMessaging`, the same module as
//! `EVENT_SYSTEM_FOREGROUND`/`WINEVENT_OUTOFCONTEXT` -- verified empirically
//! (per this issue's own instruction to check the locally-cached
//! `windows-sys` 0.61.2 source rather than trust that assumption) to be
//! wrong: they actually live in `windows_sys::Win32::UI::Accessibility`,
//! gated by a *different* Cargo feature, `Win32_UI_Accessibility`, which
//! was not yet enabled and has been added to `Cargo.toml` for this issue.
//! The event constants (`EVENT_SYSTEM_FOREGROUND`, `WINEVENT_OUTOFCONTEXT`)
//! remain in `Win32_UI_WindowsAndMessaging`, already enabled.
//!
//! # Testing
//!
//! OS-facing throughout (`SetWinEventHook`/`UnhookWinEvent` registration and
//! the real callback) -- not exercised by this crate's automated tests, per
//! this crate's established convention (manual usertest only, see
//! `mem:gotchas/window_filter_module_split`-equivalent modules like
//! `tray_ui`/`app`). The match-against-tracked-games decision the callback
//! delegates to ([`crate::app::resolve_auto_apply`], via
//! [`crate::app::handle_auto_apply_launch`]) is the pure, unit-tested part
//! -- see that function's own doc comment and tests in `crate::app::tests`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows_sys::Win32::UI::WindowsAndMessaging::{EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT};

/// The config directory the callback reloads the config from on every
/// foreground-change event -- set once, from [`register`], before the hook
/// is registered; never mutated afterward. See module docs ("No user-data
/// parameter") for why this is a `static` rather than a parameter threaded
/// through the callback.
static WATCHED_CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Registers the `EVENT_SYSTEM_FOREGROUND` WinEvent hook on the calling
/// thread. Must be called before that thread enters its `GetMessageW` pump
/// -- see module docs.
///
/// Returns `None` if `SetWinEventHook` fails (returns a null handle),
/// treated as non-fatal by the caller: matches this codebase's existing
/// "hotkey registration failure doesn't stop the app from starting" pattern
/// ([`crate::app::register_hotkeys`]) -- GameFrame still starts, just
/// without automatic launch detection.
///
/// OS-facing: registers a real Win32 hook. Not exercised by this crate's
/// automated tests -- see module docs.
pub fn register(config_dir: &Path) -> Option<HWINEVENTHOOK> {
    // `get_or_init` rather than a bare `set` so a hypothetical second call
    // (there should only ever be one, from `crate::app::run`) can't panic --
    // it just keeps whichever directory was set first.
    WATCHED_CONFIG_DIR.get_or_init(|| config_dir.to_path_buf());

    // Safety: `win_event_callback` matches the required `WINEVENTPROC`
    // signature exactly; `idProcess = 0, idThread = 0` scopes to every
    // process/thread on the desktop, which is unavoidable here -- the
    // target game process doesn't exist yet at registration time, so
    // filtering happens inside the callback instead (see module docs).
    let hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            std::ptr::null_mut(),
            Some(win_event_callback),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };

    if hook.is_null() { None } else { Some(hook) }
}

/// Tears down a hook returned by [`register`]. Called when
/// [`crate::app::run_event_loop`] returns (the "Exit" path) -- see module
/// docs for why this is worth doing even though the process exits right
/// after.
///
/// OS-facing: unregisters a real Win32 hook. Not exercised by this crate's
/// automated tests -- see module docs.
pub fn unregister(hook: HWINEVENTHOOK) {
    // Safety: `hook` is a handle this module obtained from `SetWinEventHook`
    // in `register` and hasn't already been unhooked.
    unsafe {
        UnhookWinEvent(hook);
    }
}

/// The real `WINEVENTPROC` Windows calls on every `EVENT_SYSTEM_FOREGROUND`
/// change, delivered on the calling thread's message loop (see module
/// docs). Filters defensively (a null `hwnd`) before delegating the actual
/// match-against-tracked-games decision and any resulting apply to
/// [`crate::app::handle_auto_apply_launch`].
///
/// `idobject`/`idchild`/`ideventthread`/`dwmseventtime` are unused:
/// `EVENT_SYSTEM_FOREGROUND`'s own documented semantics are specifically
/// about the foreground window changing (unlike `EVENT_OBJECT_CREATE`,
/// which fires for many non-window UI objects), so no `idObject ==
/// OBJID_WINDOW` check is needed here -- this event is already scoped to
/// real top-level windows.
///
/// OS-facing, and the exact real-hook-delivery path this issue's acceptance
/// criteria call out as manual-usertest-only -- not exercised by this
/// crate's automated tests.
unsafe extern "system" fn win_event_callback(
    _hwineventhook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _idobject: i32,
    _idchild: i32,
    _ideventthread: u32,
    _dwmseventtime: u32,
) {
    if hwnd.is_null() {
        return;
    }

    let Some(config_dir) = WATCHED_CONFIG_DIR.get() else {
        return;
    };

    crate::app::handle_auto_apply_launch(config_dir, hwnd);
}
