//! Window manipulation for the borderless-toggle mechanic: stripping/restoring
//! a window's border/title bar, resizing/repositioning it, and disabling
//! Windows 11's rounded-corner rendering while it's borderless. Also holds
//! the independent taskbar-hide mechanic (issue #42): every call site that
//! hides the taskbar goes through a mode-aware entry point
//! ([`hide_taskbar`]/[`set_taskbar_hide_absolute`]/[`toggle_taskbar_hide`])
//! that picks between two techniques per
//! [`crate::profile_store::TaskbarHideMode`] -- `FullyHidden`
//! (`ShowWindow(Shell_TrayWnd, SW_HIDE)`, no hover-reveal) or `HoverReveal`
//! (the pre-#42 native `SHAppBarMessage`/`ABS_AUTOHIDE` mechanism,
//! unchanged). Also owns the "Apply to Window" picker's `EnumWindows` call
//! ([`list_visible_windows`]/`enum_windows_callback`), which calls into
//! [`crate::window_filter`] for the actual two-layer game-window
//! filtering/ranking decision logic (see
//! `docs/research/game-window-filtering.md` for the full rationale).
//!
//! Split the same way as [`crate::settings_ui`]:
//! - Pure decision logic ([`strip_border_style`], [`plan_toggle`],
//!   [`Win32Error::is_access_denied`], [`Win32Error::from_hresult`],
//!   [`plan_taskbar_hide`], `apply_autohide_state`) has no
//!   `HWND`/OS dependency and is fully unit tested below. [`crate::window_filter`]
//!   follows the identical pure/OS-facing split for the window-filtering/ranking
//!   logic.
//! - The actual Win32 calls further down (`GetForegroundWindow`, `IsIconic`,
//!   `SetWindowLongPtrW`, `SetWindowPos`, `DwmSetWindowAttribute`,
//!   `ShellExecuteW`, `FindWindowW`, `SHAppBarMessage`, `ShowWindow`,
//!   `EnumWindows`) are OS-facing -- they need a real foreground window, a
//!   real shell taskbar, or a real running process to query, which a
//!   headless/CI sandbox cannot provide -- so they are NOT exercised by this
//!   crate's automated tests. See the module's doc comment in the issue
//!   writeup for what could/couldn't be verified.
//!
//! [`toggle_taskbar_hide`] keeps no remembered state at all -- it reads the
//! taskbar's real OS-level state fresh on every press (see its doc comment
//! for why) -- completely separate from the borderless mechanic's
//! [`RestoreState`]; the two toggles share no state.

use crate::profile_store::TaskbarHideMode;
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

/// Converts a plain `bool` (the global
/// [`crate::profile_store::Config::hide_taskbar_on_apply`] setting) into a
/// [`TaskbarAutohideState`] -- the interpretation [`set_taskbar_hide_absolute`]
/// applies. Pure logic -- no OS call.
fn taskbar_autohide_state_from_bool(auto_hide: bool) -> TaskbarAutohideState {
    if auto_hide {
        TaskbarAutohideState::AutoHidden
    } else {
        TaskbarAutohideState::AlwaysVisible
    }
}

/// Which technique produced a hidden taskbar -- the richer "how to restore"
/// state issue #42 requires: recording *which technique* hid the taskbar
/// (not just an appbar flag value) so a later restore always undoes exactly
/// that, even if [`crate::profile_store::TaskbarHideMode`] changes in
/// between (e.g. the zone editor forces the taskbar hidden in `FullyHidden`
/// mode, the user flips the setting to `HoverReveal` mid-session, then
/// closes the editor -- restoring must still call [`show_taskbar`], not
/// write an appbar state that would do nothing to a `ShowWindow`-hidden
/// window).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskbarHideRestore {
    /// Hidden via `ShowWindow(Shell_TrayWnd, SW_HIDE)` -- restoring means
    /// [`show_taskbar`].
    FullyHidden,
    /// Hidden via the native appbar auto-hide mechanism -- restoring means
    /// writing back whatever [`TaskbarAutohideState`] was current
    /// immediately before hiding.
    HoverReveal(TaskbarAutohideState),
}

/// Decides which technique to use to hide the taskbar under `mode`, and what
/// to remember so a later restore undoes exactly that (see
/// [`TaskbarHideRestore`]'s doc comment).
///
/// - `FullyHidden`: always remembers [`TaskbarHideRestore::FullyHidden`] --
///   `current` is irrelevant, since `ShowWindow`/ `SW_HIDE` doesn't touch or
///   depend on the appbar's auto-hide state at all.
/// - `HoverReveal`: remembers `current` verbatim (mirroring the zone
///   editor's pre-#42 `plan_taskbar_force_on` shape), so a restore can put
///   the appbar back exactly as it was found, not just to a hardcoded
///   `AlwaysVisible`.
///
/// Pure decision logic -- no OS call.
pub fn plan_taskbar_hide(mode: TaskbarHideMode, current: TaskbarAutohideState) -> TaskbarHideRestore {
    match mode {
        TaskbarHideMode::FullyHidden => TaskbarHideRestore::FullyHidden,
        TaskbarHideMode::HoverReveal => TaskbarHideRestore::HoverReveal(current),
    }
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
use windows_sys::Win32::Foundation::{CloseHandle, HWND, INVALID_HANDLE_VALUE, POINT, RECT};
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
    EnumWindows, FindWindowW, GetAncestor, GetForegroundWindow, WindowFromPoint, GA_ROOT, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    GWL_STYLE, SET_WINDOW_POS_FLAGS, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_SHOW, SW_SHOWNORMAL,
    WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
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

/// Pure decision: has a window that was previously made borderless at
/// `target` drifted from it? True if its bounds differ from `target`, or if
/// any border/title-bar style bit (see [`strip_border_style`]) came back.
/// Used by the in-game-resize watcher so it only re-applies when the game
/// actually changed the window -- which also keeps GameFrame's own
/// `SetWindowPos` (which fires another location-change event) from looping.
pub fn window_drifted(current_region: Region, current_style: u32, target: Region) -> bool {
    current_region != target || strip_border_style(current_style) != current_style
}

static APPLIED_WINDOWS: std::sync::Mutex<Vec<isize>> = std::sync::Mutex::new(Vec::new());

/// Remembers `hwnd` as one GameFrame applied the Zone to, so the
/// location-change watcher can cheaply (one mutex lock + scan) ignore every
/// other window's move events.
pub fn remember_applied_window(hwnd: HWND) {
    let mut windows = APPLIED_WINDOWS.lock().unwrap_or_else(|p| p.into_inner());
    if !windows.contains(&(hwnd as isize)) {
        windows.push(hwnd as isize);
    }
}

/// Whether `hwnd` was previously registered via [`remember_applied_window`].
pub fn is_applied_window(hwnd: HWND) -> bool {
    APPLIED_WINDOWS.lock().unwrap_or_else(|p| p.into_inner()).contains(&(hwnd as isize))
}

/// Forgets `hwnd` (called when it is destroyed).
pub fn forget_applied_window(hwnd: HWND) {
    APPLIED_WINDOWS.lock().unwrap_or_else(|p| p.into_inner()).retain(|h| *h != hwnd as isize);
}

/// Reads `hwnd`'s live bounds/style and reports [`window_drifted`] against
/// `target`. OS-facing.
pub fn window_has_drifted(hwnd: HWND, target: Region) -> Result<bool, Win32Error> {
    Ok(window_drifted(get_window_region(hwnd)?, get_window_style(hwnd)?, target))
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

/// Hides the real taskbar completely via `ShowWindow(Shell_TrayWnd,
/// SW_HIDE)` -- the `FullyHidden` mode's technique (issue #42): unlike the
/// native appbar auto-hide mechanism, this does not reappear on mouse hover
/// near the screen edge.
///
/// OS-facing: locates the real taskbar window and sends it a real
/// `ShowWindow` call. Not exercised by this crate's automated tests -- see
/// module docs.
pub fn hide_taskbar_completely() -> Result<(), Win32Error> {
    let hwnd = find_taskbar_window()?;
    // Safety: `hwnd` is a live handle just resolved above; `ShowWindow`
    // performs no writes through pointers, only a plain `i32` command.
    unsafe {
        ShowWindow(hwnd, SW_HIDE);
    }
    Ok(())
}

/// Shows the real taskbar via `ShowWindow(Shell_TrayWnd, SW_SHOW)` -- undoes
/// [`hide_taskbar_completely`]. Does not touch the appbar auto-hide state at
/// all (per this module's `FullyHidden`/`HoverReveal` split -- see
/// [`TaskbarHideRestore`]'s doc comment).
///
/// OS-facing: locates the real taskbar window and sends it a real
/// `ShowWindow` call. Not exercised by this crate's automated tests -- see
/// module docs.
pub fn show_taskbar() -> Result<(), Win32Error> {
    let hwnd = find_taskbar_window()?;
    // Safety: `hwnd` is a live handle just resolved above; `ShowWindow`
    // performs no writes through pointers, only a plain `i32` command.
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
    }
    Ok(())
}

/// Hides the taskbar using whichever technique `mode` selects, remembering
/// which one was used so it can be undone later -- called by the
/// taskbar-autohide hotkey's "on" direction (via [`toggle_taskbar_hide`]),
/// the zone-editor overlay's force-on session, and, as of issue #43, every
/// apply-path call site that hides the taskbar for a specific window (via
/// [`set_taskbar_hide_absolute_armed`]) -- all callers that need a "hide
/// now, restore exactly this later" round trip rather than a bare absolute
/// on/off set (the sibling [`set_taskbar_hide_absolute`] remains that bare
/// primitive, for callers -- and `set_taskbar_hide_absolute_armed`'s own
/// `hide == false` path -- with nothing to arm). Returns a
/// [`TaskbarHideRestore`] recording which technique was actually used, so a
/// later [`restore_taskbar`] call undoes exactly this even if the mode
/// setting changes in between.
///
/// OS-facing: for `HoverReveal`, resolves the taskbar window and reads its
/// flags once, reusing both for the write below (mirroring the pre-#42
/// `hide_taskbar`'s own rationale) rather than a separate
/// query-then-set round trip. Not exercised by this crate's automated tests
/// -- see module docs. The decision of what to remember is pure and tested
/// separately via [`plan_taskbar_hide`].
pub fn hide_taskbar(mode: TaskbarHideMode) -> Result<TaskbarHideRestore, Win32Error> {
    match mode {
        TaskbarHideMode::FullyHidden => {
            hide_taskbar_completely()?;
            Ok(TaskbarHideRestore::FullyHidden)
        }
        TaskbarHideMode::HoverReveal => {
            let hwnd = find_taskbar_window()?;
            let current_flags = read_appbar_flags(hwnd);
            let current = state_from_flags(current_flags);
            let restore = plan_taskbar_hide(mode, current);
            write_appbar_state(hwnd, current_flags, TaskbarAutohideState::AutoHidden);
            Ok(restore)
        }
    }
}

/// Restores the taskbar to exactly the state remembered by a prior
/// [`hide_taskbar`] call -- dispatches on which technique [`TaskbarHideRestore`]
/// records rather than assuming whatever [`TaskbarHideMode`] is currently
/// configured, which is exactly the point: the mode may have changed between
/// the hide and this restore.
pub fn restore_taskbar(remembered: TaskbarHideRestore) -> Result<(), Win32Error> {
    match remembered {
        TaskbarHideRestore::FullyHidden => show_taskbar(),
        TaskbarHideRestore::HoverReveal(state) => set_appbar_state(state),
    }
}


/// Reference-counted arming state for issue #43's auto-restore-on-close
/// feature (see [`ArmedTaskbarWindows`] for the transition logic). Tracks
/// every window whose apply-path has hidden the taskbar and is still
/// relying on it staying hidden, plus the single [`TaskbarHideRestore`]
/// needed to undo the *first* hide of the current "hidden session".
/// Consulted by [`take_armed_taskbar_restore_for_window`] when
/// [`crate::launch_watcher`]'s `EVENT_OBJECT_DESTROY` watcher observes an
/// armed window closing, and by [`take_any_armed_taskbar_restore`] when
/// [`crate::app::run`] quits via the tray "Exit" item while a hide is still
/// in effect.
///
/// This replaces an earlier single-slot `(isize, TaskbarHideRestore)`
/// design that overwrote-on-arm: code review found it broken for
/// GameFrame's central multi-tracked-game use case (e.g. `Left`+`Right`
/// running side-by-side, both hiding the taskbar). Two concrete bugs
/// followed from tracking only "the single most recent window": (1) value
/// loss -- a second window arming while the first was still armed would
/// re-capture the *already-hidden* appbar state as its own "restore to"
/// value in `HoverReveal` mode, silently discarding the true original
/// state forever; (2) premature restore -- the second window closing while
/// the first was still running would restore the taskbar out from under
/// the first game, even though it still wanted it hidden. Reference
/// counting the armed window set (rather than remembering only the last
/// one) fixes both: the remembered restore value is captured once, on the
/// empty-to-non-empty transition, and only released once the set drains
/// back to empty.
///
/// Wrapped in `Option` (rather than a directly-constructed
/// [`ArmedTaskbarWindows`]) purely so the `static` initializer can be
/// `Mutex::new(None)`, a `const` expression -- [`with_armed_taskbar_windows`]
/// lazily fills it in on first use via `get_or_insert_with`.
///
/// Stores each window as `isize` rather than `HWND`: `HWND` is `*mut
/// core::ffi::c_void`, which is neither `Send` nor `Sync`, so a `HashSet`
/// of them could not live in a `static` (a `static` requires `Sync`, and
/// `Mutex<T>` is only `Sync` when `T: Send`). Each stored `isize` is a
/// plain pointer-identity value -- compared for equality/hashed only, never
/// dereferenced -- so the round trip through an integer is lossless and
/// safe.
///
/// Fully independent of [`toggle_taskbar_hide`] (which keeps no remembered
/// state of its own at all -- see its doc comment): per
/// `docs/adr/0002-taskbar-autohide-coupled-to-preset-apply.md`, the manual
/// global taskbar-autohide hotkey's own toggle must stay independent from
/// the Zone-apply paths' taskbar handling, and this static is never read
/// or written by [`toggle_taskbar_hide`] or the zone editor's own
/// session-scoped hide (`crate::zone_overlay`, which keeps its remembered
/// [`TaskbarHideRestore`] locally in its own guard struct, not here).
static ARMED_TASKBAR_RESTORE: std::sync::Mutex<Option<ArmedTaskbarWindows>> = std::sync::Mutex::new(None);

/// Pure reference-counted arming state backing [`ARMED_TASKBAR_RESTORE`],
/// factored out so its transitions are unit testable without touching the
/// shared global mutex -- mirrors how `armed_window_matches` was split out
/// for the original single-slot design this replaces (see
/// [`ARMED_TASKBAR_RESTORE`]'s doc comment for why it was replaced).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ArmedTaskbarWindows {
    windows: std::collections::HashSet<isize>,
    restore: Option<TaskbarHideRestore>,
}

impl ArmedTaskbarWindows {
    /// True if a hidden session is currently active, i.e. at least one
    /// window is currently armed.
    fn is_active(&self) -> bool {
        !self.windows.is_empty()
    }

    /// True only if `hwnd_id` is currently armed *and* is the sole armed
    /// window -- i.e. removing it would end the hidden session and require
    /// an actual OS-level taskbar restore, as opposed to just leaving other
    /// windows' hides in effect.
    fn is_last(&self, hwnd_id: isize) -> bool {
        self.windows.len() == 1 && self.windows.contains(&hwnd_id)
    }

    /// Starts a brand new hidden session: arms `hwnd_id` and remembers
    /// `restore` as what to restore to once the session ends. Callers must
    /// only call this when [`Self::is_active`] was `false` beforehand (a
    /// fresh session) -- see [`Self::join`] for the already-active case.
    fn start(&mut self, hwnd_id: isize, restore: TaskbarHideRestore) {
        self.windows.insert(hwnd_id);
        self.restore = Some(restore);
    }

    /// Joins an already-active hidden session: arms `hwnd_id` without
    /// touching the remembered restore value -- it still reflects the true
    /// pre-first-hide state, and must never be overwritten by a later
    /// window's hide (the value-loss bug described in
    /// [`ARMED_TASKBAR_RESTORE`]'s doc comment).
    fn join(&mut self, hwnd_id: isize) {
        self.windows.insert(hwnd_id);
    }

    /// Removes `hwnd_id` from the session. Returns `Some(restore)` --
    /// clearing the remembered value -- only if `hwnd_id` was armed and
    /// removing it emptied the set (the last window relying on the hide
    /// just left); returns `None` otherwise, whether because `hwnd_id`
    /// wasn't armed at all (an unrelated window) or because other windows
    /// are still relying on the hide (the premature-restore bug described
    /// in [`ARMED_TASKBAR_RESTORE`]'s doc comment).
    fn leave(&mut self, hwnd_id: isize) -> Option<TaskbarHideRestore> {
        self.windows.remove(&hwnd_id);
        if self.windows.is_empty() { self.restore.take() } else { None }
    }

    /// Unconditionally ends the session (if any), returning the remembered
    /// restore value regardless of how many windows were still armed --
    /// backs the tray "Exit" path's semantics: the process is quitting
    /// regardless of how many tracked games are still running, so "wait for
    /// the rest" doesn't apply.
    fn take_all(&mut self) -> Option<TaskbarHideRestore> {
        self.windows.clear();
        self.restore.take()
    }
}

/// Runs `f` against the shared [`ArmedTaskbarWindows`] state, initializing
/// it on first use, and recovering (rather than silently no-op'ing, per
/// code review) if the mutex was poisoned by a panic while held --
/// mirroring `crate::window_filter::known_steam_install_dirs`'s identical
/// `unwrap_or_else(|poisoned| poisoned.into_inner())` pattern for a
/// comparable `Mutex<Option<...>>` static. The guarded data (a `HashSet`
/// plus an `Option`) has no invariant a panic mid-hold could actually
/// violate, so recovering it is safe and strictly better than disabling
/// this whole feature for the rest of the process's life the moment any
/// panic anywhere happens to occur while the lock is held.
fn with_armed_taskbar_windows<R>(f: impl FnOnce(&mut ArmedTaskbarWindows) -> R) -> R {
    let mut guard = ARMED_TASKBAR_RESTORE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f(guard.get_or_insert_with(ArmedTaskbarWindows::default))
}

/// True only if `hwnd` is currently armed *and* is the sole armed window --
/// see [`ArmedTaskbarWindows::is_last`].
fn is_last_armed_window(hwnd: HWND) -> bool {
    with_armed_taskbar_windows(|state| state.is_last(hwnd as isize))
}

/// True if a hidden session is currently active -- see
/// [`ArmedTaskbarWindows::is_active`].
fn is_taskbar_hide_session_active() -> bool {
    with_armed_taskbar_windows(|state| state.is_active())
}

/// Starts a brand new hidden session for `hwnd`, remembering `restore` as
/// what to restore to once the session ends. Called only by
/// [`set_taskbar_hide_absolute_armed`]'s `hide == true` path when
/// [`is_taskbar_hide_session_active`] was `false` beforehand.
fn start_taskbar_hide_session(hwnd: HWND, restore: TaskbarHideRestore) {
    with_armed_taskbar_windows(|state| state.start(hwnd as isize, restore));
}

/// Joins `hwnd` to an already-active hidden session, without touching the
/// remembered restore value. Called only by
/// [`set_taskbar_hide_absolute_armed`]'s `hide == true` path when
/// [`is_taskbar_hide_session_active`] was already `true`.
fn join_taskbar_hide_session(hwnd: HWND) {
    with_armed_taskbar_windows(|state| state.join(hwnd as isize));
}

/// Takes (removes and returns) the armed restore if -- and only if --
/// removing `hwnd` from the currently-armed set empties it, via
/// [`ArmedTaskbarWindows::leave`]. Called by [`crate::launch_watcher`]'s
/// `EVENT_OBJECT_DESTROY` callback when `hwnd` closes: an empty result
/// means that was the last window whose Zone-apply had hidden the
/// taskbar, so the caller should restore it immediately with no hotkey
/// press required (issue #43's core behavior). Returns `None` for every
/// other window closing (unarmed, or other windows still relying on the
/// hide), leaving the armed state's restore value untouched -- so an
/// unrelated window's destroy event has no effect, and another tracked
/// game's still-active hide is never prematurely undone.
///
/// Also used, with `hide == false`, by [`set_taskbar_hide_absolute_armed`]
/// to disarm `hwnd` when its own apply-path explicitly shows the taskbar
/// again: without this, a stale armed restore for a window whose
/// `hide_taskbar_on_apply` setting later flips to `false` (e.g. edited live in
/// Settings) would still fire an incorrect restore when that window
/// eventually closes.
///
/// A null `hwnd` never matches (defensive): both call paths above --
/// `crate::launch_watcher`'s destroy callback, and
/// `set_taskbar_hide_absolute_armed`'s `hide == false` branch -- reach this
/// function, and while neither of today's real callers can realistically
/// pass a null `HWND`, this guard makes that safe regardless of caller.
pub(crate) fn take_armed_taskbar_restore_for_window(hwnd: HWND) -> Option<TaskbarHideRestore> {
    if hwnd.is_null() {
        return None;
    }
    with_armed_taskbar_windows(|state| state.leave(hwnd as isize))
}

/// Takes (removes and returns) whatever restore is currently armed,
/// unconditionally ending the hidden session regardless of how many windows
/// were still relying on it -- see [`ArmedTaskbarWindows::take_all`].
/// Called by [`crate::app::run`] on the tray "Exit" path: the process is
/// about to quit, so "wait for the rest" doesn't apply -- any still-armed
/// hide must be undone before the process exits, full stop, or the desktop
/// is left with a hidden taskbar and no running GameFrame instance left to
/// restore it.
pub(crate) fn take_any_armed_taskbar_restore() -> Option<TaskbarHideRestore> {
    with_armed_taskbar_windows(|state| state.take_all())
}

/// Sets the taskbar's hidden/visible state to exactly `hide` (an absolute
/// set, not a toggle), using whichever technique `mode` selects. No restore
/// memory is kept here (unlike [`hide_taskbar`]/[`restore_taskbar`]): this
/// is the low-level primitive for a caller that already knows the absolute
/// value it wants and has no need to later "undo the last hide" for a
/// specific window.
///
/// As of issue #43, every apply-path call site that hides the taskbar for a
/// specific window (the tray/launch-detection/borderless-toggle apply paths
/// and Settings' live-reapply path) calls [`set_taskbar_hide_absolute_armed`]
/// instead of this function directly, so that window's later close can
/// automatically restore the taskbar. This function remains the plain
/// absolute-set primitive that wrapper builds on, and is also still called
/// directly wherever there genuinely is no window to arm (e.g. showing the
/// taskbar back on the `hide == false` path).
///
/// Deliberately independent of [`toggle_taskbar_hide`]/its state above: this
/// never reads or writes a caller-tracked [`TaskbarHideRestore`] variable, so
/// it can't drift out of sync with (or be confused for) the existing global
/// taskbar-autohide hotkey's own toggle mechanic, which ADR 0002 explicitly
/// keeps untouched and independent.
///
/// OS-facing: locates the real taskbar window and sends it a real
/// `ShowWindow`/`SHAppBarMessage` call depending on `mode`. Not exercised by
/// this crate's automated tests -- see module docs; the `bool` ->
/// [`TaskbarAutohideState`] interpretation itself is pure and unit tested via
/// [`taskbar_autohide_state_from_bool`].
pub fn set_taskbar_hide_absolute(hide: bool, mode: TaskbarHideMode) -> Result<(), Win32Error> {
    match mode {
        TaskbarHideMode::FullyHidden => {
            if hide {
                hide_taskbar_completely()
            } else {
                show_taskbar()
            }
        }
        TaskbarHideMode::HoverReveal => set_appbar_state(taskbar_autohide_state_from_bool(hide)),
    }
}

/// Sets the taskbar's hidden/visible state to exactly `hide` for `hwnd`'s
/// apply, exactly like [`set_taskbar_hide_absolute`], but additionally arms
/// (`hide == true`) or disarms (`hide == false`) issue #43's
/// destroy-watcher restore for `hwnd` via the reference-counted
/// [`ArmedTaskbarWindows`] state -- see [`ARMED_TASKBAR_RESTORE`]'s doc
/// comment. This is the entry point every apply-path call site that hides
/// the taskbar for a specific window should use, instead of calling
/// [`set_taskbar_hide_absolute`] directly, so that window's later close can
/// automatically restore the taskbar with no hotkey press required.
///
/// The `hide == true` path only calls the real OS hide
/// ([`hide_taskbar`], which *does* return a [`TaskbarHideRestore`], unlike
/// [`set_taskbar_hide_absolute`]'s plain absolute set) when no hidden
/// session is currently active; if one already is (another tracked game's
/// hide is already in effect), `hwnd` just joins that session via
/// [`join_taskbar_hide_session`] -- no redundant OS call, and critically,
/// no re-capture of the remembered restore value (which would silently
/// discard the true pre-first-hide state, the value-loss bug described in
/// [`ARMED_TASKBAR_RESTORE`]'s doc comment).
///
/// The `hide == false` path only issues the real OS show
/// ([`set_taskbar_hide_absolute`]) when `hwnd` is the *last* armed window
/// ([`is_last_armed_window`]): showing the taskbar while another tracked
/// game's hide is still in effect would force it visible out from under
/// that other game, which this deliberately avoids -- the simplest
/// consistent behavior, mirroring the destroy-watcher's own "only restore
/// once the set drains to empty" logic. When the OS show call *is* made but
/// fails, `hwnd` is deliberately left armed rather than disarmed anyway:
/// discarding the one piece of state that could still fix the taskbar later
/// (via a subsequent close or tray Exit) on top of an OS call that's
/// already known to have failed would only compound the failure.
///
/// OS-facing (delegates to [`hide_taskbar`]/[`set_taskbar_hide_absolute`]).
/// Not exercised by this crate's automated tests -- see module docs.
pub fn set_taskbar_hide_absolute_armed(hwnd: HWND, hide: bool, mode: TaskbarHideMode) -> Result<(), Win32Error> {
    if hide {
        if is_taskbar_hide_session_active() {
            // Issue #52 finding 3: no OS call here, so a taskbar the hotkey
            // showed in the meantime stays visible.
            log!("taskbar hide for {hwnd:?}: joining the active session (no OS call); {}", taskbar_debug_state());
            join_taskbar_hide_session(hwnd);
            Ok(())
        } else {
            let restore = hide_taskbar(mode)?;
            start_taskbar_hide_session(hwnd, restore);
            log!("taskbar hide for {hwnd:?}: started a session ({mode:?}); {}", taskbar_debug_state());
            Ok(())
        }
    } else if is_last_armed_window(hwnd) {
        let result = set_taskbar_hide_absolute(false, mode);
        if result.is_ok() {
            take_armed_taskbar_restore_for_window(hwnd);
        }
        result
    } else {
        // Either `hwnd` was never armed, or other windows are still relying
        // on the hide -- disarm `hwnd` (a no-op if it wasn't armed) without
        // touching the real taskbar.
        take_armed_taskbar_restore_for_window(hwnd);
        Ok(())
    }
}

/// Reads the taskbar's *actual* current hidden/visible state directly from
/// the OS, rather than trusting any caller-tracked flag: for `FullyHidden`,
/// [`IsWindowVisible`] on the real `Shell_TrayWnd`; for `HoverReveal`, the
/// real appbar auto-hide flag (via [`read_appbar_flags`]/[`state_from_flags`]).
///
/// This exists because [`toggle_taskbar_hide`] previously decided hide-vs-show
/// from a remembered `Option<TaskbarHideRestore>` that only that hotkey ever
/// wrote -- but the taskbar can also be hidden by an entirely different path
/// (a tracked game's launch-detection auto-hide, [`set_taskbar_hide_absolute_armed`],
/// which re-applies on every foreground switch back to the game, not just
/// the initial launch). That path never touched the hotkey's own memory, so
/// the two could disagree about whether the taskbar was hidden, making the
/// hotkey's next press act on a stale premise (hiding an already-hidden
/// taskbar is a no-op; the *following* press then does the opposite of what
/// was expected). Grounding the decision in the real OS state on every press
/// removes the possibility of that drift entirely.
///
/// OS-facing: locates the real taskbar window and reads its actual state.
/// Not exercised by this crate's automated tests -- see module docs.
fn is_taskbar_hidden(mode: TaskbarHideMode) -> Result<bool, Win32Error> {
    let hwnd = find_taskbar_window()?;
    match mode {
        TaskbarHideMode::FullyHidden => Ok(unsafe { IsWindowVisible(hwnd) } == 0),
        TaskbarHideMode::HoverReveal => {
            Ok(state_from_flags(read_appbar_flags(hwnd)) == TaskbarAutohideState::AutoHidden)
        }
    }
}

/// Toggles the taskbar's hidden/visible state on/off -- the manual global
/// taskbar-autohide hotkey's own independent on/off toggle *concept* (issue
/// #42's out-of-scope note), but grounded in the taskbar's real current
/// state ([`is_taskbar_hidden`]) rather than a remembered flag (see that
/// function's doc comment for why): currently visible hides via
/// [`set_taskbar_hide_absolute`]; currently hidden shows via the same.
///
/// Fully independent of [`toggle_borderless_on_foreground_window`]'s
/// `RestoreState`, and of [`set_taskbar_hide_absolute_armed`]'s
/// `ARMED_TASKBAR_RESTORE` bookkeeping -- this reads only the taskbar's real
/// OS-level visibility, never any tracked-game config or armed-window state,
/// so it remains independent of tracked-game state per
/// `docs/adr/0002-taskbar-autohide-coupled-to-preset-apply.md`, just no
/// longer independent of *reality*.
///
/// OS-facing: locates the real taskbar window and sends it a real
/// `ShowWindow`/`SHAppBarMessage` call. Not exercised by this crate's
/// automated tests -- see module docs.
pub fn toggle_taskbar_hide(mode: TaskbarHideMode) -> Result<(), Win32Error> {
    // Issue #52: the hotkey sometimes does nothing after long uptime. Log
    // what it saw and what it left behind so a report comes with evidence.
    log!("taskbar hotkey ({mode:?}): before: {}", taskbar_debug_state());
    let hidden = is_taskbar_hidden(mode)?;
    let result = set_taskbar_hide_absolute(!hidden, mode);
    log!(
        "taskbar hotkey: judged hidden={hidden}, so {} -> {result:?}; after: {}",
        if hidden { "showing" } else { "hiding" },
        taskbar_debug_state()
    );
    result
}

/// One line describing the taskbar's real state, for the log (issue #52):
/// `WS_VISIBLE`, the appbar flags, its rect, whether it is actually the
/// topmost window at its own center (it can be `WS_VISIBLE` yet covered),
/// the foreground window, and the armed hide session.
pub fn taskbar_debug_state() -> String {
    let Ok(taskbar) = find_taskbar_window() else {
        return "Shell_TrayWnd not found".to_string();
    };
    let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // Safety: plain queries on live handles with valid out-pointers.
    let (visible, on_top, foreground) = unsafe {
        GetWindowRect(taskbar, &mut rect);
        let center = POINT { x: (rect.left + rect.right) / 2, y: (rect.top + rect.bottom) / 2 };
        let at_center = WindowFromPoint(center);
        let on_top = !at_center.is_null() && GetAncestor(at_center, GA_ROOT) == taskbar;
        (IsWindowVisible(taskbar) != 0, on_top, GetForegroundWindow())
    };
    let flags = read_appbar_flags(taskbar);
    let foreground_exe = if foreground.is_null() {
        "none".to_string()
    } else {
        window_filter::exe_path_for_window(foreground)
            .map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
            .unwrap_or_else(|| "?".to_string())
    };
    let (armed, has_restore) = with_armed_taskbar_windows(|s| (s.windows.len(), s.restore.is_some()));
    format!(
        "visible={visible} appbar_flags={flags:#x} rect=({},{})-({},{}) on_top_at_center={on_top} \
         foreground={foreground:?} ({foreground_exe}) armed_windows={armed} armed_restore={has_restore}",
        rect.left, rect.top, rect.right, rect.bottom
    )
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
/// already have both in hand (e.g. [`hide_taskbar`]'s `HoverReveal` arm, which reads
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
/// [`hide_taskbar`]'s `HoverReveal` arm (which needs only the auto-hide bit, via
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
/// as a [`TaskbarAutohideState`]. Used by [`hide_taskbar`]'s `HoverReveal` arm,
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
pub(crate) fn encode_wide(value: &str) -> Vec<u16> {
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

/// Re-applies a tracked game's border/taskbar settings to its already-running
/// window, for issue #33's live-apply-on-commit feature: this is the fourth
/// call site of the "apply borderless, then gate taskbar-set on border-apply
/// success" pattern already used by the borderless-toggle hotkey path, the
/// tray "Apply to Window" path, and the launch-watcher path (see
/// `crate::app::handle_toggle_borderless`/`handle_apply_to_window`/
/// `handle_auto_apply_launch`) -- getting it right here means simply
/// mirroring the same early-return-on-border-failure shape those three
/// already use, not re-deriving it.
///
/// Deliberately takes an already-resolved `hwnd` rather than an `exe_path`:
/// resolving the running window (via [`resolve_running_window`]) is the
/// caller's (`crate::settings_ui`) responsibility, and must happen *before*
/// the caller resolves `target_region` -- a not-running game must never reach
/// this function at all, and must never cause a region-resolution error to be
/// reported as a live-apply failure (issue #33's iteration-3 review fix).
///
/// `mode` (issue #42) selects which taskbar-hide technique the taskbar-set
/// call below uses (`FullyHidden` via `ShowWindow`, or `HoverReveal` via the
/// native appbar auto-hide mechanism) -- it's threaded straight through to
/// [`set_taskbar_hide_absolute_armed`], same as every other apply path
/// (issue #43 switched this from the plain [`set_taskbar_hide_absolute`] so
/// closing `hwnd` later can auto-restore the taskbar).
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
    hide_taskbar: bool,
    mode: TaskbarHideMode,
    target_region: Region,
) -> LiveApplyOutcome {
    if let Err(e) = apply_borderless_to_window(hwnd, target_region) {
        return LiveApplyOutcome::Failed(e);
    }

    if let Err(e) = set_taskbar_hide_absolute_armed(hwnd, hide_taskbar, mode) {
        log!("live re-apply: failed to set taskbar auto-hide for {display_name}: {e}");
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

    // Regression: Last Epoch re-created its caption and resized itself after an
    // in-game settings change (rect 1660,0,3460x1440 -> 632,0,3856x1479, style
    // 0x14000000 -> 0x14CF0000) and GameFrame never re-applied.
    #[test]
    fn window_drifted_detects_the_last_epoch_settings_change() {
        let target = Region { x: 1660, y: 0, width: 3460, height: 1440 };
        let drifted = Region { x: 632, y: 0, width: 3856, height: 1479 };

        assert!(window_drifted(drifted, 0x14CF0000, target));
    }

    #[test]
    fn window_drifted_detects_a_restored_caption_even_when_bounds_match() {
        let target = Region { x: 0, y: 0, width: 100, height: 100 };

        assert!(window_drifted(target, 0x14CF0000, target));
    }

    #[test]
    fn window_drifted_is_false_for_the_state_gameframe_itself_applied() {
        let target = Region { x: 1660, y: 0, width: 3460, height: 1440 };

        assert!(!window_drifted(target, 0x14000000, target));
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
    fn taskbar_autohide_state_default_is_always_visible() {
        // Fresh install / first press starts from "always visible" -- the
        // taskbar's normal, un-toggled state.
        assert_eq!(TaskbarAutohideState::default(), TaskbarAutohideState::AlwaysVisible);
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
    fn plan_taskbar_hide_in_fully_hidden_mode_always_remembers_fully_hidden_regardless_of_current_appbar_state() {
        assert_eq!(
            plan_taskbar_hide(TaskbarHideMode::FullyHidden, TaskbarAutohideState::AlwaysVisible),
            TaskbarHideRestore::FullyHidden
        );
        assert_eq!(
            plan_taskbar_hide(TaskbarHideMode::FullyHidden, TaskbarAutohideState::AutoHidden),
            TaskbarHideRestore::FullyHidden
        );
    }

    #[test]
    fn plan_taskbar_hide_in_hover_reveal_mode_remembers_whatever_appbar_state_was_current() {
        assert_eq!(
            plan_taskbar_hide(TaskbarHideMode::HoverReveal, TaskbarAutohideState::AlwaysVisible),
            TaskbarHideRestore::HoverReveal(TaskbarAutohideState::AlwaysVisible)
        );
        assert_eq!(
            plan_taskbar_hide(TaskbarHideMode::HoverReveal, TaskbarAutohideState::AutoHidden),
            TaskbarHideRestore::HoverReveal(TaskbarAutohideState::AutoHidden)
        );
    }

    #[test]
    fn taskbar_autohide_state_from_bool_true_maps_to_auto_hidden() {
        assert_eq!(taskbar_autohide_state_from_bool(true), TaskbarAutohideState::AutoHidden);
    }

    #[test]
    fn taskbar_autohide_state_from_bool_false_maps_to_always_visible() {
        assert_eq!(taskbar_autohide_state_from_bool(false), TaskbarAutohideState::AlwaysVisible);
    }


    /// Issue #43's reference-counted armed-window state (pure -- no OS
    /// call, no shared static touched, unlike `take_armed_taskbar_restore_for_window`
    /// / `set_taskbar_hide_absolute_armed`, which wrap this same logic
    /// around the real `ARMED_TASKBAR_RESTORE` static -- see
    /// `ARMED_TASKBAR_RESTORE`'s doc comment for why this replaced the
    /// original single-slot design). These are the "multi-window scenario"
    /// tests code review called for: two windows hide the taskbar, one
    /// closes (still hidden, no restore yet), then the other closes (now
    /// restores with the *original* pre-first-hide value, not whatever the
    /// second window's own arm would have captured).
    #[test]
    fn armed_taskbar_windows_a_single_window_arming_then_leaving_restores_immediately() {
        let mut state = ArmedTaskbarWindows::default();
        let window = 1isize;

        state.start(window, TaskbarHideRestore::FullyHidden);
        let restore = state.leave(window);

        assert_eq!(restore, Some(TaskbarHideRestore::FullyHidden));
        assert!(!state.is_active());
    }

    #[test]
    fn armed_taskbar_windows_two_windows_hiding_then_one_closing_keeps_the_taskbar_hidden() {
        let mut state = ArmedTaskbarWindows::default();
        let window_a = 1isize;
        let window_b = 2isize;
        state.start(window_a, TaskbarHideRestore::FullyHidden);
        state.join(window_b);

        let restore_after_a_closes = state.leave(window_a);

        assert_eq!(restore_after_a_closes, None, "window B still wants the taskbar hidden");
        assert!(state.is_active());
    }

    #[test]
    fn armed_taskbar_windows_restores_the_original_pre_first_hide_value_once_the_last_window_closes() {
        let mut state = ArmedTaskbarWindows::default();
        let window_a = 1isize;
        let window_b = 2isize;
        let original = TaskbarHideRestore::HoverReveal(TaskbarAutohideState::AlwaysVisible);
        state.start(window_a, original);
        state.join(window_b);
        state.leave(window_a);

        let restore_after_b_closes = state.leave(window_b);

        assert_eq!(restore_after_b_closes, Some(original));
        assert!(!state.is_active());
    }

    #[test]
    fn armed_taskbar_windows_join_never_overwrites_the_remembered_restore_value() {
        // Regression test for the value-loss bug: joining an already-active
        // session must never re-capture/overwrite the true pre-first-hide
        // restore value.
        let mut state = ArmedTaskbarWindows::default();
        state.start(1, TaskbarHideRestore::FullyHidden);

        state.join(2);

        assert_eq!(state.restore, Some(TaskbarHideRestore::FullyHidden));
    }

    #[test]
    fn armed_taskbar_windows_leave_of_an_unarmed_window_returns_none_and_leaves_the_session_untouched() {
        let mut state = ArmedTaskbarWindows::default();
        state.start(1, TaskbarHideRestore::FullyHidden);

        let result = state.leave(99);

        assert_eq!(result, None);
        assert!(state.is_active());
        assert!(state.windows.contains(&1));
    }

    #[test]
    fn armed_taskbar_windows_is_last_true_only_when_hwnd_is_the_sole_armed_window() {
        let mut state = ArmedTaskbarWindows::default();
        state.start(1, TaskbarHideRestore::FullyHidden);

        assert!(state.is_last(1));

        state.join(2);

        assert!(!state.is_last(1));
        assert!(!state.is_last(2));
    }

    #[test]
    fn armed_taskbar_windows_take_all_clears_the_whole_session_and_returns_the_restore_value_regardless_of_how_many_windows_were_armed() {
        let mut state = ArmedTaskbarWindows::default();
        state.start(1, TaskbarHideRestore::FullyHidden);
        state.join(2);
        state.join(3);

        let restore = state.take_all();

        assert_eq!(restore, Some(TaskbarHideRestore::FullyHidden));
        assert!(!state.is_active());
    }

    #[test]
    fn armed_taskbar_windows_take_all_returns_none_when_nothing_is_armed() {
        let mut state = ArmedTaskbarWindows::default();

        assert_eq!(state.take_all(), None);
    }

    /// Manual, real-OS verification for issue #42's `FullyHidden` mode
    /// (acceptance criterion: "the taskbar does not reappear when the mouse
    /// hovers near the bottom of the screen while hidden" -- not scriptable
    /// without OS-level screen/window-state inspection, but the underlying
    /// `ShowWindow` state change *is* directly observable via
    /// `IsWindowVisible`). Ignored by default (not part of the automated
    /// suite, same OS-facing convention as the rest of this module) --
    /// run explicitly with `cargo test --lib win32::tests::hide_taskbar_completely_and_show_taskbar_actually_change_the_real_taskbars_window_visibility -- --ignored`
    /// on a real desktop session. Mutates the real Shell_TrayWnd on this
    /// machine for the duration of the test, restoring it in every case
    /// (including a failed assertion, via a drop guard) so a test failure
    /// never leaves the real taskbar hidden.
    #[test]
    #[ignore = "OS-facing: mutates the real taskbar window; run manually with --ignored"]
    fn hide_taskbar_completely_and_show_taskbar_actually_change_the_real_taskbars_window_visibility() {
        struct RestoreOnDrop;
        impl Drop for RestoreOnDrop {
            fn drop(&mut self) {
                let _ = show_taskbar();
            }
        }
        let _restore = RestoreOnDrop;

        let hwnd = find_taskbar_window().expect("Shell_TrayWnd must exist on a real desktop session");

        hide_taskbar_completely().expect("hide_taskbar_completely should succeed on a real desktop session");
        let visible_while_hidden = unsafe { IsWindowVisible(hwnd) };
        assert_eq!(visible_while_hidden, 0, "Shell_TrayWnd should be invisible after hide_taskbar_completely");

        show_taskbar().expect("show_taskbar should succeed on a real desktop session");
        let visible_after_show = unsafe { IsWindowVisible(hwnd) };
        assert_ne!(visible_after_show, 0, "Shell_TrayWnd should be visible again after show_taskbar");
    }


    /// Manual, real-OS verification for issue #43's auto-restore-on-close
    /// feature: exercises the exact same two calls
    /// `crate::launch_watcher`'s `EVENT_OBJECT_DESTROY` callback makes
    /// (`take_armed_taskbar_restore_for_window` then `restore_taskbar`),
    /// against the real taskbar, without needing a real second window whose
    /// destroy event would have to actually fire -- `set_taskbar_hide_absolute_armed`/
    /// `take_armed_taskbar_restore_for_window` never dereference the `HWND`
    /// they're given, only store/compare its pointer-identity value (see
    /// `ArmedTaskbarWindows`), so a fabricated `HWND` value stands in for
    /// "the window that was hidden" perfectly well here.
    ///
    /// Ignored by default (not part of the automated suite, same OS-facing
    /// convention as the rest of this module) -- run explicitly with
    /// `cargo test --bin gameframe win32::tests::set_taskbar_hide_absolute_armed_then_take_armed_taskbar_restore_for_window_restores_the_real_taskbar_on_a_simulated_window_close -- --ignored`
    /// on a real desktop session. Mutates the real Shell_TrayWnd on this
    /// machine for the duration of the test, restoring it in every case
    /// (including a failed assertion, via a drop guard) so a test failure
    /// never leaves the real taskbar hidden.
    #[test]
    #[ignore = "OS-facing: mutates the real taskbar window; run manually with --ignored"]
    fn set_taskbar_hide_absolute_armed_then_take_armed_taskbar_restore_for_window_restores_the_real_taskbar_on_a_simulated_window_close() {
        struct RestoreOnDrop;
        impl Drop for RestoreOnDrop {
            fn drop(&mut self) {
                let _ = show_taskbar();
            }
        }
        let _restore = RestoreOnDrop;

        let taskbar_hwnd = find_taskbar_window().expect("Shell_TrayWnd must exist on a real desktop session");
        // A fabricated identity for "the game window that was hidden" --
        // never dereferenced, only compared (see doc comment above).
        let fake_game_hwnd = 0xDEAD_usize as HWND;

        set_taskbar_hide_absolute_armed(fake_game_hwnd, true, TaskbarHideMode::FullyHidden)
            .expect("hide should succeed on a real desktop session");
        let visible_while_hidden = unsafe { IsWindowVisible(taskbar_hwnd) };
        assert_eq!(visible_while_hidden, 0, "Shell_TrayWnd should be invisible after arming a hide for fake_game_hwnd");

        // Simulate `window_destroy_callback`'s own logic exactly: take the
        // armed restore for the "closed" window's HWND, then apply it.
        let restore = take_armed_taskbar_restore_for_window(fake_game_hwnd)
            .expect("a restore should be armed for the window that was just hidden");
        restore_taskbar(restore).expect("restore_taskbar should succeed on a real desktop session");

        let visible_after_restore = unsafe { IsWindowVisible(taskbar_hwnd) };
        assert_ne!(
            visible_after_restore, 0,
            "Shell_TrayWnd should be visible again after the simulated window-close restore"
        );
    }

    /// Companion to the test above: an *unrelated* window closing (i.e. its
    /// `HWND` doesn't match whatever is currently armed) must never trigger
    /// a restore -- acceptance criterion 3 ("no effect"). Same real-OS
    /// caveats as the test above.
    #[test]
    #[ignore = "OS-facing: mutates the real taskbar window; run manually with --ignored"]
    fn take_armed_taskbar_restore_for_window_leaves_an_unrelated_windows_hide_untouched() {
        struct RestoreOnDrop;
        impl Drop for RestoreOnDrop {
            fn drop(&mut self) {
                let _ = show_taskbar();
            }
        }
        let _restore = RestoreOnDrop;

        let taskbar_hwnd = find_taskbar_window().expect("Shell_TrayWnd must exist on a real desktop session");
        let armed_hwnd = 0xDEAD_usize as HWND;
        let unrelated_hwnd = 0xBEEF_usize as HWND;

        set_taskbar_hide_absolute_armed(armed_hwnd, true, TaskbarHideMode::FullyHidden)
            .expect("hide should succeed on a real desktop session");

        assert!(
            take_armed_taskbar_restore_for_window(unrelated_hwnd).is_none(),
            "an unrelated window's destroy event must not take the armed restore"
        );
        let visible_after_unrelated_destroy = unsafe { IsWindowVisible(taskbar_hwnd) };
        assert_eq!(
            visible_after_unrelated_destroy, 0,
            "Shell_TrayWnd should remain hidden -- an unrelated window's close must have no effect"
        );

        // Clean up: take the real armed restore and apply it (also
        // verifies the armed state survived the unrelated take attempt
        // above untouched).
        let restore = take_armed_taskbar_restore_for_window(armed_hwnd)
            .expect("the original armed restore should still be armed after the unrelated take attempt");
        restore_taskbar(restore).expect("restore_taskbar should succeed on a real desktop session");
    }

    /// Real-OS regression test for code review's HIGH finding on issue #43's
    /// original single-slot design: two tracked-game windows both hiding the
    /// taskbar (GameFrame's central multi-game use case, e.g. `Left`+`Right`
    /// side-by-side), then closing one at a time. The taskbar must stay
    /// hidden after the first window closes (the second still wants it
    /// hidden) and only actually restore once the second -- and last --
    /// window closes. Same real-OS caveats as the tests above (fabricated
    /// `HWND`s, never dereferenced -- see the doc comment on the test
    /// above).
    #[test]
    #[ignore = "OS-facing: mutates the real taskbar window; run manually with --ignored"]
    fn set_taskbar_hide_absolute_armed_keeps_the_real_taskbar_hidden_until_the_last_of_two_armed_windows_closes() {
        struct RestoreOnDrop;
        impl Drop for RestoreOnDrop {
            fn drop(&mut self) {
                let _ = show_taskbar();
            }
        }
        let _restore = RestoreOnDrop;

        let taskbar_hwnd = find_taskbar_window().expect("Shell_TrayWnd must exist on a real desktop session");
        let window_a = 0x1111_usize as HWND;
        let window_b = 0x2222_usize as HWND;

        set_taskbar_hide_absolute_armed(window_a, true, TaskbarHideMode::FullyHidden)
            .expect("hiding for window A should succeed on a real desktop session");
        set_taskbar_hide_absolute_armed(window_b, true, TaskbarHideMode::FullyHidden)
            .expect("hiding for window B (joining the already-active session) should succeed");
        let visible_while_both_hidden = unsafe { IsWindowVisible(taskbar_hwnd) };
        assert_eq!(visible_while_both_hidden, 0, "Shell_TrayWnd should be invisible while both windows are armed");

        // Simulate window A closing: the destroy watcher's own logic.
        assert!(
            take_armed_taskbar_restore_for_window(window_a).is_none(),
            "window B is still armed, so window A closing must not yield a restore value yet"
        );
        let visible_after_a_closes = unsafe { IsWindowVisible(taskbar_hwnd) };
        assert_eq!(
            visible_after_a_closes, 0,
            "Shell_TrayWnd must remain hidden -- window B still wants it hidden (the premature-restore bug this test guards against)"
        );

        // Now simulate window B closing -- the last armed window.
        let restore = take_armed_taskbar_restore_for_window(window_b)
            .expect("window B was the last armed window, so a restore value should now be returned");
        restore_taskbar(restore).expect("restore_taskbar should succeed on a real desktop session");

        let visible_after_b_closes = unsafe { IsWindowVisible(taskbar_hwnd) };
        assert_ne!(
            visible_after_b_closes, 0,
            "Shell_TrayWnd should be visible again once the last armed window closes"
        );
    }


    /// Manual, real-OS verification for issue #43's tray-"Exit" auto-restore
    /// path: exercises the exact same two calls `crate::app::run` makes
    /// after `run_event_loop` returns (`take_any_armed_taskbar_restore`
    /// then `restore_taskbar`), against the real taskbar. Same real-OS
    /// caveats as the tests above.
    #[test]
    #[ignore = "OS-facing: mutates the real taskbar window; run manually with --ignored"]
    fn take_any_armed_taskbar_restore_then_restore_taskbar_restores_the_real_taskbar_regardless_of_which_window_was_armed() {
        struct RestoreOnDrop;
        impl Drop for RestoreOnDrop {
            fn drop(&mut self) {
                let _ = show_taskbar();
            }
        }
        let _restore = RestoreOnDrop;

        let taskbar_hwnd = find_taskbar_window().expect("Shell_TrayWnd must exist on a real desktop session");
        let fake_game_hwnd = 0xCAFE_usize as HWND;

        set_taskbar_hide_absolute_armed(fake_game_hwnd, true, TaskbarHideMode::FullyHidden)
            .expect("hide should succeed on a real desktop session");
        let visible_while_hidden = unsafe { IsWindowVisible(taskbar_hwnd) };
        assert_eq!(visible_while_hidden, 0, "Shell_TrayWnd should be invisible after arming a hide");

        // Simulate `crate::app::run`'s own tray-"Exit" logic exactly: take
        // whatever is armed, regardless of which window it was armed for.
        let restore = take_any_armed_taskbar_restore().expect("a restore should be armed after the hide above");
        restore_taskbar(restore).expect("restore_taskbar should succeed on a real desktop session");

        let visible_after_restore = unsafe { IsWindowVisible(taskbar_hwnd) };
        assert_ne!(
            visible_after_restore, 0,
            "Shell_TrayWnd should be visible again after the simulated tray-Exit restore"
        );
    }

    /// Real-OS regression test for the bug this fix addresses: the manual
    /// taskbar-autohide hotkey (`toggle_taskbar_hide`) used to decide
    /// hide-vs-show from its own remembered `Option<TaskbarHideRestore>`,
    /// which never learned about a hide performed by a completely different
    /// call site (a tracked game's launch-detection auto-hide, simulated
    /// here via `set_taskbar_hide_absolute_armed`) -- so the hotkey's first
    /// press after such a hide was a no-op (hiding an already-hidden
    /// taskbar), and the *next* press did the opposite of what was expected.
    /// `toggle_taskbar_hide` now reads the taskbar's real state on every
    /// press instead, so it must correctly show it (was hidden by the
    /// simulated game) and then correctly hide it again (now visible) with
    /// no state of its own passed in at all. Same real-OS caveats as the
    /// tests above.
    #[test]
    #[ignore = "OS-facing: mutates the real taskbar window; run manually with --ignored"]
    fn toggle_taskbar_hide_reflects_the_real_state_regardless_of_who_hid_it_last() {
        struct RestoreOnDrop;
        impl Drop for RestoreOnDrop {
            fn drop(&mut self) {
                let _ = show_taskbar();
            }
        }
        let _restore = RestoreOnDrop;

        let taskbar_hwnd = find_taskbar_window().expect("Shell_TrayWnd must exist on a real desktop session");
        let fake_game_hwnd = 0xF00D_usize as HWND;

        // Simulate a tracked game's launch-detection auto-hide -- a call
        // site `toggle_taskbar_hide` never reads or writes state for.
        set_taskbar_hide_absolute_armed(fake_game_hwnd, true, TaskbarHideMode::FullyHidden)
            .expect("hide should succeed on a real desktop session");
        assert_eq!(
            unsafe { IsWindowVisible(taskbar_hwnd) },
            0,
            "Shell_TrayWnd should be hidden by the simulated game launch"
        );

        // The manual toggle, despite having no memory of that hide, must
        // still detect "currently hidden" from the real OS state and show it.
        toggle_taskbar_hide(TaskbarHideMode::FullyHidden).expect("toggle should succeed on a real desktop session");
        assert_ne!(
            unsafe { IsWindowVisible(taskbar_hwnd) },
            0,
            "toggle should show the taskbar since it was actually hidden"
        );

        // And the very next press must detect "currently visible" and hide
        // it again -- the old remembered-flag design would get this wrong.
        toggle_taskbar_hide(TaskbarHideMode::FullyHidden).expect("toggle should succeed on a real desktop session");
        assert_eq!(
            unsafe { IsWindowVisible(taskbar_hwnd) },
            0,
            "toggle should hide the taskbar again since it was actually visible"
        );

        // Clean up the simulated game's armed-window bookkeeping (the
        // toggle above never touched it, so it's still armed).
        let _ = take_armed_taskbar_restore_for_window(fake_game_hwnd);
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
