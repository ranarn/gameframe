//! Window manipulation for the borderless-toggle mechanic: stripping/restoring
//! a window's border/title bar, resizing/repositioning it, and disabling
//! Windows 11's rounded-corner rendering while it's borderless. Also holds
//! the independent taskbar-autohide mechanic: toggling the shell taskbar
//! between always-visible and auto-hidden via `SHAppBarMessage`. Also owns
//! the "Apply to Window" picker's `EnumWindows` call
//! ([`list_visible_windows`]/`enum_windows_callback`), which calls into
//! [`crate::window_filter`] for the actual two-layer game-window
//! filtering/ranking decision logic (see
//! `docs/research/game-window-filtering.md` for the full rationale).
//!
//! Split the same way as [`crate::settings_ui`]:
//! - Pure decision logic ([`strip_border_style`], [`plan_toggle`],
//!   [`Win32Error::is_access_denied`], [`Win32Error::from_hresult`],
//!   [`toggle_taskbar_autohide_state`], `apply_autohide_state`) has no
//!   `HWND`/OS dependency and is fully unit tested below. [`crate::window_filter`]
//!   follows the identical pure/OS-facing split for the window-filtering/ranking
//!   logic.
//! - The actual Win32 calls further down (`GetForegroundWindow`, `IsIconic`,
//!   `SetWindowLongPtrW`, `SetWindowPos`, `DwmSetWindowAttribute`,
//!   `ShellExecuteW`, `FindWindowW`, `SHAppBarMessage`, `EnumWindows`) are
//!   OS-facing -- they need a real foreground window, a real shell taskbar, or
//!   a real running process to query, which a headless/CI sandbox cannot
//!   provide -- so they are NOT exercised by this crate's automated tests. See
//!   the module's doc comment in the issue writeup for what could/couldn't be
//!   verified.
//!
//! [`toggle_taskbar_autohide`] keeps its own [`TaskbarAutohideState`],
//! completely separate from the borderless mechanic's [`RestoreState`] --
//! the two toggles share no state.

use crate::region_calc::Region;

/// The style bits removed to strip a window's border for the borderless
/// toggle: `WS_CAPTION` is the title bar itself, `WS_THICKFRAME` the
/// resizable sizing border, and `WS_MINIMIZEBOX`/`WS_MAXIMIZEBOX`/`WS_SYSMENU`
/// the title-bar buttons/system menu that have no purpose once the title bar
/// is gone.
const BORDERLESS_STYLE_MASK: u32 = WS_CAPTION | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX | WS_SYSMENU;

/// Removes the border/title-bar style bits from a raw `GWL_STYLE` value,
/// leaving every other bit (e.g. `WS_VISIBLE`, `WS_POPUP`, `WS_CHILD`)
/// untouched. Pure bit manipulation -- no `HWND` involved.
pub fn strip_border_style(style: u32) -> u32 {
    style & !BORDERLESS_STYLE_MASK
}

/// What a foreground window looked like immediately before the borderless
/// hotkey most recently stripped its border -- remembered so a second press
/// of the same hotkey can restore it exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestoreState {
    pub style: u32,
    pub region: Region,
}

/// The window action the borderless-toggle hotkey should perform next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleAction {
    /// Strip the window to `new_style` and move/resize it to `region`.
    MakeBorderless { new_style: u32, region: Region },
    /// Restore the window to exactly the style/region it had before.
    Restore(RestoreState),
}

/// Decides what the borderless-toggle hotkey should do next, and the new
/// `RestoreState` to remember for the press after that.
///
/// - When nothing is remembered (`remembered == None`), the window is
///   currently bordered: strip it to `target_region`, remembering
///   `live_style`/`live_region` so the next press can restore them.
/// - When a `RestoreState` is remembered, the window is currently borderless
///   (from a previous press): restore it to exactly that remembered
///   style/region, and forget it (so the press after *that* starts fresh).
///
/// Pure decision logic -- no OS calls, no real `HWND` -- so it's unit
/// testable in complete isolation from the live Win32 calls that apply it.
pub fn plan_toggle(
    remembered: Option<RestoreState>,
    live_style: u32,
    live_region: Region,
    target_region: Region,
) -> (ToggleAction, Option<RestoreState>) {
    match remembered {
        None => {
            let restore = RestoreState { style: live_style, region: live_region };
            let new_style = strip_border_style(live_style);
            (ToggleAction::MakeBorderless { new_style, region: target_region }, Some(restore))
        }
        Some(restore) => (ToggleAction::Restore(restore), None),
    }
}

use windows_sys::Win32::UI::WindowsAndMessaging::{
    WS_CAPTION, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME,
};

/// The current on/off state of the taskbar auto-hide toggle -- fully
/// independent of [`RestoreState`]/the borderless toggle's state (see this
/// module's doc comment): a distinct type with its own two-state flip, never
/// read or written by the borderless-toggle code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskbarAutohideState {
    #[default]
    AlwaysVisible,
    AutoHidden,
}

/// Decides the next taskbar-autohide state after a press of the bound
/// hotkey: a simple two-state flip. Pure logic -- no OS call, no `HWND` --
/// so it's unit testable in complete isolation from the live
/// `SHAppBarMessage` call that applies it.
pub fn toggle_taskbar_autohide_state(current: TaskbarAutohideState) -> TaskbarAutohideState {
    match current {
        TaskbarAutohideState::AlwaysVisible => TaskbarAutohideState::AutoHidden,
        TaskbarAutohideState::AutoHidden => TaskbarAutohideState::AlwaysVisible,
    }
}

/// Converts a plain `bool` (a [`crate::profile_store::TrackedGame`]'s
/// `taskbar_autohide` field) into a [`TaskbarAutohideState`] -- the
/// interpretation [`set_taskbar_autohide_absolute`] applies. Pure logic --
/// no OS call.
fn taskbar_autohide_state_from_bool(auto_hide: bool) -> TaskbarAutohideState {
    if auto_hide {
        TaskbarAutohideState::AutoHidden
    } else {
        TaskbarAutohideState::AlwaysVisible
    }
}

/// Decides what the zone editor should do to the taskbar on entering an
/// editing session (issue #28's "zones may extend into the taskbar strip"
/// requirement): always force it into auto-hide, regardless of `current` --
/// but remember `current` verbatim so it can be restored exactly on
/// Save/Cancel, mirroring [`plan_toggle`]'s remember-then-restore shape.
/// Pure decision logic -- no OS call.
pub fn plan_taskbar_force_on(current: TaskbarAutohideState) -> (TaskbarAutohideState, TaskbarAutohideState) {
    (TaskbarAutohideState::AutoHidden, current)
}

/// Computes the new `APPBARDATA.lParam` value for
/// `SHAppBarMessage(ABM_SETSTATE, ...)`, given the appbar's `current_flags`
/// (as read via a prior `ABM_GETSTATE` call) and the desired
/// [`TaskbarAutohideState`].
///
/// Per the Win32 `appbar`/`ABM_SETSTATE` documentation, `lParam` is "a
/// combination of" `ABS_AUTOHIDE` and `ABS_ALWAYSONTOP` -- these are
/// independent, combinable bits describing the appbar's FULL desired state,
/// not a delta from whatever it currently is. Sending a hardcoded
/// single-flag value (e.g. `ABS_AUTOHIDE` alone) would silently clear
/// whichever of the two bits wasn't included -- in particular, it would
/// clear the taskbar's always-on-top bit as an unintended side effect of
/// enabling auto-hide. This function instead flips only the single bit being
/// changed (auto-hide) and preserves every other bit (always-on-top) exactly
/// as read from `current_flags`. Pure bit manipulation -- no OS call.
fn apply_autohide_state(current_flags: u32, state: TaskbarAutohideState) -> u32 {
    match state {
        TaskbarAutohideState::AutoHidden => current_flags | ABS_AUTOHIDE,
        TaskbarAutohideState::AlwaysVisible => current_flags & !ABS_AUTOHIDE,
    }
}

/// Reasons a live Win32 window-manipulation call can fail.
#[derive(Debug)]
pub enum Win32Error {
    /// `GetForegroundWindow` returned a null handle (no foreground window).
    NoForegroundWindow,
    /// The window is minimized (`IsIconic`), so its bounding rectangle would
    /// be its off-screen minimized position (roughly `(-32000, -32000)`)
    /// rather than a real, restorable region. Returned by both
    /// [`toggle_borderless_on_foreground_window`] (for the foreground window)
    /// and [`apply_borderless_to_window`] (for an arbitrary tray-picked
    /// window, which is very likely not the foreground window).
    WindowMinimized,
    /// A Win32 call failed; wraps the OS error from `GetLastError`.
    Os(std::io::Error),
    /// An `HRESULT` failure that isn't a `FACILITY_WIN32` code (so it can't
    /// be safely reinterpreted as a plain Win32/`GetLastError`-style error
    /// code) -- see [`Win32Error::from_hresult`].
    Hresult(i32),
    /// `FindWindowW(L"Shell_TrayWnd", ...)` returned a null handle -- the
    /// taskbar's own window couldn't be located, so the taskbar-autohide
    /// toggle has nothing to send `SHAppBarMessage` to.
    TaskbarNotFound,
}

impl Win32Error {
    /// Converts a failing `HRESULT` (e.g. from `DwmSetWindowAttribute`) into
    /// a [`Win32Error`].
    ///
    /// `HRESULT` failure codes are not plain `GetLastError()`-style codes in
    /// general (e.g. `E_ACCESSDENIED` is `0x80070005`, not `5`) -- but codes
    /// in the `FACILITY_WIN32` family (the `0x8007xxxx` range) wrap a plain
    /// Win32 error code in their low 16 bits by construction, so those are
    /// safely unwrapped into [`Win32Error::Os`] (letting
    /// [`is_access_denied`](Self::is_access_denied) and `Display` treat them
    /// identically to a `GetLastError`-sourced failure). Anything outside
    /// that facility is kept as an opaque [`Win32Error::Hresult`] rather than
    /// mislabeled.
    pub fn from_hresult(hresult: i32) -> Self {
        const FACILITY_WIN32_MASK: u32 = 0xFFFF_0000;
        const FACILITY_WIN32: u32 = 0x8007_0000;
        let bits = hresult as u32;
        if bits & FACILITY_WIN32_MASK == FACILITY_WIN32 {
            Win32Error::Os(std::io::Error::from_raw_os_error((bits & 0xFFFF) as i32))
        } else {
            Win32Error::Hresult(hresult)
        }
    }

    /// Whether this error looks like an access-denied failure -- i.e. the
    /// manipulation failed because GameFrame isn't running elevated and the
    /// target window is (e.g. it belongs to a process running as
    /// administrator). This is the signal `app` uses to decide whether to
    /// prompt for elevation rather than just reporting the failure.
    pub fn is_access_denied(&self) -> bool {
        const ERROR_ACCESS_DENIED: i32 = 5;
        matches!(self, Win32Error::Os(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED))
    }
}

impl std::fmt::Display for Win32Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Win32Error::NoForegroundWindow => write!(f, "no foreground window to manipulate"),
            Win32Error::WindowMinimized => write!(f, "window is minimized"),
            Win32Error::Os(e) => write!(f, "Win32 window manipulation failed: {e}"),
            Win32Error::Hresult(hr) => write!(f, "Win32 window manipulation failed: HRESULT {:#010x}", *hr as u32),
            Win32Error::TaskbarNotFound => write!(f, "could not find the taskbar window (Shell_TrayWnd)"),
        }
    }
}

impl std::error::Error for Win32Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Win32Error::NoForegroundWindow => None,
            Win32Error::WindowMinimized => None,
            Win32Error::Os(e) => Some(e),
            Win32Error::Hresult(_) => None,
            Win32Error::TaskbarNotFound => None,
        }
    }
}

use crate::window_filter::{self, WindowRank};
use std::path::Path;
use windows_sys::Win32::Foundation::{CloseHandle, HWND, INVALID_HANDLE_VALUE, RECT};
use windows_sys::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DEFAULT, DWMWCP_DONOTROUND,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::UI::Shell::{
    ABM_GETSTATE, ABM_SETSTATE, ABS_AUTOHIDE, APPBARDATA, SHAppBarMessage, ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetWindowLongPtrW, SetWindowPos, GWL_STYLE,
    SET_WINDOW_POS_FLAGS, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOWNORMAL, WS_EX_APPWINDOW,
    WS_EX_TOOLWINDOW,
};

/// Reads a window's current `GWL_STYLE` value.
///
/// Per `GetWindowLongPtrW`'s documented contract, a return value of zero is
/// ambiguous (it can be either a genuine style value of `0` or a failure),
/// so -- mirroring [`set_window_style`] below -- `SetLastError(0)` is called
/// first and a non-zero `GetLastError()` afterwards is the real failure
/// signal.
fn get_window_style(hwnd: HWND) -> Result<u32, Win32Error> {
    // Safety: `hwnd` is a live handle; `GetWindowLongPtrW` performs no writes
    // and takes no pointers besides `hwnd` itself.
    let style = unsafe {
        windows_sys::Win32::Foundation::SetLastError(0);
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        if style == 0 {
            let last_error = windows_sys::Win32::Foundation::GetLastError();
            if last_error != 0 {
                return Err(Win32Error::Os(std::io::Error::from_raw_os_error(last_error as i32)));
            }
        }
        style
    };
    Ok(style as u32)
}

/// Writes a new `GWL_STYLE` value to a window.
///
/// Per `SetWindowLongPtrW`'s documented contract, a return value of zero is
/// ambiguous (it can be either the previous style value or a failure), so --
/// mirroring [`get_window_style`] above -- `SetLastError(0)` is called first
/// and `GetLastError()` is only trusted as a failure signal when the
/// returned previous-style value is itself `0`; a nonzero return is
/// unambiguous success regardless of what `GetLastError()` reports
/// afterwards.
fn set_window_style(hwnd: HWND, style: u32) -> Result<(), Win32Error> {
    // Safety: `hwnd` is a live handle; `SetWindowLongPtrW` performs no reads
    // besides `hwnd`/`style` and takes no pointers.
    unsafe {
        windows_sys::Win32::Foundation::SetLastError(0);
        let previous_style = SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize);
        if previous_style == 0 {
            let last_error = windows_sys::Win32::Foundation::GetLastError();
            if last_error != 0 {
                return Err(Win32Error::Os(std::io::Error::from_raw_os_error(last_error as i32)));
            }
        }
    }
    Ok(())
}

/// Reads a window's current bounding rectangle (screen coordinates) as a
/// [`Region`].
fn get_window_region(hwnd: HWND) -> Result<Region, Win32Error> {
    let mut rect: RECT = unsafe { std::mem::zeroed() };
    // Safety: `hwnd` is a live handle; `rect` is a valid, correctly-sized
    // out-pointer for `GetWindowRect` to write into.
    let ok = unsafe { GetWindowRect(hwnd, &mut rect) };
    if ok == 0 {
        return Err(Win32Error::Os(std::io::Error::last_os_error()));
    }
    Ok(Region { x: rect.left, y: rect.top, width: rect.right - rect.left, height: rect.bottom - rect.top })
}

/// Moves/resizes a window to `region` without changing its z-order or
/// activation state (the borderless toggle shouldn't steal focus or restack
/// the window relative to its siblings). Includes `SWP_FRAMECHANGED`, which
/// `SetWindowPos`'s documented contract requires after a `GWL_STYLE` change
/// (via [`set_window_style`]) for the non-client area (title bar) to actually
/// be recalculated/repainted -- without it, a target region that happens to
/// already match the window's current bounds could leave the title bar
/// visually unchanged despite the style change underneath.
fn set_window_region(hwnd: HWND, region: Region) -> Result<(), Win32Error> {
    const FLAGS: SET_WINDOW_POS_FLAGS = SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED;
    // Safety: `hwnd` is a live handle; `SetWindowPos` takes only plain
    // integers besides it.
    let ok = unsafe {
        SetWindowPos(hwnd, std::ptr::null_mut(), region.x, region.y, region.width, region.height, FLAGS)
    };
    if ok == 0 {
        return Err(Win32Error::Os(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Sets `DWMWA_WINDOW_CORNER_PREFERENCE` on a window to `preference`.
/// Shared by [`disable_rounded_corners`] (borderless) and
/// [`reset_corner_preference`] (restored to bordered).
fn set_corner_preference(hwnd: HWND, preference: i32) -> Result<(), Win32Error> {
    // Safety: `hwnd` is a live handle; `&preference` is a valid pointer to a
    // correctly-sized (`i32`) buffer matching `cbAttribute`, per
    // `DwmSetWindowAttribute`'s documented contract for this attribute.
    let hresult = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            &preference as *const i32 as *const core::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        )
    };
    if hresult < 0 {
        return Err(Win32Error::from_hresult(hresult));
    }
    Ok(())
}

/// Sets `DWMWA_WINDOW_CORNER_PREFERENCE` to `DWMWCP_DONOTROUND` so Windows
/// 11's rounded corners don't show on a borderless window (they look wrong
/// without a title bar to visually anchor them).
fn disable_rounded_corners(hwnd: HWND) -> Result<(), Win32Error> {
    set_corner_preference(hwnd, DWMWCP_DONOTROUND)
}

/// Resets `DWMWA_WINDOW_CORNER_PREFERENCE` back to `DWMWCP_DEFAULT` when a
/// window is restored to its normal bordered state -- undoes
/// [`disable_rounded_corners`] so a toggle-twice cycle doesn't permanently
/// leave the window's corners square.
fn reset_corner_preference(hwnd: HWND) -> Result<(), Win32Error> {
    set_corner_preference(hwnd, DWMWCP_DEFAULT)
}

/// Strips a window's border and moves/resizes it to `target_region`: the
/// shared "make borderless" mutation shared by
/// [`toggle_borderless_on_foreground_window`] (acting on
/// `GetForegroundWindow()`'s result) and the tray menu's "Apply to Window"
/// picker (acting on an explicit window the user chose, which may not be the
/// foreground window at all -- see [`crate::app`]'s dispatch of a decoded
/// window-item id via [`crate::tray_ui::decode_window_item_id`]). Reads
/// `hwnd`'s current style itself (rather than taking a pre-computed one) so
/// callers only need a target `HWND` and region -- neither caller needs to
/// separately read/strip the style first.
///
/// Unlike [`toggle_borderless_on_foreground_window`], this performs a
/// one-shot strip with no toggle/restore-state tracking: callers that need a
/// remembered "undo" (the hotkey path) layer that on top themselves via
/// [`plan_toggle`]/[`RestoreState`]; callers that don't (the tray-menu path)
/// simply call this directly.
///
/// Mirrors [`toggle_borderless_on_foreground_window`]'s own minimized-window
/// guard: a minimized window's `GetWindowRect` reports its off-screen
/// minimized position (roughly `(-32000, -32000)`), not a real, restorable
/// region, so applying a target region to it would move/resize it based on
/// that bogus rect. `is_listable_window`'s picker filter only checks
/// `IsWindowVisible` (which stays `TRUE` for minimized windows), so a
/// minimized window can legitimately reach this function via the tray-menu
/// path -- this guard is what keeps that path's OS-mutation behavior
/// identical to the hotkey path's, per "the same underlying logic as the
/// borderless hotkey" (issue #8's acceptance criteria), not just sharing the
/// three-call mutation sequence.
///
/// OS-facing: acts on the real window identified by `hwnd`. Not exercised by
/// this crate's automated tests -- see module docs.
pub fn apply_borderless_to_window(hwnd: HWND, target_region: Region) -> Result<(), Win32Error> {
    // Safety: `hwnd` is a live handle passed in by the caller; `IsIconic`
    // performs no writes.
    if unsafe { IsIconic(hwnd) } != 0 {
        return Err(Win32Error::WindowMinimized);
    }

    let live_style = get_window_style(hwnd)?;
    let new_style = strip_border_style(live_style);
    set_window_style(hwnd, new_style)?;
    set_window_region(hwnd, target_region)?;
    disable_rounded_corners(hwnd)?;
    Ok(())
}

/// Performs the borderless-toggle hotkey's full effect on the current
/// foreground window: strips its border and moves it to `target_region` (if
/// nothing is remembered), or restores it to exactly the style/region
/// remembered from the last strip (if something is).
///
/// `state` is read to decide which of those two happens (via [`plan_toggle`])
/// and updated in place to reflect the new remembered state -- callers keep
/// one `state` per hotkey press cycle across calls (see
/// [`crate::app::run_event_loop`]).
///
/// OS-facing: acts on the real foreground window. Not exercised by this
/// crate's automated tests -- see module docs.
pub fn toggle_borderless_on_foreground_window(
    state: &mut Option<RestoreState>,
    target_region: Region,
) -> Result<(), Win32Error> {
    // Safety: `GetForegroundWindow` takes no arguments and performs no
    // writes; its result is null-checked immediately below before any other
    // call uses it.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return Err(Win32Error::NoForegroundWindow);
    }

    // A minimized window's `GetWindowRect` reports its off-screen minimized
    // position (roughly `(-32000, -32000)`), not a real, restorable region
    // -- bail out before that gets read/remembered/applied as if it were
    // one.
    //
    // Safety: `hwnd` is a live handle; `IsIconic` performs no writes.
    if unsafe { IsIconic(hwnd) } != 0 {
        return Err(Win32Error::WindowMinimized);
    }

    let live_style = get_window_style(hwnd)?;
    let live_region = get_window_region(hwnd)?;

    let (action, next_state) = plan_toggle(*state, live_style, live_region, target_region);

    match action {
        ToggleAction::MakeBorderless { region, .. } => {
            // Commit the new remembered state *before* attempting the
            // OS-mutating calls below. `next_state` here is `Some(restore)`
            // derived purely from `live_style`/`live_region`, already safely
            // read above via `GetWindowLongPtrW`/`GetWindowRect`, so
            // committing it early is safe -- and it matters: if a later call
            // in this sequence fails (e.g. the style is stripped but the
            // region move then fails), the original restore state must not
            // be lost. Committing early means the next press still restores
            // the window correctly instead of deriving a wrong "restore"
            // state from an already-partially-mutated window.
            //
            // The actual mutation is delegated to
            // [`apply_borderless_to_window`] (shared with the tray-menu
            // path) rather than repeating its three-call sequence here; it
            // re-reads `hwnd`'s style itself, which is safe since nothing
            // has mutated it between the `live_style` read above and here --
            // the resulting stripped style is identical to plan_toggle's own
            // `new_style` (ignored above via `..`).
            *state = next_state;
            apply_borderless_to_window(hwnd, region)?;
        }
        ToggleAction::Restore(restore) => {
            // Do NOT commit `next_state` (always `None` here) until all three
            // restore calls below have actually succeeded. Unlike the
            // `MakeBorderless` arm, `restore` already holds the window's true
            // original style/region -- clearing `*state` to `None` early
            // would, if one of these calls fails partway through (e.g. the
            // target window closed/crashed between presses), permanently
            // discard the only copy of that original state before it was
            // ever successfully applied, leaving no way to retry or recover
            // it.
            set_window_style(hwnd, restore.style)?;
            set_window_region(hwnd, restore.region)?;
            reset_corner_preference(hwnd)?;
            *state = next_state;
        }
    }

    Ok(())
}

/// Resolves the current foreground window's `HWND`, via `GetForegroundWindow()`.
///
/// Returns `None` if there is no foreground window. Shared by
/// [`foreground_window_exe_path`] (exe-path resolution for the tracked-game
/// lookup) and, per the issue #31 HIGH review fix, by
/// [`crate::app::handle_toggle_borderless`] directly -- it needs the raw
/// `HWND` itself (not just its exe path) to remember *which* window a strip
/// was applied to, so a later restore press can be refused if a different
/// window is now focused (see `crate::app`'s `stripped_hwnd`/
/// `should_allow_restore`).
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs.
pub fn foreground_window() -> Option<HWND> {
    // Safety: `GetForegroundWindow` takes no arguments and performs no
    // writes; its result is null-checked immediately below before any other
    // call uses it.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        None
    } else {
        Some(hwnd)
    }
}

/// Resolves the current foreground window's owning process's executable
/// path, via [`foreground_window`] + [`window_filter::exe_path_for_window`].
///
/// Lets a caller (namely [`crate::app::handle_toggle_borderless`]) decide
/// whether the foreground window belongs to a tracked game *before*
/// deciding whether to proceed at all -- [`toggle_borderless_on_foreground_window`]
/// resolves `GetForegroundWindow()` internally and never surfaces the
/// resolved `HWND` to its caller, so this calls [`foreground_window`]
/// separately rather than sharing a single resolved `HWND` across both; the
/// two calls are expected to resolve to the same window in practice
/// (nothing user-driven runs between them), but even if focus somehow
/// changed between the two calls, the worst case is a stale tracked-game
/// lookup for this one hotkey press -- not a crash or a wrong window being
/// mutated, since the actual mutation still resolves its own `HWND`
/// independently.
///
/// Returns `None` if there is no foreground window, or if the exe path
/// can't be resolved (see [`window_filter::exe_path_for_window`]) -- both
/// treated identically by callers as "no tracked-game lookup possible for
/// this press".
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs.
///
/// Currently has no internal caller: [`crate::app::handle_toggle_borderless`]
/// resolves [`foreground_window`] once and passes that same `HWND` into
/// [`window_filter::exe_path_for_window`] directly, to avoid a second,
/// redundant `GetForegroundWindow()` call (see the HIGH review fix on issue
/// #31 iteration 3). Kept as a small standalone convenience wrapper for any
/// future caller that only needs the exe path and doesn't care about the
/// `HWND` itself.
#[allow(dead_code)]
pub fn foreground_window_exe_path() -> Option<std::path::PathBuf> {
    window_filter::exe_path_for_window(foreground_window()?)
}

/// Toggles taskbar auto-hide on/off: flips `*state` (via
/// [`toggle_taskbar_autohide_state`]) and applies it to the real taskbar via
/// `SHAppBarMessage(ABM_SETSTATE, ...)`.
///
/// Fully independent of [`toggle_borderless_on_foreground_window`]'s
/// `RestoreState` -- `state` here is its own [`TaskbarAutohideState`], never
/// shared with or derived from the borderless toggle's state, so either
/// toggle works whether or not the other has been pressed yet in the same
/// session.
///
/// OS-facing: locates the real taskbar window and sends it a real
/// `SHAppBarMessage`. Not exercised by this crate's automated tests -- see
/// module docs. `*state` is only updated after the OS call so a failure
/// (e.g. the taskbar window can't be found) leaves the remembered state
/// consistent with what was actually last applied.
pub fn toggle_taskbar_autohide(state: &mut TaskbarAutohideState) -> Result<(), Win32Error> {
    let next = toggle_taskbar_autohide_state(*state);
    set_appbar_state(next)?;
    *state = next;
    Ok(())
}

/// Sets the taskbar's auto-hide state to exactly `auto_hide` (an absolute
/// set, not a toggle) -- the primitive the hotkey/tray apply paths call with
/// a tracked game's `taskbar_autohide` field whenever they apply that game's
/// preset (see `docs/adr/0002-taskbar-autohide-coupled-to-preset-apply.md`).
///
/// Deliberately independent of [`toggle_taskbar_autohide`]/`state` above:
/// this never reads or writes a caller-tracked [`TaskbarAutohideState`]
/// variable, so it can't drift out of sync with (or be confused for) the
/// existing global taskbar-autohide hotkey's own toggle mechanic, which ADR
/// 0002 explicitly keeps untouched and independent.
///
/// OS-facing: locates the real taskbar window and sends it a real
/// `SHAppBarMessage`. Not exercised by this crate's automated tests -- see
/// module docs; the `bool` -> [`TaskbarAutohideState`] interpretation itself
/// is pure and unit tested via [`taskbar_autohide_state_from_bool`].
pub fn set_taskbar_autohide_absolute(auto_hide: bool) -> Result<(), Win32Error> {
    set_appbar_state(taskbar_autohide_state_from_bool(auto_hide))
}

/// Locates the taskbar's own window (class name `Shell_TrayWnd`, per the
/// documented convention third-party appbar callers use to address the
/// shell's taskbar itself), reads its CURRENT auto-hide/always-on-top state
/// via `SHAppBarMessage(ABM_GETSTATE, ...)`, then sends
/// `SHAppBarMessage(ABM_SETSTATE, ...)` with only the auto-hide bit flipped
/// (via [`apply_autohide_state`]) -- preserving whatever the always-on-top
/// bit currently is, since `ABM_SETSTATE`'s `lParam` is the appbar's FULL
/// desired state, not a delta (see [`apply_autohide_state`]'s doc comment).
///
/// Per the Win32 `appbar`/`ABM_SETSTATE`/`ABM_GETSTATE` documentation, the
/// appbar being addressed does not need to have been separately registered
/// via `ABM_NEW` -- both messages accept any window handle in
/// `APPBARDATA.hWnd`, and it's documented practice to pass the taskbar's own
/// handle to query/set its auto-hide/always-on-top state directly. For
/// `ABM_GETSTATE`, the current state is returned as `SHAppBarMessage`'s
/// return value itself (not written into `APPBARDATA.lParam`).
fn set_appbar_state(state: TaskbarAutohideState) -> Result<(), Win32Error> {
    let hwnd = find_taskbar_window()?;
    let current_flags = read_appbar_flags(hwnd);
    write_appbar_state(hwnd, current_flags, state);
    Ok(())
}

/// Sends the actual `SHAppBarMessage(ABM_SETSTATE, ...)` write, given a
/// taskbar `hwnd` and its `current_flags` already read via
/// [`read_appbar_flags`]. Split out from [`set_appbar_state`] so callers that
/// already have both in hand (e.g. [`force_taskbar_autohide_on`], which reads
/// them once to derive the state to force) don't have to pay for a redundant
/// [`find_taskbar_window`] + [`read_appbar_flags`] round trip just to write.
fn write_appbar_state(hwnd: HWND, current_flags: u32, state: TaskbarAutohideState) {
    let mut data: APPBARDATA = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of::<APPBARDATA>() as u32;
    data.hWnd = hwnd;
    data.lParam = apply_autohide_state(current_flags, state) as isize;

    // Safety: `data` is a validly-sized, zero-initialized `APPBARDATA` with
    // `cbSize`/`hWnd`/`lParam` populated per `SHAppBarMessage`'s documented
    // `ABM_SETSTATE` contract; `SHAppBarMessage` takes only `dwMessage` and a
    // pointer to `data`, both valid here.
    unsafe {
        SHAppBarMessage(ABM_SETSTATE, &mut data);
    }
}

/// Reads the appbar's current `ABM_GETSTATE` flags (a combination of
/// `ABS_AUTOHIDE`/`ABS_ALWAYSONTOP`) for the taskbar window `hwnd`. Shared
/// by [`set_appbar_state`] (which needs the current flags to preserve the
/// always-on-top bit -- see [`apply_autohide_state`]'s doc comment) and
/// [`force_taskbar_autohide_on`] (which needs only the auto-hide bit, via
/// [`state_from_flags`]).
fn read_appbar_flags(hwnd: HWND) -> u32 {
    let mut data: APPBARDATA = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of::<APPBARDATA>() as u32;
    data.hWnd = hwnd;

    // Safety: `data` is a validly-sized, zero-initialized `APPBARDATA` with
    // `cbSize`/`hWnd` populated per `SHAppBarMessage`'s documented
    // `ABM_GETSTATE` contract; `SHAppBarMessage` takes only `dwMessage` and a
    // pointer to `data`, both valid here. `ABM_GETSTATE` does not read
    // `data.lParam`, so it's left zeroed.
    unsafe { SHAppBarMessage(ABM_GETSTATE, &mut data) as u32 }
}

/// Interprets `ABM_GETSTATE`'s raw flags (as read by [`read_appbar_flags`])
/// as a [`TaskbarAutohideState`]. Used by [`force_taskbar_autohide_on`],
/// which reads the taskbar's actual current auto-hide state directly from
/// the OS (as opposed to whatever a caller's own [`TaskbarAutohideState`]
/// variable happens to say -- that variable, e.g. `app::run_event_loop`'s
/// `taskbar_autohide_state`, is local to a different call site and may not
/// be in scope where the zone editor is launched from) without a separate
/// [`find_taskbar_window`] + [`read_appbar_flags`] round trip of its own.
fn state_from_flags(current_flags: u32) -> TaskbarAutohideState {
    if current_flags & ABS_AUTOHIDE != 0 {
        TaskbarAutohideState::AutoHidden
    } else {
        TaskbarAutohideState::AlwaysVisible
    }
}

/// Forces the taskbar into auto-hide for the duration of a zone-editor
/// session (issue #28: a drawn zone may extend into the taskbar's screen
/// strip), returning whatever state was actually current beforehand so the
/// caller can restore it exactly via [`restore_taskbar_autohide`] once the
/// session ends (Save or Cancel).
///
/// OS-facing: queries and sets the real taskbar's appbar state. Not
/// exercised by this crate's automated tests -- see module docs. The
/// decision of what to force/remember is pure and tested separately via
/// [`plan_taskbar_force_on`].
pub fn force_taskbar_autohide_on() -> Result<TaskbarAutohideState, Win32Error> {
    // Resolves the taskbar window and reads its flags once, then reuses both
    // for the write below -- avoids a redundant second `find_taskbar_window`
    // + `read_appbar_flags` round trip that a naive query-then-set
    // implementation would incur.
    let hwnd = find_taskbar_window()?;
    let current_flags = read_appbar_flags(hwnd);
    let current = state_from_flags(current_flags);
    let (to_apply, to_remember) = plan_taskbar_force_on(current);
    write_appbar_state(hwnd, current_flags, to_apply);
    Ok(to_remember)
}

/// Restores the taskbar to exactly the state remembered by a prior
/// [`force_taskbar_autohide_on`] call.
pub fn restore_taskbar_autohide(remembered: TaskbarAutohideState) -> Result<(), Win32Error> {
    set_appbar_state(remembered)
}

/// Finds the taskbar's own window handle via `FindWindowW(L"Shell_TrayWnd", NULL)`.
fn find_taskbar_window() -> Result<HWND, Win32Error> {
    let class_name = encode_wide("Shell_TrayWnd");
    // Safety: `class_name` is a valid, null-terminated UTF-16 buffer kept
    // alive for the duration of this call; the window-name pointer is null,
    // which `FindWindowW` documents as "match any window title".
    let hwnd = unsafe { FindWindowW(class_name.as_ptr(), std::ptr::null()) };
    if hwnd.is_null() {
        return Err(Win32Error::TaskbarNotFound);
    }
    Ok(hwnd)
}

/// Re-launches the current executable elevated (UAC "runas" prompt),
/// following `ShellExecuteW`'s documented `"runas"` verb contract.
///
/// Used when a window manipulation fails with an access-denied-style error
/// ([`Win32Error::is_access_denied`]) -- GameFrame runs unelevated by
/// default and only asks for elevation reactively, at the point a specific
/// operation actually needs it, rather than requiring admin rights upfront.
///
/// OS-facing (spawns a real elevated child process via a UAC prompt); not
/// exercised by this crate's automated tests -- see module docs.
pub fn relaunch_elevated() -> Result<(), Win32Error> {
    let exe_path = std::env::current_exe().map_err(Win32Error::Os)?;
    let verb = encode_wide("runas");
    let file = encode_wide(exe_path.to_string_lossy().as_ref());

    // Safety: `verb` and `file` are valid, null-terminated UTF-16 buffers
    // kept alive for the duration of this call; the remaining pointer
    // arguments (`hwnd`, `lpparameters`, `lpdirectory`) are null, which
    // `ShellExecuteW` documents as valid ("use current/none").
    let result = unsafe {
        ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL)
    };
    // Per ShellExecuteW's documented contract, a return value greater than
    // 32 indicates success; anything else IS the failure code itself (e.g.
    // distinguishing a declined UAC prompt from other failures) -- unlike
    // most Win32 calls, ShellExecuteW is not documented to route failures
    // through `GetLastError()`, so the error must be built from `result`
    // rather than from a (potentially unrelated/misleading) `GetLastError()`
    // call.
    let code = result as isize;
    if code <= 32 {
        return Err(Win32Error::Os(std::io::Error::from_raw_os_error(code as i32)));
    }
    Ok(())
}

/// Encodes a Rust string as a null-terminated UTF-16 buffer, the string
/// format every wide (`*W`) Win32 API used here expects.
fn encode_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Per-`EnumWindows`-call userdata smuggled through `EnumWindows`'s `LPARAM`
/// parameter: the listable windows gathered so far, plus a per-PID
/// process-level-signal memoization cache (see
/// [`window_filter::rank_window_for_hwnd`]) so a process owning multiple
/// top-level listable windows only pays for one launcher-match/graphics-
/// module gather instead of one per window -- the window-class signal is
/// deliberately excluded from this cache since it's a per-window, not
/// per-process, property.
struct EnumWindowsState {
    windows: Vec<(HWND, String, WindowRank)>,
    rank_cache: std::collections::HashMap<u32, window_filter::ProcessRankSignals>,
}

pub fn list_visible_windows() -> Vec<(HWND, String)> {
    let mut state = EnumWindowsState { windows: Vec::new(), rank_cache: std::collections::HashMap::new() };
    // Safety: `state` outlives this call (it's a local that isn't dropped
    // until after `EnumWindows` returns); it's cast to a raw pointer solely
    // to smuggle it through `EnumWindows`'s documented `LPARAM` userdata
    // parameter, and `enum_windows_callback` below casts it back to the
    // exact same type before dereferencing it.
    unsafe {
        EnumWindows(Some(enum_windows_callback), std::ptr::addr_of_mut!(state) as isize);
    }
    // Layer 2 (see `docs/research/game-window-filtering.md`): a stable sort
    // by descending `WindowRank` surfaces likely games first in the picker,
    // without ever removing a window ranked `WindowRank::Unknown` -- ranking
    // only ever reorders, it never filters. `Vec::sort_by` is a stable sort,
    // so windows that tie on rank keep the relative order `EnumWindows`
    // produced them in (roughly Z-order).
    state.windows.sort_by(|a, b| b.2.cmp(&a.2));
    state.windows.into_iter().map(|(hwnd, title, _rank)| (hwnd, title)).collect()
}

unsafe extern "system" fn enum_windows_callback(hwnd: HWND, lparam: isize) -> i32 {
    // Safety: `lparam` is exactly the pointer `list_visible_windows` passed
    // in, valid for the duration of the `EnumWindows` call that is
    // synchronously invoking this callback.
    let state = unsafe { &mut *(lparam as *mut EnumWindowsState) };

    // Safety: `hwnd` is a live handle supplied by `EnumWindows` itself;
    // `IsWindowVisible` performs no writes.
    let is_visible = unsafe { IsWindowVisible(hwnd) } != 0;
    let title = window_text(hwnd);
    let is_cloaked = window_filter::is_window_cloaked(hwnd);
    let ex_style = window_filter::window_ex_style(hwnd);
    let has_tool_window_style = ex_style & WS_EX_TOOLWINDOW != 0;
    let has_app_window_style = ex_style & WS_EX_APPWINDOW != 0;
    let is_representative_window = window_filter::is_root_owner_or_last_active_popup(hwnd);

    if window_filter::is_listable_window(
        &title,
        is_visible,
        is_cloaked,
        is_representative_window,
        has_tool_window_style,
        has_app_window_style,
    ) {
        let rank = window_filter::rank_window_for_hwnd(hwnd, &mut state.rank_cache);
        state.windows.push((hwnd, title, rank));
    }

    1 // TRUE: keep enumerating.
}

/// Reads a window's title via `GetWindowTextLengthW`/`GetWindowTextW`,
/// returning an empty string if the window has no title (or either call
/// fails) rather than propagating an error -- an untitled window is simply
/// filtered out by [`is_listable_window`], not treated as a hard failure of
/// the whole enumeration.
pub(crate) fn window_text(hwnd: HWND) -> String {
    // Safety: `hwnd` is a live handle; `GetWindowTextLengthW` performs no
    // writes.
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }

    let mut buffer = vec![0u16; len as usize + 1];
    // Safety: `hwnd` is a live handle; `buffer` is a valid, correctly-sized
    // (`len + 1`, matching `nMaxCount`) out-buffer for `GetWindowTextW` to
    // write into, per its documented contract.
    let copied = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if copied <= 0 {
        return String::new();
    }

    String::from_utf16_lossy(&buffer[..copied as usize])
}

/// Picks a single window among `candidates` (each a live top-level window's
/// `HWND` paired with its screen area in pixels, gathered by
/// [`windows_for_pid`]) to treat as "the" window for issue #33's live
/// re-apply, when a tracked game's process owns more than one -- the pure
/// decision behind [`resolve_hwnd_for_pid`], separated out so it's testable
/// without a real `EnumWindows`/`GetWindowRect`/`GetForegroundWindow` call.
///
/// Disambiguation order, per this issue's acceptance criteria: prefer the
/// window matching `foreground` (the live `GetForegroundWindow()` result), if
/// it's among `candidates`; otherwise the largest by `area`; otherwise the
/// first one gathered (a tie in area also falls through to "first found",
/// since ties are broken by strict-greater-than below).
///
/// Pure -- plain data in, plain data out -- so it's unit tested in complete
/// isolation from the real window enumeration/measurement.
fn choose_candidate_window(candidates: &[(HWND, i64)], foreground: Option<HWND>) -> Option<HWND> {
    if foreground.is_some_and(|fg| candidates.iter().any(|&(hwnd, _)| hwnd == fg)) {
        return foreground;
    }

    let mut best: Option<(HWND, i64)> = None;
    for &(hwnd, area) in candidates {
        let is_better = match best {
            Some((_, best_area)) => area > best_area,
            None => true,
        };
        if is_better {
            best = Some((hwnd, area));
        }
    }
    best.map(|(hwnd, _)| hwnd)
}

/// Cheap, case-insensitive pre-filter comparing a toolhelp snapshot entry's
/// bare file name (`PROCESSENTRY32W::szExeFile`, which never carries a
/// directory component) against `target_exe_path`'s own file name -- the
/// first of the two checks [`find_pid_by_exe_path`] applies before accepting
/// a PID match; a full-path confirmation via
/// [`crate::window_filter::process_exe_path`] still follows, so two
/// processes that merely share an executable *name* in different
/// directories are never confused for the same tracked game.
///
/// Pure string comparison -- no OS dependency -- so it's unit tested in
/// complete isolation from the real toolhelp snapshot.
fn exe_file_name_matches(candidate_file_name: &str, target_exe_path: &Path) -> bool {
    target_exe_path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|target_name| target_name.eq_ignore_ascii_case(candidate_file_name))
}

/// Per-[`windows_for_pid`]-call userdata smuggled through `EnumWindows`'s
/// `LPARAM` parameter -- the target PID to filter on, plus the listable
/// windows gathered so far. Analogous to [`EnumWindowsState`], but scoped to
/// a single PID rather than the whole desktop, and with no ranking (issue
/// #33's live re-apply doesn't need a "most game-like" ordering, only a set
/// of candidates to disambiguate via [`choose_candidate_window`]).
struct PidWindowsState {
    target_pid: u32,
    windows: Vec<HWND>,
}

unsafe extern "system" fn enum_windows_for_pid_callback(hwnd: HWND, lparam: isize) -> i32 {
    // Safety: `lparam` is exactly the pointer `windows_for_pid` passed in,
    // valid for the duration of the `EnumWindows` call that is synchronously
    // invoking this callback.
    let state = unsafe { &mut *(lparam as *mut PidWindowsState) };

    let mut window_pid: u32 = 0;
    // Safety: `hwnd` is a live handle supplied by `EnumWindows` itself;
    // `&mut window_pid` is a valid out-pointer for
    // `GetWindowThreadProcessId`'s documented contract.
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut window_pid);
    }
    if window_pid != state.target_pid {
        return 1; // TRUE: keep enumerating -- not this process's window.
    }

    // The same Layer 1 canonical-window formula `enum_windows_callback` uses
    // for the "Apply to Window" picker (issue #27), via the shared
    // `window_filter::is_canonical_window` predicate -- minus the non-blank
    // -title requirement `is_listable_window` layers on top of it: a live
    // re-apply target doesn't need a human-readable title, only to be the
    // representative top-level window for its process.
    // Safety: `hwnd` is a live handle; `IsWindowVisible` performs no writes.
    let is_visible = unsafe { IsWindowVisible(hwnd) } != 0;
    let is_cloaked = window_filter::is_window_cloaked(hwnd);
    let ex_style = window_filter::window_ex_style(hwnd);
    let has_tool_window_style = ex_style & WS_EX_TOOLWINDOW != 0;
    let has_app_window_style = ex_style & WS_EX_APPWINDOW != 0;
    let is_representative_window = window_filter::is_root_owner_or_last_active_popup(hwnd);

    if window_filter::is_canonical_window(
        is_visible,
        is_cloaked,
        is_representative_window,
        has_tool_window_style,
        has_app_window_style,
    ) {
        state.windows.push(hwnd);
    }

    1 // TRUE: keep enumerating.
}

/// Enumerates every top-level window belonging to `target_pid` that passes
/// the same Layer 1 canonical-window filtering the "Apply to Window" picker
/// uses (see [`enum_windows_for_pid_callback`]) -- the candidate-gathering
/// step behind [`resolve_hwnd_for_pid`]. A separate, PID-scoped enumeration
/// rather than a `pid` filter parameter added to [`list_visible_windows`]
/// itself, so that function (and the picker it serves) is untouched by this
/// issue.
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs.
fn windows_for_pid(target_pid: u32) -> Vec<HWND> {
    let mut state = PidWindowsState { target_pid, windows: Vec::new() };
    // Safety: `state` outlives this call; see `list_visible_windows`'s
    // identical `EnumWindows` safety comment for why casting it through a
    // raw pointer here is sound.
    unsafe {
        EnumWindows(Some(enum_windows_for_pid_callback), std::ptr::addr_of_mut!(state) as isize);
    }
    state.windows
}

/// Looks up the PID of a running process whose executable path is
/// `exe_path`, via a `TH32CS_SNAPPROCESS` toolhelp snapshot. Each entry's
/// bare file name (`PROCESSENTRY32W::szExeFile`) is first cheaply
/// pre-filtered via [`exe_file_name_matches`], then confirmed with a
/// full-path comparison via [`window_filter::process_exe_path`] before being
/// accepted -- guarding against two different processes that merely happen
/// to share an executable *name* in different install directories.
///
/// Returns the first live match found; `None` if no running process matches
/// (the common case for a tracked game that isn't currently running -- see
/// [`resolve_running_window`]'s doc comment).
///
/// The file-name pre-filter above is case-insensitive, but the full-path
/// confirmation that follows it is a plain, case-sensitive string comparison
/// against `exe_path`. Same accepted-risk rationale as
/// `profile_store::upsert_tracked_game`'s doc comment: both sides of the
/// comparison are resolved via the same OS API
/// (`QueryFullProcessImageNameW`, see [`window_filter::process_exe_path`]),
/// which returns a consistent casing run-to-run, so a casing mismatch
/// between the two is not expected in practice.
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs; [`exe_file_name_matches`] above is the pure decision this pre-filter
/// delegates to, and is what's actually unit tested.
fn find_pid_by_exe_path(exe_path: &Path) -> Option<u32> {
    // Safety: `0` (ignored for `TH32CS_SNAPPROCESS`) is the only argument;
    // `CreateToolhelp32Snapshot` takes no pointers.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }

    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

    // Safety: `snapshot` is the live handle just created above; `entry` is a
    // valid, correctly-sized out-pointer with `dwSize` set, per
    // `Process32FirstW`/`Process32NextW`'s documented contract.
    let mut has_entry = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    let mut found_pid = None;
    while has_entry {
        let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
        let file_name = String::from_utf16_lossy(&entry.szExeFile[..len]);

        if exe_file_name_matches(&file_name, exe_path)
            && window_filter::process_exe_path(entry.th32ProcessID).as_deref() == Some(exe_path)
        {
            found_pid = Some(entry.th32ProcessID);
            break;
        }

        // Safety: same as above.
        has_entry = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }

    // Safety: `snapshot` is a valid handle owned solely by this function; no
    // other reference to it survives past this point.
    unsafe {
        CloseHandle(snapshot);
    }
    found_pid
}

/// Resolves `hwnd`'s current on-screen area in pixels (width * height), used
/// solely as [`choose_candidate_window`]'s size-disambiguation signal.
/// Returns `0` if the window's rect can't be read -- an unreadable window
/// simply never wins on size, it can still win via the foreground-match or
/// first-found rules.
fn window_area(hwnd: HWND) -> i64 {
    get_window_region(hwnd).map(|r| r.width as i64 * r.height as i64).unwrap_or(0)
}

/// Picks a single top-level window to treat as "the" window for `pid`,
/// gathering [`windows_for_pid`]'s candidates (paired with their
/// [`window_area`]) and the live [`foreground_window`] result, then deferring
/// the actual choice to the pure [`choose_candidate_window`].
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs; [`choose_candidate_window`] above is the pure decision this
/// delegates to, and is what's actually unit tested.
fn resolve_hwnd_for_pid(pid: u32) -> Option<HWND> {
    let candidates: Vec<(HWND, i64)> =
        windows_for_pid(pid).into_iter().map(|hwnd| (hwnd, window_area(hwnd))).collect();
    choose_candidate_window(&candidates, foreground_window())
}

/// Resolves a running window for `exe_path`, for issue #33's live re-apply:
/// no live window handle is persisted in config, so each commit-triggered
/// re-apply must rediscover the tracked game's current window (if any) from
/// scratch via a fresh toolhelp snapshot + window enumeration.
///
/// Returns `None` if no running process matches `exe_path` (the game isn't
/// currently running -- callers must silently skip it, per this issue's
/// acceptance criteria: its setting still saves normally and applies on its
/// next launch/hotkey/tray-apply) or if one does but it has no listable
/// top-level window (e.g. still starting up with no window shown yet).
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs.
pub(crate) fn resolve_running_window(exe_path: &Path) -> Option<HWND> {
    let pid = find_pid_by_exe_path(exe_path)?;
    resolve_hwnd_for_pid(pid)
}

/// The outcome of a single tracked game's live re-apply attempt (see
/// [`live_reapply_running_window`]), for the caller (`crate::settings_ui`) to
/// decide what -- if anything -- to surface in its non-blocking status line.
///
/// Deliberately has no `NotRunning` variant: whether the game is currently
/// running is decided by the caller *before* this function (or a region) is
/// ever invoked -- via [`resolve_running_window`] -- so that a not-running
/// game always short-circuits ahead of any region-resolution error and is
/// never routed through this type at all (see issue #33's iteration-3 review
/// fix: region resolution must never run, let alone be reported as a
/// failure, for a game that was never actually running).
pub(crate) enum LiveApplyOutcome {
    /// The window was found and successfully repositioned (taskbar-autohide
    /// may or may not have also been set -- a failure there is only logged,
    /// never escalated, matching every other apply path's established
    /// "gate taskbar-set on border-apply success, but don't fail the whole
    /// operation over a taskbar-only failure" pattern).
    Applied,
    /// The window was found, but repositioning it failed (e.g. access
    /// denied). Deliberately never triggers [`relaunch_elevated`] -- see this
    /// function's doc comment for why reactive elevation must stay scoped to
    /// the hotkey path only.
    Failed(Win32Error),
}

/// Live re-apply for a single tracked game (issue #33), given its
/// already-resolved current running window `hwnd`: strips its border and
/// repositions it to `target_region` ([`apply_borderless_to_window`]), then
/// -- only on that step's success -- sets its taskbar-auto-hide value
/// absolutely ([`set_taskbar_autohide_absolute`]), mirroring the same "gate
/// taskbar-set on border-apply success" pattern already established by the
/// hotkey path, the tray "Apply to Window" path, and the launch-watcher path
/// (see `crate::app::handle_toggle_borderless`/`handle_apply_to_window`/
/// `handle_auto_apply_launch`) -- this is the fourth call site for that exact
/// pattern, so getting it right here means simply mirroring the same
/// early-return-on-border-failure shape those three already use, not
/// re-deriving it.
///
/// Deliberately takes an already-resolved `hwnd` rather than an `exe_path`:
/// resolving the running window (via [`resolve_running_window`]) is the
/// caller's (`crate::settings_ui`) responsibility, and must happen *before*
/// the caller resolves `target_region` -- a not-running game must never
/// reach this function at all, and must never cause a region-resolution
/// error to be reported as a live-apply failure (issue #33's iteration-3
/// review fix).
///
/// A taskbar-set failure (after a successful border-apply) is only logged,
/// never escalated to [`LiveApplyOutcome::Failed`] -- the window was already
/// successfully repositioned, so treating the whole attempt as failed (and
/// showing "will apply on next launch" for it) would be misleading.
///
/// Deliberately never calls [`relaunch_elevated`] on an access-denied
/// failure, unlike the hotkey path: this runs synchronously inside the
/// Settings window's own commit handlers, and `relaunch_elevated` restarts
/// the whole app -- doing so here would destroy the in-progress Settings
/// session out from under the user for a comparatively minor, easily
/// retried-later failure. Reactive elevation stays scoped to the hotkey path
/// only, per this issue's explicit out-of-scope note.
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs.
pub(crate) fn live_reapply_running_window(
    hwnd: HWND,
    display_name: &str,
    taskbar_autohide: bool,
    target_region: Region,
) -> LiveApplyOutcome {
    if let Err(e) = apply_borderless_to_window(hwnd, target_region) {
        return LiveApplyOutcome::Failed(e);
    }

    if let Err(e) = set_taskbar_autohide_absolute(taskbar_autohide) {
        eprintln!("live re-apply: failed to set taskbar auto-hide for {display_name}: {e}");
    }

    LiveApplyOutcome::Applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::region_calc::Region;
    use windows_sys::Win32::UI::Shell::ABS_ALWAYSONTOP;

    #[test]
    fn strip_border_style_removes_caption_and_thick_frame_bits() {
        let style = WS_CAPTION | WS_THICKFRAME;

        let stripped = strip_border_style(style);

        assert_eq!(stripped & WS_CAPTION, 0);
        assert_eq!(stripped & WS_THICKFRAME, 0);
    }

    #[test]
    fn strip_border_style_removes_the_system_menu_and_min_max_boxes() {
        let style = WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX;

        let stripped = strip_border_style(style);

        assert_eq!(stripped, 0);
    }

    #[test]
    fn strip_border_style_leaves_unrelated_style_bits_untouched() {
        const WS_VISIBLE: u32 = 0x10000000;
        let style = WS_CAPTION | WS_VISIBLE;

        let stripped = strip_border_style(style);

        assert_eq!(stripped, WS_VISIBLE);
    }

    #[test]
    fn plan_toggle_with_nothing_remembered_strips_the_border_and_remembers_the_live_state() {
        let live_style = WS_CAPTION | WS_THICKFRAME;
        let live_region = Region { x: 10, y: 20, width: 800, height: 600 };
        let target_region = Region { x: 0, y: 0, width: 960, height: 1080 };

        let (action, next_state) = plan_toggle(None, live_style, live_region, target_region);

        assert_eq!(
            action,
            ToggleAction::MakeBorderless { new_style: strip_border_style(live_style), region: target_region }
        );
        assert_eq!(next_state, Some(RestoreState { style: live_style, region: live_region }));
    }

    #[test]
    fn plan_toggle_with_a_remembered_state_restores_it_and_forgets_it() {
        let remembered = RestoreState {
            style: WS_CAPTION | WS_THICKFRAME,
            region: Region { x: 10, y: 20, width: 800, height: 600 },
        };
        // Live style/region reflect the currently-borderless window; they're
        // irrelevant to this branch since we're restoring the remembered
        // state, not the live one.
        let live_style = strip_border_style(remembered.style);
        let target_region = Region { x: 0, y: 0, width: 960, height: 1080 };

        let (action, next_state) = plan_toggle(Some(remembered), live_style, target_region, target_region);

        assert_eq!(action, ToggleAction::Restore(remembered));
        assert_eq!(next_state, None);
    }

    #[test]
    fn pressing_the_hotkey_twice_round_trips_back_to_the_original_style_and_region() {
        let original_style = WS_CAPTION | WS_THICKFRAME | WS_SYSMENU;
        let original_region = Region { x: 10, y: 20, width: 800, height: 600 };
        let target_region = Region { x: 0, y: 0, width: 960, height: 1080 };

        let (first_action, state_after_first) =
            plan_toggle(None, original_style, original_region, target_region);
        let ToggleAction::MakeBorderless { new_style: stripped_style, .. } = first_action else {
            panic!("expected MakeBorderless on the first press");
        };

        let (second_action, state_after_second) =
            plan_toggle(state_after_first, stripped_style, target_region, target_region);

        assert_eq!(
            second_action,
            ToggleAction::Restore(RestoreState { style: original_style, region: original_region })
        );
        assert_eq!(state_after_second, None);
    }

    #[test]
    fn toggle_taskbar_autohide_state_flips_from_always_visible_to_auto_hidden() {
        assert_eq!(
            toggle_taskbar_autohide_state(TaskbarAutohideState::AlwaysVisible),
            TaskbarAutohideState::AutoHidden
        );
    }

    #[test]
    fn choose_candidate_window_returns_none_for_no_candidates() {
        assert_eq!(choose_candidate_window(&[], None), None);
    }

    #[test]
    fn choose_candidate_window_returns_the_sole_candidate() {
        let hwnd = 1usize as HWND;

        assert_eq!(choose_candidate_window(&[(hwnd, 100)], None), Some(hwnd));
    }

    #[test]
    fn choose_candidate_window_prefers_the_foreground_window_over_a_larger_candidate() {
        let foreground = 1usize as HWND;
        let larger = 2usize as HWND;

        let chosen = choose_candidate_window(&[(foreground, 10), (larger, 10_000)], Some(foreground));

        assert_eq!(chosen, Some(foreground));
    }

    #[test]
    fn choose_candidate_window_falls_back_to_the_largest_when_foreground_is_not_a_candidate() {
        let small = 1usize as HWND;
        let large = 2usize as HWND;
        let elsewhere = 3usize as HWND;

        let chosen = choose_candidate_window(&[(small, 10), (large, 10_000)], Some(elsewhere));

        assert_eq!(chosen, Some(large));
    }

    #[test]
    fn choose_candidate_window_falls_back_to_the_largest_when_there_is_no_foreground_window() {
        let small = 1usize as HWND;
        let large = 2usize as HWND;

        let chosen = choose_candidate_window(&[(small, 10), (large, 10_000)], None);

        assert_eq!(chosen, Some(large));
    }

    #[test]
    fn choose_candidate_window_prefers_the_first_found_when_areas_tie() {
        let first = 1usize as HWND;
        let second = 2usize as HWND;

        let chosen = choose_candidate_window(&[(first, 500), (second, 500)], None);

        assert_eq!(chosen, Some(first));
    }

    #[test]
    fn exe_file_name_matches_an_exact_case_insensitive_file_name() {
        let target = std::path::Path::new("C:\\Games\\Thing\\thing.exe");

        assert!(exe_file_name_matches("THING.EXE", target));
    }

    #[test]
    fn exe_file_name_matches_returns_false_for_a_different_file_name() {
        let target = std::path::Path::new("C:\\Games\\Thing\\thing.exe");

        assert!(!exe_file_name_matches("other.exe", target));
    }

    #[test]
    fn exe_file_name_matches_ignores_directory_components_of_the_target() {
        // The toolhelp snapshot's `szExeFile` is a bare file name, never a
        // full path -- this only ever compares against `target`'s own file
        // name, regardless of which directory it lives in.
        let target = std::path::Path::new("D:\\SomewhereElse\\thing.exe");

        assert!(exe_file_name_matches("thing.exe", target));
    }

    #[test]
    fn toggle_taskbar_autohide_state_flips_from_auto_hidden_back_to_always_visible() {
        assert_eq!(
            toggle_taskbar_autohide_state(TaskbarAutohideState::AutoHidden),
            TaskbarAutohideState::AlwaysVisible
        );
    }

    #[test]
    fn taskbar_autohide_state_default_is_always_visible() {
        // Fresh install / first press starts from "always visible" -- the
        // taskbar's normal, un-toggled state.
        assert_eq!(TaskbarAutohideState::default(), TaskbarAutohideState::AlwaysVisible);
    }

    #[test]
    fn pressing_the_taskbar_hotkey_twice_round_trips_back_to_always_visible() {
        let state = TaskbarAutohideState::default();

        let after_first = toggle_taskbar_autohide_state(state);
        let after_second = toggle_taskbar_autohide_state(after_first);

        assert_eq!(after_first, TaskbarAutohideState::AutoHidden);
        assert_eq!(after_second, TaskbarAutohideState::AlwaysVisible);
    }

    #[test]
    fn apply_autohide_state_sets_the_autohide_bit_when_no_bits_are_currently_set() {
        assert_eq!(apply_autohide_state(0, TaskbarAutohideState::AutoHidden), ABS_AUTOHIDE);
    }

    #[test]
    fn apply_autohide_state_preserves_the_always_on_top_bit_when_enabling_auto_hide() {
        // ABM_SETSTATE's lParam is a *combination* of ABS_AUTOHIDE and
        // ABS_ALWAYSONTOP describing the appbar's full desired state, not a
        // delta -- so enabling auto-hide on a taskbar that currently has
        // ABS_ALWAYSONTOP set must preserve that bit rather than silently
        // clearing it.
        let current_flags = ABS_ALWAYSONTOP;

        let new_flags = apply_autohide_state(current_flags, TaskbarAutohideState::AutoHidden);

        assert_eq!(new_flags, ABS_AUTOHIDE | ABS_ALWAYSONTOP);
    }

    #[test]
    fn apply_autohide_state_clears_only_the_autohide_bit_when_restoring_always_visible() {
        let current_flags = ABS_AUTOHIDE | ABS_ALWAYSONTOP;

        let new_flags = apply_autohide_state(current_flags, TaskbarAutohideState::AlwaysVisible);

        assert_eq!(new_flags, ABS_ALWAYSONTOP);
    }

    #[test]
    fn apply_autohide_state_restoring_always_visible_does_not_add_an_always_on_top_bit_that_was_not_already_set() {
        let current_flags = ABS_AUTOHIDE;

        let new_flags = apply_autohide_state(current_flags, TaskbarAutohideState::AlwaysVisible);

        assert_eq!(new_flags, 0);
    }

    #[test]
    fn plan_taskbar_force_on_always_forces_auto_hidden_regardless_of_current_state() {
        let (to_apply, _) = plan_taskbar_force_on(TaskbarAutohideState::AlwaysVisible);
        assert_eq!(to_apply, TaskbarAutohideState::AutoHidden);

        let (to_apply, _) = plan_taskbar_force_on(TaskbarAutohideState::AutoHidden);
        assert_eq!(to_apply, TaskbarAutohideState::AutoHidden);
    }

    #[test]
    fn plan_taskbar_force_on_remembers_whatever_was_current_before_forcing() {
        let (_, to_remember) = plan_taskbar_force_on(TaskbarAutohideState::AlwaysVisible);
        assert_eq!(to_remember, TaskbarAutohideState::AlwaysVisible);

        let (_, to_remember) = plan_taskbar_force_on(TaskbarAutohideState::AutoHidden);
        assert_eq!(to_remember, TaskbarAutohideState::AutoHidden);
    }

    #[test]
    fn taskbar_autohide_state_from_bool_true_maps_to_auto_hidden() {
        assert_eq!(taskbar_autohide_state_from_bool(true), TaskbarAutohideState::AutoHidden);
    }

    #[test]
    fn taskbar_autohide_state_from_bool_false_maps_to_always_visible() {
        assert_eq!(taskbar_autohide_state_from_bool(false), TaskbarAutohideState::AlwaysVisible);
    }

    #[test]
    fn win32_error_is_access_denied_recognizes_error_code_5() {
        let err = Win32Error::Os(std::io::Error::from_raw_os_error(5));

        assert!(err.is_access_denied());
    }

    #[test]
    fn win32_error_is_access_denied_is_false_for_other_error_codes() {
        let err = Win32Error::Os(std::io::Error::from_raw_os_error(2)); // ERROR_FILE_NOT_FOUND

        assert!(!err.is_access_denied());
    }

    #[test]
    fn win32_error_no_foreground_window_is_not_access_denied() {
        assert!(!Win32Error::NoForegroundWindow.is_access_denied());
    }

    #[test]
    fn win32_error_implements_display_and_std_error() {
        fn assert_is_std_error<E: std::error::Error>(_: &E) {}

        let no_fg = Win32Error::NoForegroundWindow;
        let os_err = Win32Error::Os(std::io::Error::from_raw_os_error(5));

        assert_is_std_error(&no_fg);
        assert_is_std_error(&os_err);
        assert_eq!(no_fg.to_string(), "no foreground window to manipulate");
        assert!(os_err.to_string().starts_with("Win32 window manipulation failed: "));
    }

    #[test]
    fn win32_error_from_hresult_unwraps_a_win32_facility_hresult_into_a_plain_os_error() {
        const E_ACCESSDENIED: i32 = 0x8007_0005u32 as i32;

        let err = Win32Error::from_hresult(E_ACCESSDENIED);

        // A FACILITY_WIN32 HRESULT (the `0x8007xxxx` family) wraps a plain
        // Win32 error code (`ERROR_ACCESS_DENIED` == 5) in its low 16 bits;
        // unwrapping it is what lets `is_access_denied` recognize it.
        assert!(err.is_access_denied());
        assert!(matches!(err, Win32Error::Os(ref e) if e.raw_os_error() == Some(5)));
    }

    #[test]
    fn win32_error_from_hresult_keeps_a_non_win32_facility_hresult_as_an_opaque_hresult() {
        const E_FAIL: i32 = 0x8000_4005u32 as i32;

        let err = Win32Error::from_hresult(E_FAIL);

        // Not a FACILITY_WIN32 code, so it can't be safely reinterpreted as
        // a plain Win32 error -- kept as-is, and (correctly) not mistaken
        // for access-denied.
        assert!(!err.is_access_denied());
        assert!(matches!(err, Win32Error::Hresult(E_FAIL)));
    }

    #[test]
    fn win32_error_hresult_variant_implements_display_and_std_error() {
        fn assert_is_std_error<E: std::error::Error>(_: &E) {}

        let err = Win32Error::Hresult(0x8000_4005u32 as i32);

        assert_is_std_error(&err);
        assert!(err.to_string().contains("0x80004005"));
    }

    #[test]
    fn win32_error_window_minimized_is_not_access_denied() {
        assert!(!Win32Error::WindowMinimized.is_access_denied());
    }

    #[test]
    fn win32_error_window_minimized_implements_display_and_std_error() {
        fn assert_is_std_error<E: std::error::Error>(_: &E) {}

        let err = Win32Error::WindowMinimized;

        assert_is_std_error(&err);
        assert_eq!(err.to_string(), "window is minimized");
    }

    #[test]
    fn win32_error_taskbar_not_found_is_not_access_denied() {
        assert!(!Win32Error::TaskbarNotFound.is_access_denied());
    }

    #[test]
    fn win32_error_taskbar_not_found_implements_display_and_std_error() {
        fn assert_is_std_error<E: std::error::Error>(_: &E) {}

        let err = Win32Error::TaskbarNotFound;

        assert_is_std_error(&err);
        assert_eq!(err.to_string(), "could not find the taskbar window (Shell_TrayWnd)");
    }

}
