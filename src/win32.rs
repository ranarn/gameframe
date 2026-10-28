//! Window manipulation for the borderless-toggle mechanic: stripping/restoring
//! a window's border/title bar, resizing/repositioning it, and disabling
//! Windows 11's rounded-corner rendering while it's borderless. Also holds
//! the OS primitives of the independent taskbar-hide mechanic: every call
//! site that hides the taskbar goes through a mode-aware entry point in
//! [`crate::taskbar`] (`hide_taskbar`/`set_taskbar_hide_absolute`/
//! `toggle_taskbar_hide`), which keeps the crash-recovery marker
//! ([`crate::taskbar_recovery`]) and calls the unmarked primitives here
//! (this module imports neither). It picks between two techniques per
//! [`crate::profile_store::TaskbarHideMode`] -- `FullyHidden`
//! (`ShowWindow(Shell_TrayWnd, SW_HIDE)`, no hover-reveal) or `HoverReveal`
//! (the native `SHAppBarMessage`/`ABS_AUTOHIDE` mechanism). Also owns the
//! "Apply to Window" picker's `EnumWindows` call
//! ([`list_visible_windows`]/`enum_windows_callback`), which calls into
//! [`crate::window_filter`] for the actual two-layer game-window
//! filtering/ranking decision logic.
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
//!   crate's automated tests.
//!
//! `crate::taskbar::toggle_taskbar_hide` keeps no remembered state at all -- it reads the
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
/// `WS_MAXIMIZE` is the window's *state* rather than a border bit, but a
/// stripped window must not stay maximized (the Zone rect would be overridden),
/// so it is cleared with them.
const BORDERLESS_STYLE_MASK: u32 =
    WS_CAPTION | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX | WS_SYSMENU | WS_MAXIMIZE;

/// The extended-style edge bits removed to strip a window: the sunken client
/// edge, the raised window edge and the dialog frame all draw a border that
/// survives removing `WS_CAPTION`/`WS_THICKFRAME`.
const BORDERLESS_EX_STYLE_MASK: u32 = WS_EX_CLIENTEDGE | WS_EX_WINDOWEDGE | WS_EX_DLGMODALFRAME;

/// Removes the border/title-bar style bits (and `WS_MAXIMIZE`) from a raw
/// `GWL_STYLE` value, leaving every other bit (e.g. `WS_VISIBLE`,
/// `WS_POPUP`, `WS_CHILD`) untouched. Pure bit manipulation -- no `HWND`
/// involved.
pub fn strip_border_style(style: u32) -> u32 {
    style & !BORDERLESS_STYLE_MASK
}

/// Removes the border-drawing extended-style bits from a raw `GWL_EXSTYLE`
/// value, leaving every other bit (e.g. `WS_EX_TOPMOST`, `WS_EX_APPWINDOW`)
/// untouched.
pub fn strip_border_ex_style(ex_style: u32) -> u32 {
    ex_style & !BORDERLESS_EX_STYLE_MASK
}

/// What a foreground window looked like immediately before the borderless
/// hotkey most recently stripped its border -- remembered so a second press
/// of the same hotkey can restore it exactly.
///
/// `style` never carries `WS_MAXIMIZE` (see [`RestoreState::new`]) and, for a
/// window that was maximized, `region` is its normal (un-maximized) rect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestoreState {
    pub style: u32,
    pub ex_style: u32,
    pub region: Region,
}

impl RestoreState {
    /// Builds the state to remember for a window with the given live styles.
    /// A maximized window is remembered un-maximized: restoring must not
    /// write `WS_MAXIMIZE` back onto a window that is no longer maximized.
    pub fn new(style: u32, ex_style: u32, region: Region) -> Self {
        Self {
            style: style & !WS_MAXIMIZE,
            ex_style,
            region,
        }
    }
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
    live_ex_style: u32,
    live_region: Region,
    target_region: Region,
) -> (ToggleAction, Option<RestoreState>) {
    match remembered {
        None => {
            let restore = RestoreState::new(live_style, live_ex_style, live_region);
            let new_style = strip_border_style(live_style);
            (
                ToggleAction::MakeBorderless {
                    new_style,
                    region: target_region,
                },
                Some(restore),
            )
        }
        Some(restore) => (ToggleAction::Restore(restore), None),
    }
}

use windows_sys::Win32::UI::WindowsAndMessaging::{
    WS_CAPTION, WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME, WS_EX_WINDOWEDGE, WS_MAXIMIZE,
    WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME,
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
/// [`TaskbarAutohideState`] -- the interpretation `crate::taskbar::set_taskbar_hide_absolute`
/// applies. Pure logic -- no OS call.
pub(crate) fn taskbar_autohide_state_from_bool(auto_hide: bool) -> TaskbarAutohideState {
    if auto_hide {
        TaskbarAutohideState::AutoHidden
    } else {
        TaskbarAutohideState::AlwaysVisible
    }
}

/// Which technique produced a hidden taskbar -- the "how to restore"
/// state: recording *which technique* hid the taskbar
/// (not just an appbar flag value) so a later restore always undoes exactly
/// that, even if [`crate::profile_store::TaskbarHideMode`] changes in
/// between (e.g. the zone editor's taskbar toggle hides it in `FullyHidden`
/// mode, the user flips the setting to `HoverReveal` mid-session, then
/// closes the editor -- restoring must still call `crate::taskbar::show_taskbar`, not
/// write an appbar state that would do nothing to a `ShowWindow`-hidden
/// window).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskbarHideRestore {
    /// Hidden via `ShowWindow(Shell_TrayWnd, SW_HIDE)` -- restoring means
    /// `crate::taskbar::show_taskbar`.
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
/// - `HoverReveal`: remembers `current` verbatim so a restore can put
///   the appbar back exactly as it was found, not just to a hardcoded
///   `AlwaysVisible`.
///
/// Pure decision logic -- no OS call.
pub fn plan_taskbar_hide(
    mode: TaskbarHideMode,
    current: TaskbarAutohideState,
) -> TaskbarHideRestore {
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

/// Why the `FullyHidden` taskbar counts as hidden (see
/// [`judge_taskbar_hidden`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskbarHiddenReason {
    /// Not `WS_VISIBLE` (what `SW_HIDE` produces).
    NotVisible,
    /// `WS_VISIBLE`, but its rect lies (mostly) outside its monitor.
    OffMonitor,
    /// On screen, but another window is topmost at the taskbar's own center
    /// (e.g. a borderless game in the Zone).
    Covered,
}

/// Judges whether the `FullyHidden` taskbar is actually hidden, from plain
/// inputs: `Some(reason)` when it is, `None` when it is really on screen.
/// "Meaningfully on its monitor" means at least half the taskbar's area
/// intersects `monitor` (rects are `(left, top, right, bottom)`).
///
/// With Windows' own auto-hide on (`windows_auto_hide`), a taskbar slid off
/// its monitor and not under its own center is Windows' normal resting
/// state, not one GameFrame hid, so only `visible` decides; otherwise every
/// hotkey press would "show" and the hotkey could never hide it again.
///
/// Pure decision logic -- no OS call.
pub fn judge_taskbar_hidden(
    windows_auto_hide: bool,
    visible: bool,
    taskbar: (i32, i32, i32, i32),
    monitor: (i32, i32, i32, i32),
    on_top_at_center: bool,
) -> Option<TaskbarHiddenReason> {
    if !visible {
        return Some(TaskbarHiddenReason::NotVisible);
    }
    if windows_auto_hide {
        return None;
    }
    let area = |l: i32, t: i32, r: i32, b: i32| {
        (r as i64 - l as i64).max(0) * (b as i64 - t as i64).max(0)
    };
    let own = area(taskbar.0, taskbar.1, taskbar.2, taskbar.3);
    let shared = area(
        taskbar.0.max(monitor.0),
        taskbar.1.max(monitor.1),
        taskbar.2.min(monitor.2),
        taskbar.3.min(monitor.3),
    );
    if own == 0 || shared * 2 < own {
        return Some(TaskbarHiddenReason::OffMonitor);
    }
    if !on_top_at_center {
        return Some(TaskbarHiddenReason::Covered);
    }
    None
}

/// Re-sends the appbar state unchanged: `read` supplies the flags and `send`
/// receives them verbatim as the `ABM_SETSTATE` `lParam` -- no bit flipped,
/// so the user's own auto-hide/always-on-top settings are preserved exactly.
fn resend_appbar_state(read: impl FnOnce() -> u32, send: impl FnOnce(isize)) {
    send(read() as isize);
}

/// Whether joining an already-active hide session must re-issue the OS hide:
/// only when the *session* was started with `FullyHidden` (its remembered
/// restore, not the currently configured mode -- a mode changed mid-session
/// applies from the next session), and whenever the taskbar is `WS_VISIBLE`
/// (any verdict but `NotVisible`) -- on top, covered by the returning game,
/// or off its monitor. The session's restore value is never touched.
pub(crate) fn should_rehide_on_join(
    session: Option<TaskbarHideRestore>,
    judged_hidden: Option<TaskbarHiddenReason>,
) -> bool {
    session == Some(TaskbarHideRestore::FullyHidden)
        && judged_hidden != Some(TaskbarHiddenReason::NotVisible)
}

/// Reasons a live Win32 window-manipulation call can fail.
#[derive(Debug)]
pub enum Win32Error {
    /// The window is minimized (`IsIconic`), so its bounding rectangle would
    /// be its off-screen minimized position (roughly `(-32000, -32000)`)
    /// rather than a real, restorable region. Returned by both
    /// [`toggle_borderless_on_window`] (for the foreground window)
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
    /// The target window's thread is not responding (`IsHungAppWindow`), so
    /// no call into it was made; the next auto-apply, or the drift re-check
    /// the drift handler schedules ([`HUNG_RECHECK_INTERVAL`]), retries.
    WindowHung,
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

    /// Whether the call was skipped because the target window is hung. The
    /// skip is already logged once per window by the win32 layer, so callers
    /// leave it out of their own failure logs.
    pub fn is_window_hung(&self) -> bool {
        matches!(self, Win32Error::WindowHung)
    }
}

impl std::fmt::Display for Win32Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Win32Error::WindowMinimized => write!(f, "window is minimized"),
            Win32Error::WindowHung => write!(f, "window is not responding"),
            Win32Error::Os(e) => write!(f, "Win32 window manipulation failed: {e}"),
            Win32Error::Hresult(hr) => write!(
                f,
                "Win32 window manipulation failed: HRESULT {:#010x}",
                *hr as u32
            ),
            Win32Error::TaskbarNotFound => {
                write!(f, "could not find the taskbar window (Shell_TrayWnd)")
            }
        }
    }
}

impl std::error::Error for Win32Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Win32Error::WindowMinimized => None,
            Win32Error::WindowHung => None,
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
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DEFAULT, DWMWCP_DONOTROUND, DwmSetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::UI::Shell::{
    ABM_GETSTATE, ABM_SETSTATE, ABS_AUTOHIDE, APPBARDATA, QUERY_USER_NOTIFICATION_STATE,
    QUNS_RUNNING_D3D_FULL_SCREEN, SHAppBarMessage, SHQueryUserNotificationState, ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GA_ROOT, GWL_EXSTYLE, GWL_STYLE, GetAncestor, GetForegroundWindow,
    GetWindowLongPtrW, GetWindowPlacement, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, HWND_TOP, IsHungAppWindow, IsIconic, IsWindowVisible, IsZoomed,
    SET_WINDOW_POS_FLAGS, SW_HIDE, SW_RESTORE, SW_SHOW, SW_SHOWNORMAL, SWP_ASYNCWINDOWPOS,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, ShowWindowAsync, WINDOW_LONG_PTR_INDEX, WINDOWPLACEMENT,
    WindowFromPoint,
};

/// Reads a window's current `GWL_STYLE` value.
///
/// Per `GetWindowLongPtrW`'s documented contract, a return value of zero is
/// ambiguous (it can be either a genuine style value of `0` or a failure),
/// so -- mirroring [`set_window_style`] below -- `SetLastError(0)` is called
/// first and a non-zero `GetLastError()` afterwards is the real failure
/// signal.
fn get_window_style(hwnd: HWND) -> Result<u32, Win32Error> {
    get_window_long(hwnd, GWL_STYLE)
}

/// Reads a window's current `GWL_EXSTYLE` value (see [`get_window_style`]).
fn get_window_ex_style(hwnd: HWND) -> Result<u32, Win32Error> {
    get_window_long(hwnd, GWL_EXSTYLE)
}

fn get_window_long(hwnd: HWND, index: WINDOW_LONG_PTR_INDEX) -> Result<u32, Win32Error> {
    // Safety: `hwnd` is a live handle; `GetWindowLongPtrW` performs no writes
    // and takes no pointers besides `hwnd` itself.
    let style = unsafe {
        windows_sys::Win32::Foundation::SetLastError(0);
        let style = GetWindowLongPtrW(hwnd, index);
        if style == 0 {
            let last_error = windows_sys::Win32::Foundation::GetLastError();
            if last_error != 0 {
                return Err(Win32Error::Os(std::io::Error::from_raw_os_error(
                    last_error as i32,
                )));
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
    set_window_long(hwnd, GWL_STYLE, style)
}

/// Writes a new `GWL_EXSTYLE` value to a window (see [`set_window_style`]).
fn set_window_ex_style(hwnd: HWND, ex_style: u32) -> Result<(), Win32Error> {
    set_window_long(hwnd, GWL_EXSTYLE, ex_style)
}

fn set_window_long(hwnd: HWND, index: WINDOW_LONG_PTR_INDEX, style: u32) -> Result<(), Win32Error> {
    // Safety: `hwnd` is a live handle; `SetWindowLongPtrW` performs no reads
    // besides `hwnd`/`style` and takes no pointers.
    unsafe {
        windows_sys::Win32::Foundation::SetLastError(0);
        let previous_style = SetWindowLongPtrW(hwnd, index, style as isize);
        if previous_style == 0 {
            let last_error = windows_sys::Win32::Foundation::GetLastError();
            if last_error != 0 {
                return Err(Win32Error::Os(std::io::Error::from_raw_os_error(
                    last_error as i32,
                )));
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
    Ok(Region {
        x: rect.left,
        y: rect.top,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
    })
}

/// The region to remember as a window's original look: its bounding rect, or,
/// for a maximized window (whose rect is the full-screen one, not somewhere
/// restorable), its normal rect from `GetWindowPlacement`.
fn get_restorable_region(hwnd: HWND, maximized: bool) -> Result<Region, Win32Error> {
    if !maximized {
        return get_window_region(hwnd);
    }
    let mut placement: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
    placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
    // Safety: `hwnd` is a live handle; `placement` is a valid out-pointer
    // with `length` set, as `GetWindowPlacement` requires.
    if unsafe { GetWindowPlacement(hwnd, &mut placement) } == 0 {
        return get_window_region(hwnd);
    }
    let rect = placement.rcNormalPosition;
    Ok(Region {
        x: rect.left,
        y: rect.top,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
    })
}

/// Un-maximizes `hwnd` without blocking on its thread (`ShowWindowAsync`
/// queues `SW_RESTORE` to the owning thread). The caller has already passed
/// the hung check.
fn restore_maximized_window(hwnd: HWND) {
    // Safety: `hwnd` is a live handle; `ShowWindowAsync` takes plain integers.
    unsafe {
        ShowWindowAsync(hwnd, SW_RESTORE);
    }
}

/// Whether `hwnd` is currently maximized.
fn is_maximized(hwnd: HWND) -> bool {
    // Safety: `IsZoomed` only reads window state.
    unsafe { IsZoomed(hwnd) != 0 }
}

/// Moves/resizes a window to `region` without changing its z-order or
/// activation state (the borderless toggle shouldn't steal focus or restack
/// the window relative to its siblings). Includes `SWP_FRAMECHANGED`, which
/// `SetWindowPos`'s documented contract requires after a `GWL_STYLE` change
/// (via [`set_window_style`]) for the non-client area (title bar) to actually
/// be recalculated/repainted -- without it, a target region that happens to
/// already match the window's current bounds could leave the title bar
/// visually unchanged despite the style change underneath.
///
/// Uses `SWP_ASYNCWINDOWPOS`: the call queues the move to the owning thread
/// and returns, so a busy game cannot block the caller. `Ok` therefore means
/// "queued"; the window has not necessarily moved yet, so a rect read right
/// after must not be taken as the result (the drift watcher re-checks on the
/// location-change event the move itself raises).
fn set_window_region(hwnd: HWND, region: Region) -> Result<(), Win32Error> {
    const FLAGS: SET_WINDOW_POS_FLAGS =
        SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_ASYNCWINDOWPOS;
    // Safety: `hwnd` is a live handle; `SetWindowPos` takes only plain
    // integers besides it.
    let ok = unsafe {
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            region.x,
            region.y,
            region.width,
            region.height,
            FLAGS,
        )
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
pub fn window_drifted(
    current_region: Region,
    current_style: u32,
    current_ex_style: u32,
    target: Region,
) -> bool {
    current_region != target
        || strip_border_style(current_style) != current_style
        || strip_border_ex_style(current_ex_style) != current_ex_style
}

/// The pre-strip [`RestoreState`] of every window GameFrame has stripped,
/// keyed by window handle, plus the windows the user restored by hand. Pure
/// data -- no OS calls -- so the per-window toggle decision
/// ([`toggle_with_store`]) is unit testable.
#[derive(Debug, Default)]
struct RestoreStore {
    states: std::collections::BTreeMap<isize, RestoreState>,
    /// Windows the user restored with the hotkey: auto-apply and drift
    /// re-apply leave them alone until the window is destroyed or the user
    /// strips it again.
    opted_out: std::collections::BTreeSet<isize>,
}

impl RestoreStore {
    const fn new() -> Self {
        Self {
            states: std::collections::BTreeMap::new(),
            opted_out: std::collections::BTreeSet::new(),
        }
    }

    fn get(&self, window: isize) -> Option<RestoreState> {
        self.states.get(&window).copied()
    }

    /// Records `state` unless the window already has one: the first state
    /// recorded is the window's original bordered look and must survive any
    /// later re-apply.
    ///
    /// A state whose style has no border to strip is not recorded: a window
    /// that is already borderless the first time GameFrame sees it has no
    /// truer original to go back to, and the hotkey must not claim a restore
    /// it cannot do.
    fn record_if_absent(&mut self, window: isize, state: RestoreState) {
        // `WS_MAXIMIZE` is not a border: a maximized borderless window has
        // nothing to restore either.
        let borderless = strip_border_style(state.style) == state.style & !WS_MAXIMIZE
            && strip_border_ex_style(state.ex_style) == state.ex_style;
        if borderless {
            return;
        }
        self.states.entry(window).or_insert(state);
    }

    /// Drops everything remembered about `window` (it was destroyed).
    fn forget(&mut self, window: isize) {
        self.states.remove(&window);
        self.opted_out.remove(&window);
    }
}

static RESTORE_STATES: std::sync::Mutex<RestoreStore> = std::sync::Mutex::new(RestoreStore::new());

fn lock_store(store: &std::sync::Mutex<RestoreStore>) -> std::sync::MutexGuard<'_, RestoreStore> {
    store.lock().unwrap_or_else(|p| p.into_inner())
}

/// Whether `hwnd` has a remembered pre-strip state, i.e. the next borderless
/// hotkey press on it restores rather than strips.
pub fn has_restore_state(hwnd: HWND) -> bool {
    lock_store(&RESTORE_STATES).get(hwnd as isize).is_some()
}

/// Decides and performs one borderless-toggle press for `window`.
///
/// No remembered state: the pre-strip state is committed *before* `mutate`
/// runs, so a partial failure still leaves the original recoverable. A
/// remembered state: `mutate` restores it, and the entry is removed only if
/// that fully succeeded. Other windows' entries never influence the press.
///
/// `store` is locked only around each read/write, never while `mutate` runs.
fn toggle_with_store<E>(
    store: &std::sync::Mutex<RestoreStore>,
    window: isize,
    live_style: u32,
    live_ex_style: u32,
    live_region: Region,
    target_region: Region,
    mutate: impl FnOnce(ToggleAction) -> Result<(), E>,
) -> Result<(), E> {
    let remembered = lock_store(store).get(window);
    let (action, next_state) = plan_toggle(
        remembered,
        live_style,
        live_ex_style,
        live_region,
        target_region,
    );
    match action {
        ToggleAction::MakeBorderless { .. } => {
            {
                let mut guard = lock_store(store);
                guard.opted_out.remove(&window);
                if let Some(state) = next_state {
                    guard.record_if_absent(window, state);
                }
            }
            mutate(action)
        }
        ToggleAction::Restore(_) => {
            mutate(action)?;
            let mut guard = lock_store(store);
            guard.states.remove(&window);
            guard.opted_out.insert(window);
            Ok(())
        }
    }
}

/// Windows GameFrame applied the Zone to, as `(window handle, owning process
/// id)`. The pid is captured at registration because a destroyed window can no
/// longer be asked for it.
static APPLIED_WINDOWS: std::sync::Mutex<Vec<(isize, u32)>> = std::sync::Mutex::new(Vec::new());

/// Whether the user restored `hwnd` with the borderless hotkey, so auto-apply
/// and drift re-apply must leave it alone until it is destroyed or stripped
/// again.
pub fn auto_apply_suppressed(hwnd: HWND) -> bool {
    lock_store(&RESTORE_STATES)
        .opted_out
        .contains(&(hwnd as isize))
}

/// Lifts the suppression set by a hotkey restore: the user explicitly asked
/// for `hwnd` to be placed again (tray picker).
pub fn allow_auto_apply(hwnd: HWND) {
    lock_store(&RESTORE_STATES)
        .opted_out
        .remove(&(hwnd as isize));
}

/// Remembers `hwnd` as one GameFrame applied the Zone to, so the
/// location-change watcher can cheaply (one mutex lock + scan) ignore every
/// other window's move events.
pub fn remember_applied_window(hwnd: HWND) {
    let pid = window_process_id(hwnd);
    {
        let mut windows = APPLIED_WINDOWS.lock().unwrap_or_else(|p| p.into_inner());
        match windows.iter_mut().find(|(h, _)| *h == hwnd as isize) {
            // A reused handle (or one first seen with an unreadable pid) takes
            // the live owner's pid instead of keeping a stale one.
            Some(entry) => {
                if pid != 0 {
                    entry.1 = pid;
                }
            }
            None => windows.push((hwnd as isize, pid)),
        }
    }
    crate::launch_watcher::sync_drift_hooks();
}

/// The process id owning `hwnd`, 0 when it can't be read (stale handle).
fn window_process_id(hwnd: HWND) -> u32 {
    let mut pid: u32 = 0;
    // Safety: `&mut pid` is a valid out-pointer; a stale handle just yields 0.
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    pid
}

/// Whether `hwnd` was previously registered via [`remember_applied_window`].
pub fn is_applied_window(hwnd: HWND) -> bool {
    APPLIED_WINDOWS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .any(|(h, _)| *h == hwnd as isize)
}

/// Process ids owning at least one applied window (never 0).
fn pids_of(entries: &[(isize, u32)]) -> std::collections::BTreeSet<u32> {
    entries
        .iter()
        .map(|(_, pid)| *pid)
        .filter(|pid| *pid != 0)
        .collect()
}

/// Brings registry `entries` in line with the live windows. `probe` maps a
/// window handle to `None` when it is no longer a window, else its owning pid
/// (0 if unreadable). Dead windows (killed without an `EVENT_OBJECT_DESTROY`)
/// are dropped, and an entry whose pid was unknown or changed takes the live
/// one. Returns whether any entry still has no pid, so the caller re-checks later.
fn reconcile_entries(
    entries: &mut Vec<(isize, u32)>,
    probe: impl Fn(isize) -> Option<u32>,
) -> bool {
    entries.retain_mut(|(hwnd, pid)| match probe(*hwnd) {
        None => false,
        Some(live) => {
            if live != 0 {
                *pid = live;
            }
            true
        }
    });
    entries.iter().any(|(_, pid)| *pid == 0)
}

/// Reconciles the applied-window registry with the live windows (see
/// [`reconcile_entries`]) and returns the pids owning applied windows plus
/// whether some window's pid is still unknown.
pub fn refresh_applied_window_pids() -> (std::collections::BTreeSet<u32>, bool) {
    let mut windows = APPLIED_WINDOWS.lock().unwrap_or_else(|p| p.into_inner());
    let unresolved = reconcile_entries(&mut windows, |h| {
        let hwnd = h as HWND;
        // Safety: `IsWindow` tolerates any handle value.
        if unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsWindow(hwnd) } == 0 {
            None
        } else {
            Some(window_process_id(hwnd))
        }
    });
    (pids_of(&windows), unresolved)
}

/// Minimum time between two drift re-apply attempts on one window. A game that
/// keeps restyling itself would otherwise be fought on every location event;
/// one second still corrects a settings change promptly while capping the
/// attempts (and any failure log lines) at about one per second per window.
const DRIFT_REAPPLY_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Consecutive drift re-applies on one window within
/// [`DRIFT_REAPPLY_CAP_WINDOW`] after which GameFrame stops re-applying it.
const DRIFT_REAPPLY_CAP: usize = 3;

/// The span [`DRIFT_REAPPLY_CAP`] re-applies must fall in to cap a window.
const DRIFT_REAPPLY_CAP_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

/// What to do about a location-change event on an applied window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriftDecision {
    Reapply,
    SkipNotDrifted,
    /// The window is in exclusive fullscreen (see [`is_fullscreen`]): left alone.
    SkipFullscreen,
    /// The window drifted back [`DRIFT_REAPPLY_CAP`] times within
    /// [`DRIFT_REAPPLY_CAP_WINDOW`] and was re-applied each time; it is left
    /// alone until the user applies or restores it, or it is destroyed.
    SkipCapped,
    /// Another attempt on this window happened less than
    /// [`DRIFT_REAPPLY_MIN_INTERVAL`] ago; the event should be re-checked once
    /// `retry_in` has passed.
    SkipRateLimited {
        retry_in: std::time::Duration,
    },
}

/// Pure decision for one drift event. `since_last_attempt` is the time since
/// this window's previous re-apply attempt, `None` if it has had none.
fn decide_drift_reapply(
    drifted: bool,
    fullscreen: bool,
    capped: bool,
    since_last_attempt: Option<std::time::Duration>,
) -> DriftDecision {
    if !drifted {
        DriftDecision::SkipNotDrifted
    } else if fullscreen {
        DriftDecision::SkipFullscreen
    } else if capped {
        DriftDecision::SkipCapped
    } else if let Some(since) = since_last_attempt.filter(|d| *d < DRIFT_REAPPLY_MIN_INTERVAL) {
        DriftDecision::SkipRateLimited {
            retry_in: DRIFT_REAPPLY_MIN_INTERVAL - since,
        }
    } else {
        DriftDecision::Reapply
    }
}

/// Recent drift re-apply attempts of each window (keyed by window handle) and
/// the windows that hit [`DRIFT_REAPPLY_CAP`].
#[derive(Debug, Default)]
struct DriftAttempts {
    recent: std::collections::BTreeMap<isize, Vec<std::time::Instant>>,
    capped: std::collections::BTreeSet<isize>,
    /// Capped windows whose cap is already logged.
    cap_logged: std::collections::BTreeSet<isize>,
}

impl DriftAttempts {
    const fn new() -> Self {
        Self {
            recent: std::collections::BTreeMap::new(),
            capped: std::collections::BTreeSet::new(),
            cap_logged: std::collections::BTreeSet::new(),
        }
    }

    /// Decides for `window` at `now`. Does not record an attempt; the caller
    /// does that with [`Self::record`] once it actually re-applies.
    fn decide(
        &self,
        window: isize,
        now: std::time::Instant,
        drifted: bool,
        fullscreen: bool,
    ) -> DriftDecision {
        let since = self
            .recent
            .get(&window)
            .and_then(|attempts| attempts.last())
            .map(|t| now.saturating_duration_since(*t));
        decide_drift_reapply(drifted, fullscreen, self.capped.contains(&window), since)
    }

    /// Records a re-apply attempt. The attempt that reaches the cap caps the
    /// window: later events are skipped ([`DriftDecision::SkipCapped`]).
    fn record(&mut self, window: isize, now: std::time::Instant) {
        let attempts = self.recent.entry(window).or_default();
        attempts.retain(|t| now.saturating_duration_since(*t) < DRIFT_REAPPLY_CAP_WINDOW);
        attempts.push(now);
        if attempts.len() >= DRIFT_REAPPLY_CAP {
            self.capped.insert(window);
        }
    }

    /// Notes that a skipped event of `window` is being reported. True the first
    /// time since the cap was set, so the caller logs it once.
    fn note_capped(&mut self, window: isize) -> bool {
        self.cap_logged.insert(window)
    }

    /// Forgets `window`'s attempts and lifts its cap.
    fn reset(&mut self, window: isize) {
        self.recent.remove(&window);
        self.capped.remove(&window);
        self.cap_logged.remove(&window);
    }
}

static DRIFT_ATTEMPTS: std::sync::Mutex<DriftAttempts> =
    std::sync::Mutex::new(DriftAttempts::new());

/// Whether `hwnd` is minimized (`IsIconic`).
pub(crate) fn is_window_minimized(hwnd: HWND) -> bool {
    // Safety: `IsIconic` only reads window state and tolerates a stale handle.
    unsafe { IsIconic(hwnd) != 0 }
}

/// Decides whether a drift event on `hwnd` should re-apply the Zone.
pub(crate) fn decide_drift_for_window(hwnd: HWND, drifted: bool) -> DriftDecision {
    // Only a drifted window needs the live fullscreen read.
    let fullscreen = drifted && window_is_fullscreen(hwnd);
    DRIFT_ATTEMPTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .decide(
            hwnd as isize,
            std::time::Instant::now(),
            drifted,
            fullscreen,
        )
}

/// Records now as the time of `hwnd`'s latest drift re-apply attempt.
pub(crate) fn record_drift_attempt(hwnd: HWND) {
    DRIFT_ATTEMPTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .record(hwnd as isize, std::time::Instant::now());
}

/// True the first time a capped `hwnd`'s event is skipped, so the caller logs
/// the cap once, when it actually stops a re-apply.
pub(crate) fn note_drift_capped(hwnd: HWND) -> bool {
    DRIFT_ATTEMPTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .note_capped(hwnd as isize)
}

/// Lifts `hwnd`'s drift re-apply cap and forgets its recent attempts. Called
/// for a placement the user asked for (the hotkey or the tray picker).
pub fn reset_drift_cap(hwnd: HWND) {
    DRIFT_ATTEMPTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .reset(hwnd as isize);
}

/// Pure decision: is a window in exclusive fullscreen? `d3d_fullscreen` is
/// `SHQueryUserNotificationState() == QUNS_RUNNING_D3D_FULL_SCREEN`, which is
/// system-wide, so it only counts for the foreground window. Only the drift
/// re-apply of a window GameFrame already placed asks: that window sits in the
/// smaller Zone, so the signal then means the game itself went fullscreen.
/// Auto-apply never asks, since the same signal can be reported for a
/// borderless window covering its monitor, which GameFrame must shrink.
pub(crate) fn is_fullscreen(d3d_fullscreen: bool, foreground: bool) -> bool {
    d3d_fullscreen && foreground
}

/// Reads the shell's notification state and applies [`is_fullscreen`] to
/// `hwnd`. OS-facing.
pub(crate) fn window_is_fullscreen(hwnd: HWND) -> bool {
    let mut state: QUERY_USER_NOTIFICATION_STATE = 0;
    // Safety: `state` is a valid out-pointer.
    let d3d = unsafe { SHQueryUserNotificationState(&mut state) } >= 0
        && state == QUNS_RUNNING_D3D_FULL_SCREEN;
    // Safety: `GetForegroundWindow` takes no arguments.
    let foreground = unsafe { GetForegroundWindow() } == hwnd;
    is_fullscreen(d3d, foreground)
}

/// Forgets `hwnd` (called when it is destroyed) along with its remembered
/// restore state, drift attempt time and hung-window log entry.
pub fn forget_applied_window(hwnd: HWND) {
    unmark_applied_window(hwnd);
    lock_store(&RESTORE_STATES).forget(hwnd as isize);
    lock_hung_log().logged.remove(&(hwnd as isize));
}

/// Stops the drift watcher treating `hwnd` as GameFrame-placed and drops its
/// drift attempt time.
fn unmark_applied_window(hwnd: HWND) {
    DRIFT_ATTEMPTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .reset(hwnd as isize);
    APPLIED_WINDOWS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|(h, _)| *h != hwnd as isize);
    crate::launch_watcher::sync_drift_hooks();
}

/// Reads `hwnd`'s live bounds/style and reports [`window_drifted`] against
/// `target`. OS-facing.
pub fn window_has_drifted(hwnd: HWND, target: Region) -> Result<bool, Win32Error> {
    Ok(window_drifted(
        get_window_region(hwnd)?,
        get_window_style(hwnd)?,
        get_window_ex_style(hwnd)?,
        target,
    ))
}

/// What one apply request does to a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApplyDecision {
    Apply,
    /// The window already has the stripped style and the Zone rect: no style
    /// or position call is made (the same check the drift path uses).
    SkipInPlace,
    /// The window's thread is not responding: no call is made.
    SkipHung,
}

/// Pure decision for one apply request. A hung window wins over everything
/// else, since even reading it back would be pointless.
pub(crate) fn decide_apply(hung: bool, drifted: bool) -> ApplyDecision {
    if hung {
        ApplyDecision::SkipHung
    } else if drifted {
        ApplyDecision::Apply
    } else {
        ApplyDecision::SkipInPlace
    }
}

/// Windows whose hung skip is already logged, so a hung game costs one log
/// line, not one per foreground event.
#[derive(Debug, Default)]
struct HungSkipLog {
    logged: std::collections::BTreeSet<isize>,
}

impl HungSkipLog {
    const fn new() -> Self {
        Self {
            logged: std::collections::BTreeSet::new(),
        }
    }

    /// Records one apply request on `window`; true when its skip should be
    /// logged (first time since it was last responsive). A responsive window
    /// re-arms the log.
    fn note(&mut self, window: isize, hung: bool) -> bool {
        if hung {
            self.logged.insert(window)
        } else {
            self.logged.remove(&window);
            false
        }
    }
}

static HUNG_SKIP_LOG: std::sync::Mutex<HungSkipLog> = std::sync::Mutex::new(HungSkipLog::new());

/// How long the drift handler waits before re-checking a window it skipped
/// because it was not responding.
pub(crate) const HUNG_RECHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Poison-tolerant lock on [`HUNG_SKIP_LOG`]: the set has no invariant a panic
/// could break.
fn lock_hung_log() -> std::sync::MutexGuard<'static, HungSkipLog> {
    HUNG_SKIP_LOG.lock().unwrap_or_else(|p| p.into_inner())
}

/// Whether `hwnd`'s thread has stopped pumping messages (`IsHungAppWindow`).
/// Logs the first request on a window that is hung. Every blocking call into
/// a foreign window (a game or the taskbar) comes after this check.
pub(crate) fn is_window_hung_logged(hwnd: HWND) -> bool {
    // Safety: `IsHungAppWindow` only reads window state and tolerates a stale
    // handle.
    let hung = unsafe { IsHungAppWindow(hwnd) } != 0;
    let first = lock_hung_log().note(hwnd as isize, hung);
    if first {
        log!("window {hwnd:?} is not responding; skipping calls into it until it is");
    }
    hung
}

/// Strips a window's border and moves/resizes it to `target_region`: the
/// shared "make borderless" mutation shared by
/// [`toggle_borderless_on_window`] (acting on
/// `GetForegroundWindow()`'s result) and the tray menu's "Apply to Window"
/// picker (acting on an explicit window the user chose, which may not be the
/// foreground window at all -- see [`crate::app`]'s dispatch of a decoded
/// window-item id via [`crate::tray_ui::decode_window_item_id`]). Reads
/// `hwnd`'s current style itself (rather than taking a pre-computed one) so
/// callers only need a target `HWND` and region -- neither caller needs to
/// separately read/strip the style first.
///
/// Once the style change succeeds, records the window's pre-strip
/// style/region as its [`RestoreState`] unless it already has one, so the
/// borderless hotkey can later restore a window placed by any path (tray
/// picker, auto-apply, drift re-apply) to its original look. A window that
/// was already borderless has no border to restore and gets no entry (see
/// [`RestoreStore::record_if_absent`]).
///
/// Mirrors [`toggle_borderless_on_window`]'s own minimized-window
/// guard: a minimized window's `GetWindowRect` reports its off-screen
/// minimized position (roughly `(-32000, -32000)`), not a real, restorable
/// region, so applying a target region to it would move/resize it based on
/// that bogus rect. The eligibility filter
/// ([`crate::window_filter::eligibility_for_hwnd`]) has no minimized check
/// (`IsWindowVisible` stays `TRUE` for minimized windows), so a minimized
/// window can legitimately reach this function via the
/// tray-menu path -- this guard is what keeps that path's OS-mutation
/// behavior identical to the hotkey path's, not just sharing the three-call
/// mutation sequence.
///
/// OS-facing: acts on the real window identified by `hwnd`. Not exercised by
/// this crate's automated tests -- see module docs.
pub fn apply_borderless_to_window(hwnd: HWND, target_region: Region) -> Result<(), Win32Error> {
    // Safety: `hwnd` is a live handle passed in by the caller; `IsIconic`
    // performs no writes.
    if unsafe { IsIconic(hwnd) } != 0 {
        return Err(Win32Error::WindowMinimized);
    }

    let hung = is_window_hung_logged(hwnd);
    // A maximized window is un-maximized first (queued, behind the hung check)
    // so the Zone rect is not overridden by the maximized state. Its normal
    // rect is read before the restore lands and remembered as the original.
    let maximized = !hung && is_maximized(hwnd);
    let restore_region = if maximized {
        let region = get_restorable_region(hwnd, true);
        restore_maximized_window(hwnd);
        Some(region)
    } else {
        None
    };
    let drifted = !hung && window_has_drifted(hwnd, target_region).unwrap_or(true);
    match decide_apply(hung, drifted) {
        ApplyDecision::SkipHung => return Err(Win32Error::WindowHung),
        // Already stripped and in place: no style or position call (every
        // foreground switch lands here). The corner preference is only a DWM
        // attribute write that does not wait on the game's thread, and a
        // window first seen already in place still needs it.
        ApplyDecision::SkipInPlace => return disable_rounded_corners(hwnd),
        ApplyDecision::Apply => {}
    }

    let live_style = get_window_style(hwnd)?;
    let live_ex_style = get_window_ex_style(hwnd)?;
    let live_region = match restore_region {
        Some(region) => region?,
        None => get_window_region(hwnd)?,
    };
    set_window_style(hwnd, strip_border_style(live_style))?;
    // Recorded once the first mutation succeeded, so a window GameFrame could
    // not change (e.g. an elevated game) never gets a restore entry; the
    // remaining calls below can still fail with the original already safe.
    lock_store(&RESTORE_STATES).record_if_absent(
        hwnd as isize,
        RestoreState::new(live_style, live_ex_style, live_region),
    );
    set_window_ex_style(hwnd, strip_border_ex_style(live_ex_style))?;
    set_window_region(hwnd, target_region)?;
    disable_rounded_corners(hwnd)?;
    Ok(())
}

/// Performs the borderless-toggle hotkey's full effect on `hwnd` (the
/// foreground window): strips its border and moves it to `target_region` (if
/// it has no remembered state), or restores it to exactly the style/region
/// remembered from its first strip (if it has). The remembered state is kept
/// per window, so other windows never affect the press; see
/// [`toggle_with_store`].
///
/// OS-facing: acts on the real window. Not exercised by this crate's
/// automated tests -- see module docs.
pub fn toggle_borderless_on_window(hwnd: HWND, target_region: Region) -> Result<(), Win32Error> {
    // A minimized window's `GetWindowRect` reports its off-screen minimized
    // position (roughly `(-32000, -32000)`), not a real, restorable region
    // -- bail out before that gets read/remembered/applied as if it were
    // one.
    //
    // Safety: `hwnd` is a live handle; `IsIconic` performs no writes.
    if unsafe { IsIconic(hwnd) } != 0 {
        return Err(Win32Error::WindowMinimized);
    }
    // Both directions write to the game (style, then position).
    if is_window_hung_logged(hwnd) {
        return Err(Win32Error::WindowHung);
    }

    let live_style = get_window_style(hwnd)?;
    let live_ex_style = get_window_ex_style(hwnd)?;
    // A maximized window remembers its normal rect, not the full-screen one.
    let live_region = get_restorable_region(hwnd, live_style & WS_MAXIMIZE != 0)?;

    toggle_with_store(
        &RESTORE_STATES,
        hwnd as isize,
        live_style,
        live_ex_style,
        live_region,
        target_region,
        |action| match action {
            // The mutation is delegated to [`apply_borderless_to_window`]
            // (shared with the tray-menu and auto-apply paths). The
            // pre-strip state was already committed by
            // [`toggle_with_store`], so a partial failure here still leaves
            // the original restorable.
            ToggleAction::MakeBorderless { region, .. } => {
                reset_drift_cap(hwnd);
                apply_borderless_to_window(hwnd, region)
            }
            // The entry is only removed once all three restore calls
            // succeeded; a failure partway keeps the only copy of the
            // original state so a retry can recover it.
            ToggleAction::Restore(restore) => {
                set_window_style(hwnd, restore.style)?;
                set_window_ex_style(hwnd, restore.ex_style)?;
                set_window_region(hwnd, restore.region)?;
                reset_corner_preference(hwnd)?;
                // A restored window is no longer GameFrame-placed: stop the
                // drift watcher from re-stripping it. [`toggle_with_store`]
                // also marks it as restored by the user, which keeps
                // auto-apply (including its re-apply on every foreground
                // event) away from it until it is destroyed or stripped again.
                unmark_applied_window(hwnd);
                Ok(())
            }
        },
    )
}

/// Resolves the current foreground window's `HWND`, via `GetForegroundWindow()`.
///
/// Returns `None` if there is no foreground window. Used directly by
/// [`crate::app::handle_toggle_borderless`], which needs the raw `HWND` to
/// look up and act on that window.
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs.
pub fn foreground_window() -> Option<HWND> {
    // Safety: `GetForegroundWindow` takes no arguments and performs no
    // writes; its result is null-checked immediately below before any other
    // call uses it.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() { None } else { Some(hwnd) }
}

/// `ShowWindow(taskbar, SW_HIDE)` -- the `FullyHidden` mode's technique:
/// unlike the native appbar auto-hide mechanism, this does not reappear on
/// mouse hover near the screen edge. Pure OS call: the recovery-marker
/// bookkeeping lives in `crate::taskbar`, which every hide goes through.
///
/// OS-facing: sends a real `ShowWindow` call. Not exercised by this crate's
/// automated tests -- see module docs.
///
/// Synchronous on purpose, so a later show can never be overtaken by a queued
/// hide; the caller checks [`is_window_hung_logged`] first and skips a hung
/// Explorer (never queue a hide behind a later show).
///
/// # Unmarked: call through `crate::taskbar`
/// Does not touch the recovery marker; only `crate::taskbar` may call it.
pub(crate) fn hide_taskbar_window(hwnd: HWND) {
    // Safety: `hwnd` is a live handle just resolved by the caller;
    // `ShowWindow` takes a plain `i32` command and writes through no pointers.
    unsafe {
        ShowWindow(hwnd, SW_HIDE);
    }
}

/// The full normal show, without marker bookkeeping or the record lock: the
/// shape `crate::taskbar::show_taskbar` and startup recovery use. Undoes
/// [`hide_taskbar_window`]. Does not touch the appbar auto-hide state beyond
/// re-sending it unchanged (per this module's `FullyHidden`/`HoverReveal`
/// split -- see [`TaskbarHideRestore`]'s doc comment).
///
/// Returns the appbar flags read just after the show, before the state
/// re-send: the session-end caller compares them with the session start.
///
/// # Unmarked: call through `crate::taskbar`
/// Does not touch the recovery marker; only `crate::taskbar` (and `taskbar_recovery`) may call it.
pub(crate) fn show_taskbar_full() -> Result<u32, Win32Error> {
    let hwnd = show_taskbar_window()?;
    // A hung Explorer got only the posted show: the appbar re-send and the
    // raise below would block on it. The failure keeps the recovery mark, so
    // a later restore or the next start finishes the job.
    if taskbar_is_hung(hwnd) {
        return Err(Win32Error::WindowHung);
    }
    // `SW_SHOW` alone does not re-lay-out or raise a taskbar that a game
    // covered or that slid away. Re-send the appbar state read just now,
    // unchanged, then raise without activating.
    // Unknown flags (Explorer hung since the check above) are not guessed.
    let flags = read_appbar_flags(hwnd).ok_or(Win32Error::WindowHung)?;
    let mut sent = Ok(());
    resend_appbar_state(|| flags, |lparam| sent = send_appbar_setstate(hwnd, lparam));
    sent?;
    // Safety: live handle; zero position/size are ignored via SWP_NOMOVE |
    // SWP_NOSIZE, and SWP_NOACTIVATE keeps focus where it is.
    unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOP,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
        );
    }
    Ok(flags)
}

/// Whether the taskbar's thread has stopped pumping messages, logged once per
/// hung episode (callers rely on that and don't log `WindowHung` again). Not
/// used on the panic hook's bare show, which must take no lock.
fn taskbar_is_hung(hwnd: HWND) -> bool {
    is_window_hung_logged(hwnd)
}

/// The bare `ShowWindow(SW_SHOW)` behind [`show_taskbar_full`] and the panic
/// hook's [`restore_taskbar_raw`]: one call, no marker bookkeeping, no lock,
/// nothing else, so it stays safe there. Returns the taskbar handle.
fn show_taskbar_window() -> Result<HWND, Win32Error> {
    let hwnd = find_taskbar_window()?;
    // Safety: `hwnd` is a live handle just resolved above; neither call
    // writes through pointers, only a plain `i32` command. A hung Explorer
    // gets the show posted instead of sent, so neither the panic hook nor the
    // UI thread waits on it.
    unsafe {
        if IsHungAppWindow(hwnd) != 0 {
            ShowWindowAsync(hwnd, SW_SHOW);
        } else {
            ShowWindow(hwnd, SW_SHOW);
        }
    }
    Ok(hwnd)
}

/// What [`gather_taskbar_judgement`] read from the OS, with the verdict
/// [`judge_taskbar_hidden`] drew from it.
#[derive(Clone, Copy)]
pub(crate) struct TaskbarJudgement {
    pub(crate) taskbar: HWND,
    pub(crate) verdict: Option<TaskbarHiddenReason>,
    visible: bool,
    rect: RECT,
    on_top_at_center: bool,
}

/// Gathers the plain inputs for [`judge_taskbar_hidden`] from the OS and
/// judges them.
pub(crate) fn gather_taskbar_judgement(taskbar: HWND) -> TaskbarJudgement {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    // Safety: plain queries on a live handle with valid out-pointers.
    let (visible, on_top, monitor_ok, rect_ok) = unsafe {
        let rect_ok = GetWindowRect(taskbar, &mut rect) != 0;
        let center = POINT {
            x: (rect.left + rect.right) / 2,
            y: (rect.top + rect.bottom) / 2,
        };
        let at_center = WindowFromPoint(center);
        let on_top = !at_center.is_null() && GetAncestor(at_center, GA_ROOT) == taskbar;
        let monitor = MonitorFromWindow(taskbar, MONITOR_DEFAULTTONEAREST);
        let monitor_ok = GetMonitorInfoW(monitor, &mut info) != 0;
        (IsWindowVisible(taskbar) != 0, on_top, monitor_ok, rect_ok)
    };
    let m = info.rcMonitor;
    // If the monitor cannot be read, don't claim "off monitor": use the
    // taskbar's own rect so only visibility/cover decide.
    let monitor = if monitor_ok {
        (m.left, m.top, m.right, m.bottom)
    } else {
        (rect.left, rect.top, rect.right, rect.bottom)
    };
    if !rect_ok {
        log!("taskbar judgement: GetWindowRect failed; rect is a zeroed placeholder");
    }
    // Unknown flags (Explorer hung) are judged like Windows' auto-hide:
    // visibility alone decides, so nothing is acted on from a guess.
    let windows_auto_hide = read_appbar_flags(taskbar).is_none_or(|f| f & ABS_AUTOHIDE != 0);
    // Without a real rect, position and cover can't be judged: fall back to
    // visibility alone, exactly as under Windows' auto-hide.
    let verdict = judge_taskbar_hidden(
        windows_auto_hide || !rect_ok,
        visible,
        (rect.left, rect.top, rect.right, rect.bottom),
        monitor,
        on_top,
    );
    TaskbarJudgement {
        taskbar,
        verdict,
        visible,
        rect,
        on_top_at_center: on_top,
    }
}

/// Which `FullyHidden` restore a raw path performs (see
/// [`crate::taskbar_recovery::show_kind_for`] for who picks which).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowKind {
    /// A bare `ShowWindow(SW_SHOW)`: one lock-free call, nothing else.
    Bare,
    /// `SW_SHOW`, the appbar state re-send and the `HWND_TOP` raise (#52).
    Full,
}

/// A restore without any marker bookkeeping or locking -- the panic hook's
/// and startup recovery's variant ([`crate::taskbar_recovery`]), which must
/// not touch a lock the panicking thread may already hold.
///
/// `show` picks the `FullyHidden` show.
pub(crate) fn restore_taskbar_raw(
    remembered: TaskbarHideRestore,
    show: ShowKind,
) -> Result<(), Win32Error> {
    match remembered {
        TaskbarHideRestore::FullyHidden => match show {
            ShowKind::Bare => show_taskbar_window().map(|_| ()),
            ShowKind::Full => show_taskbar_full().map(|_| ()),
        },
        TaskbarHideRestore::HoverReveal(state) => set_appbar_state(state),
    }
}

/// Reads the taskbar as it really is: `(window visible, appbar auto-hide
/// state). Used by startup recovery. The state is None when Explorer is
/// hung (unknown, not AlwaysVisible). OS-facing.
pub(crate) fn observe_taskbar() -> Result<(bool, Option<TaskbarAutohideState>), Win32Error> {
    let hwnd = find_taskbar_window()?;
    // Safety: `hwnd` is a live handle just resolved above.
    let visible = unsafe { IsWindowVisible(hwnd) } != 0;
    Ok((visible, read_appbar_flags(hwnd).map(state_from_flags)))
}

/// Whether the taskbar is already hidden by any means, for the zone editor's
/// one read at open: not `WS_VISIBLE` (a FullyHidden hide), or Windows'
/// auto-hide flag set (a HoverReveal hide, or the user's own auto-hide
/// setting). Either way the editor must not take ownership of it. Direct
/// reads only, no on-screen judgement. `false` when the taskbar cannot be
/// found; `true` when its auto-hide state is unknown (Explorer hung), so the
/// editor leaves it alone.
///
/// OS-facing; not exercised by this crate's automated tests -- see module docs.
pub fn taskbar_hidden_flag() -> bool {
    let Ok(hwnd) = find_taskbar_window() else {
        return false;
    };
    // Safety: plain query on a live handle.
    let visible = unsafe { IsWindowVisible(hwnd) } != 0;
    !visible || read_appbar_flags(hwnd).is_none_or(|f| f & ABS_AUTOHIDE != 0)
}

/// `crate::taskbar::taskbar_debug_state` for an already-gathered judgement;
/// `armed` is `(armed windows, has a remembered restore)`, which the caller
/// owns.
pub(crate) fn describe_taskbar(judgement: &TaskbarJudgement, armed: (usize, bool)) -> String {
    let TaskbarJudgement {
        taskbar,
        verdict,
        visible,
        rect,
        on_top_at_center: on_top,
    } = *judgement;
    // Safety: plain query, no arguments.
    let foreground = unsafe { GetForegroundWindow() };
    let flags =
        read_appbar_flags(taskbar).map_or_else(|| "unknown".to_string(), |f| format!("{f:#x}"));
    let foreground_exe = if foreground.is_null() {
        "none".to_string()
    } else {
        window_filter::exe_path_for_window(foreground)
            .map(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .unwrap_or_else(|| "?".to_string())
    };
    let (armed, has_restore) = armed;
    format!(
        "visible={visible} appbar_flags={flags} rect=({},{})-({},{}) on_top_at_center={on_top} \
         judged_hidden={verdict:?} foreground={foreground:?} ({foreground_exe}) armed_windows={armed} armed_restore={has_restore}",
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
///
/// # Unmarked: call through `crate::taskbar`
/// Does not touch the recovery marker; only `crate::taskbar` (and `taskbar_recovery`) may call it.
pub(crate) fn set_appbar_state(state: TaskbarAutohideState) -> Result<(), Win32Error> {
    let hwnd = find_taskbar_window()?;
    // Never write from a guessed value: that would clear always-on-top.
    let current_flags = read_appbar_flags(hwnd).ok_or(Win32Error::WindowHung)?;
    write_appbar_state(hwnd, current_flags, state)
}

/// Sends the actual `SHAppBarMessage(ABM_SETSTATE, ...)` write, given a
/// taskbar `hwnd` and its `current_flags` already read via
/// [`read_appbar_flags`]. Split out from [`set_appbar_state`] so callers that
/// already have both in hand (e.g. `crate::taskbar::hide_taskbar`'s `HoverReveal` arm, which reads
/// them once to derive the state to force) don't have to pay for a redundant
/// [`find_taskbar_window`] + [`read_appbar_flags`] round trip just to write.
///
/// # Unmarked: call through `crate::taskbar`
/// Does not touch the recovery marker; only `crate::taskbar` may call it.
pub(crate) fn write_appbar_state(
    hwnd: HWND,
    current_flags: u32,
    state: TaskbarAutohideState,
) -> Result<(), Win32Error> {
    send_appbar_setstate(hwnd, apply_autohide_state(current_flags, state) as isize)
}

/// Sends SHAppBarMessage(ABM_SETSTATE) with the given full-state lParam.
/// Fails with [Win32Error::WindowHung] without sending when Explorer is hung
/// (the call is sent to its thread and would block), so callers keep their
/// recovery mark.
fn send_appbar_setstate(hwnd: HWND, lparam: isize) -> Result<(), Win32Error> {
    if taskbar_is_hung(hwnd) {
        return Err(Win32Error::WindowHung);
    }
    let mut data: APPBARDATA = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of::<APPBARDATA>() as u32;
    data.hWnd = hwnd;
    data.lParam = lparam;

    // Safety: `data` is a validly-sized, zero-initialized `APPBARDATA` with
    // `cbSize`/`hWnd`/`lParam` populated per `SHAppBarMessage`'s documented
    // `ABM_SETSTATE` contract; `SHAppBarMessage` takes only `dwMessage` and a
    // pointer to `data`, both valid here.
    unsafe {
        SHAppBarMessage(ABM_SETSTATE, &mut data);
    }
    Ok(())
}

/// Reads the appbar's current `ABM_GETSTATE` flags (a combination of
/// `ABS_AUTOHIDE`/`ABS_ALWAYSONTOP`) for the taskbar window `hwnd`. Shared
/// by [`set_appbar_state`] (which needs the current flags to preserve the
/// always-on-top bit -- see [`apply_autohide_state`]'s doc comment) and
/// `crate::taskbar::hide_taskbar`'s `HoverReveal` arm (which needs only the auto-hide bit, via
/// [`state_from_flags`]).
///
/// Returns None (flags unknown) without calling when the taskbar's thread
/// is hung: ABM_GETSTATE is sent to Explorer and would block on it. Callers
/// must not treat unknown as `0` (no auto-hide, not always-on-top).
pub(crate) fn read_appbar_flags(hwnd: HWND) -> Option<u32> {
    if taskbar_is_hung(hwnd) {
        return None;
    }
    let mut data: APPBARDATA = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of::<APPBARDATA>() as u32;
    data.hWnd = hwnd;

    // Safety: `data` is a validly-sized, zero-initialized `APPBARDATA` with
    // `cbSize`/`hWnd` populated per `SHAppBarMessage`'s documented
    // `ABM_GETSTATE` contract; `SHAppBarMessage` takes only `dwMessage` and a
    // pointer to `data`, both valid here. `ABM_GETSTATE` does not read
    // `data.lParam`, so it's left zeroed.
    Some(unsafe { SHAppBarMessage(ABM_GETSTATE, &mut data) as u32 })
}

/// Interprets `ABM_GETSTATE`'s raw flags (as read by [`read_appbar_flags`])
/// as a [`TaskbarAutohideState`]. Used by `crate::taskbar::hide_taskbar`'s `HoverReveal` arm,
/// which reads the taskbar's actual current auto-hide state directly from
/// the OS (as opposed to whatever a caller's own [`TaskbarAutohideState`]
/// variable happens to say -- that variable, e.g. `app::run_event_loop`'s
/// `taskbar_autohide_state`, is local to a different call site and may not
/// be in scope where the zone editor is launched from) without a separate
/// [`find_taskbar_window`] + [`read_appbar_flags`] round trip of its own.
pub(crate) fn state_from_flags(current_flags: u32) -> TaskbarAutohideState {
    if current_flags & ABS_AUTOHIDE != 0 {
        TaskbarAutohideState::AutoHidden
    } else {
        TaskbarAutohideState::AlwaysVisible
    }
}

/// Finds the taskbar's own window handle via `FindWindowW(L"Shell_TrayWnd", NULL)`.
pub(crate) fn find_taskbar_window() -> Result<HWND, Win32Error> {
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
    // The flag tells the child to wait for this (old) instance to release
    // the single-instance mutex instead of exiting on first contact.
    let params = encode_wide(crate::single_instance::ELEVATED_RELAUNCH_FLAG);

    // Safety: `verb`, `file` and `params` are valid, null-terminated UTF-16
    // buffers kept alive for the duration of this call; the remaining
    // pointer arguments (`hwnd`, `lpdirectory`) are null, which
    // `ShellExecuteW` documents as valid ("use current/none").
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
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
        return Err(Win32Error::Os(std::io::Error::from_raw_os_error(
            code as i32,
        )));
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
    /// Executable paths of the tracked games; their processes are never
    /// module-snapshotted.
    tracked_exes: Vec<std::path::PathBuf>,
}

/// Lists the pickable top-level windows, likely games first. Blocking and
/// slow (window walk, exe paths, module snapshots, Steam scan): call it from
/// a worker thread, never the UI thread. `tracked_exes` are the tracked games'
/// executable paths.
pub fn list_visible_windows(tracked_exes: Vec<std::path::PathBuf>) -> Vec<(HWND, String)> {
    let mut state = EnumWindowsState {
        windows: Vec::new(),
        rank_cache: std::collections::HashMap::new(),
        tracked_exes,
    };
    // Safety: `state` outlives this call (it's a local that isn't dropped
    // until after `EnumWindows` returns); it's cast to a raw pointer solely
    // to smuggle it through `EnumWindows`'s documented `LPARAM` userdata
    // parameter, and `enum_windows_callback` below casts it back to the
    // exact same type before dereferencing it.
    unsafe {
        EnumWindows(
            Some(enum_windows_callback),
            std::ptr::addr_of_mut!(state) as isize,
        );
    }
    // Layer 2: a stable sort
    // by descending `WindowRank` surfaces likely games first in the picker,
    // without ever removing a window ranked `WindowRank::Unknown` -- ranking
    // only ever reorders, it never filters. `Vec::sort_by_key` is a stable
    // sort, so windows that tie on rank keep the relative order `EnumWindows`
    // produced them in (roughly Z-order).
    state.windows.sort_by_key(|w| std::cmp::Reverse(w.2));
    state
        .windows
        .into_iter()
        .map(|(hwnd, title, _rank)| (hwnd, title))
        .collect()
}

unsafe extern "system" fn enum_windows_callback(hwnd: HWND, lparam: isize) -> i32 {
    // Safety: `lparam` is exactly the pointer `list_visible_windows` passed
    // in, valid for the duration of the `EnumWindows` call that is
    // synchronously invoking this callback.
    let state = unsafe { &mut *(lparam as *mut EnumWindowsState) };

    // A hung window can't be applied to anyway, and querying it may block.
    if is_window_hung_logged(hwnd) {
        return 1;
    }

    // An untitled window isn't something a user could sensibly pick by name.
    let title = window_text(hwnd);
    if window_filter::is_listable_title(&title)
        && let Ok(exe_path) = window_filter::eligibility_for_hwnd(hwnd)
    {
        let rank = window_filter::rank_window_for_hwnd(
            hwnd,
            &exe_path,
            &state.tracked_exes,
            &mut state.rank_cache,
        );
        state.windows.push((hwnd, title, rank));
    }
    1 // TRUE: keep enumerating.
}

/// Reads a window's title via `GetWindowTextLengthW`/`GetWindowTextW`,
/// returning an empty string if the window has no title (or either call
/// fails) rather than propagating an error -- an untitled window is simply
/// filtered out by [`crate::window_filter::is_listable_title`], not treated
/// as a hard failure of the whole enumeration.
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
/// [`windows_for_pid`]) to treat as "the" window for the live
/// re-apply, when a tracked game's process owns more than one -- the pure
/// decision behind [`resolve_hwnd_for_pid`], separated out so it's testable
/// without a real `EnumWindows`/`GetWindowRect`/`GetForegroundWindow` call.
///
/// Disambiguation order: prefer the
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
/// a single PID rather than the whole desktop, and with no ranking (the
/// live re-apply doesn't need a "most game-like" ordering, only a set of
/// candidates to disambiguate via [`choose_candidate_window`]).
struct PidWindowsState {
    target_pid: u32,
    windows: Vec<HWND>,
    /// Why each of the process's other top-level windows was refused.
    refusals: Vec<window_filter::Refusal>,
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

    // The same eligibility check the picker uses, minus its non-blank-title
    // requirement: a live re-apply target doesn't need a human-readable title.
    match window_filter::eligibility_for_hwnd(hwnd) {
        Ok(_) => state.windows.push(hwnd),
        Err(refusal) => state.refusals.push(refusal),
    }
    1 // TRUE: keep enumerating.
}

/// Enumerates every top-level window belonging to `target_pid` that passes
/// the same Layer 1 canonical-window filtering the "Apply to Window" picker
/// uses (see [`enum_windows_for_pid_callback`]) -- the candidate-gathering
/// step behind [`resolve_hwnd_for_pid`]. A separate, PID-scoped enumeration
/// rather than a `pid` filter parameter added to [`list_visible_windows`]
/// itself, leaving that function (and the picker it serves) independent of
/// this one.
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs.
fn windows_for_pid(target_pid: u32) -> (Vec<HWND>, Vec<window_filter::Refusal>) {
    let mut state = PidWindowsState {
        target_pid,
        windows: Vec::new(),
        refusals: Vec::new(),
    };
    // Safety: `state` outlives this call; see `list_visible_windows`'s
    // identical `EnumWindows` safety comment for why casting it through a
    // raw pointer here is sound.
    unsafe {
        EnumWindows(
            Some(enum_windows_for_pid_callback),
            std::ptr::addr_of_mut!(state) as isize,
        );
    }
    (state.windows, state.refusals)
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
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
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
    get_window_region(hwnd)
        .map(|r| r.width as i64 * r.height as i64)
        .unwrap_or(0)
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
    let (windows, refusals) = windows_for_pid(pid);
    if windows.is_empty() && !refusals.is_empty() {
        let mut reasons: Vec<String> = refusals.iter().map(ToString::to_string).collect();
        reasons.sort();
        reasons.dedup();
        log!(
            "no eligible window for process {pid}: {}",
            reasons.join("; ")
        );
    }
    let candidates: Vec<(HWND, i64)> = windows
        .into_iter()
        .map(|hwnd| (hwnd, window_area(hwnd)))
        .collect();
    choose_candidate_window(&candidates, foreground_window())
}

/// Resolves a running window for `exe_path`, for the live re-apply:
/// no live window handle is persisted in config, so each commit-triggered
/// re-apply must rediscover the tracked game's current window (if any) from
/// scratch via a fresh toolhelp snapshot + window enumeration.
///
/// Returns `None` if no running process matches `exe_path` (the game isn't
/// currently running -- callers must silently skip it: its setting still
/// saves normally and applies on its next launch/hotkey/tray-apply) or if one
/// does but it has no listable top-level window (e.g. still starting up with
/// no window shown yet).
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs.
pub(crate) fn resolve_running_window(exe_path: &Path) -> Option<HWND> {
    let pid = find_pid_by_exe_path(exe_path)?;
    resolve_hwnd_for_pid(pid)
}

/// The outcome of a single tracked game's live re-apply attempt (see
/// `crate::taskbar::live_reapply_running_window`), for the caller (`crate::settings_ui`) to
/// decide what -- if anything -- to surface in its non-blocking status line.
///
/// Deliberately has no `NotRunning` variant: whether the game is currently
/// running is decided by the caller *before* the live re-apply
/// (`crate::taskbar::live_reapply_running_window`) or a region is ever invoked -- via [`resolve_running_window`] -- so that a not-running
/// game always short-circuits ahead of any region-resolution error and is
/// never routed through this type at all (region resolution must never run,
/// let alone be reported as a failure, for a game that was never actually
/// running).
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
        let target = Region {
            x: 1660,
            y: 0,
            width: 3460,
            height: 1440,
        };
        let drifted = Region {
            x: 632,
            y: 0,
            width: 3856,
            height: 1479,
        };

        assert!(window_drifted(drifted, 0x14CF0000, 0, target));
    }

    #[test]
    fn window_drifted_detects_a_restored_caption_even_when_bounds_match() {
        let target = Region {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };

        assert!(window_drifted(target, 0x14CF0000, 0, target));
    }

    #[test]
    fn window_drifted_is_false_for_the_state_gameframe_itself_applied() {
        let target = Region {
            x: 1660,
            y: 0,
            width: 3460,
            height: 1440,
        };

        assert!(!window_drifted(target, 0x14000000, 0, target));
    }

    #[test]
    fn strip_border_ex_style_removes_the_leftover_edge_bits() {
        const WS_EX_TOPMOST: u32 = 0x8;
        let ex = WS_EX_CLIENTEDGE | WS_EX_WINDOWEDGE | WS_EX_DLGMODALFRAME | WS_EX_TOPMOST;

        assert_eq!(strip_border_ex_style(ex), WS_EX_TOPMOST);
    }

    #[test]
    fn strip_border_style_clears_the_maximized_state() {
        const WS_VISIBLE: u32 = 0x10000000;

        assert_eq!(
            strip_border_style(WS_MAXIMIZE | WS_CAPTION | WS_VISIBLE),
            WS_VISIBLE
        );
    }

    #[test]
    fn window_drifted_detects_a_maximized_window_even_when_bounds_match() {
        let target = Region {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };

        assert!(window_drifted(target, 0x14000000 | WS_MAXIMIZE, 0, target));
    }

    #[test]
    fn window_drifted_detects_a_returned_edge_even_when_bounds_match() {
        let target = Region {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };

        assert!(window_drifted(target, 0x14000000, WS_EX_CLIENTEDGE, target));
    }

    #[test]
    fn a_maximized_window_is_remembered_un_maximized() {
        let state = RestoreState::new(WS_CAPTION | WS_MAXIMIZE, WS_EX_WINDOWEDGE, ORIGINAL);

        assert_eq!(state.style, WS_CAPTION);
        assert_eq!(state.ex_style, WS_EX_WINDOWEDGE);
        assert_eq!(state.region, ORIGINAL);
    }

    #[test]
    fn a_window_with_only_leftover_edges_still_gets_a_restore_state() {
        let mut store = RestoreStore::new();
        let state = RestoreState::new(0, WS_EX_CLIENTEDGE, ORIGINAL);

        store.record_if_absent(1, state);

        assert_eq!(store.get(1), Some(state));
    }

    #[test]
    fn a_maximized_but_borderless_window_gets_no_restore_state() {
        let mut store = RestoreStore::new();

        store.record_if_absent(1, RestoreState::new(WS_MAXIMIZE, 0, ORIGINAL));

        assert_eq!(store.get(1), None);
    }

    #[test]
    fn the_ex_style_round_trips_through_a_toggle() {
        let store = std::sync::Mutex::new(RestoreStore::new());
        let ex = WS_EX_CLIENTEDGE | WS_EX_DLGMODALFRAME;
        let mut seen = Vec::new();

        for _ in 0..2 {
            let _ = toggle_with_store(&store, 1, BORDERED, ex, ORIGINAL, TARGET, |a| {
                seen.push(a);
                Ok::<(), ()>(())
            });
        }

        assert_eq!(
            seen[1],
            ToggleAction::Restore(RestoreState::new(BORDERED, ex, ORIGINAL))
        );
    }

    #[test]
    fn apply_is_skipped_for_a_window_already_in_place() {
        assert_eq!(decide_apply(false, false), ApplyDecision::SkipInPlace);
    }

    #[test]
    fn apply_goes_ahead_for_a_window_that_drifted() {
        assert_eq!(decide_apply(false, true), ApplyDecision::Apply);
    }

    #[test]
    fn apply_is_skipped_for_a_hung_window_whatever_its_state() {
        assert_eq!(decide_apply(true, true), ApplyDecision::SkipHung);
        assert_eq!(decide_apply(true, false), ApplyDecision::SkipHung);
    }

    #[test]
    fn d3d_fullscreen_only_counts_for_the_foreground_window() {
        assert!(is_fullscreen(true, true));
        assert!(!is_fullscreen(true, false));
        assert!(!is_fullscreen(false, true));
    }

    #[test]
    fn drift_decision_leaves_a_fullscreen_window_alone() {
        let attempts = DriftAttempts::default();

        assert_eq!(
            attempts.decide(1, std::time::Instant::now(), true, true),
            DriftDecision::SkipFullscreen
        );
    }

    #[test]
    fn drift_reapply_runs_three_times_within_a_minute_then_stops() {
        let mut attempts = DriftAttempts::default();
        let t0 = std::time::Instant::now();
        let secs = std::time::Duration::from_secs;

        for i in 0..3 {
            let now = t0 + secs(2 * i);
            assert_eq!(
                attempts.decide(1, now, true, false),
                DriftDecision::Reapply,
                "attempt {} still runs",
                i + 1
            );
            attempts.record(1, now);
        }
        // Sticky: even long after the minute, until reset.
        assert_eq!(
            attempts.decide(1, t0 + secs(300), true, false),
            DriftDecision::SkipCapped
        );
        // Another window is unaffected.
        assert_eq!(
            attempts.decide(2, t0 + secs(5), true, false),
            DriftDecision::Reapply
        );
    }

    #[test]
    fn a_capped_window_is_reported_once_until_reset() {
        let mut attempts = DriftAttempts::default();

        assert!(attempts.note_capped(1));
        assert!(!attempts.note_capped(1));
        assert!(attempts.note_capped(2), "another window has its own line");

        attempts.reset(1);

        assert!(attempts.note_capped(1), "a reset window reports a new cap");
    }

    #[test]
    fn drift_attempts_spread_over_more_than_a_minute_never_cap() {
        let mut attempts = DriftAttempts::default();
        let t0 = std::time::Instant::now();
        let secs = std::time::Duration::from_secs;

        for i in 0..6 {
            attempts.record(1, t0 + secs(40 * i));
        }
        assert_eq!(
            attempts.decide(1, t0 + secs(240), true, false),
            DriftDecision::Reapply
        );
    }

    #[test]
    fn resetting_a_window_lifts_its_drift_cap() {
        let mut attempts = DriftAttempts::default();
        let t0 = std::time::Instant::now();
        for i in 0..3 {
            attempts.record(1, t0 + std::time::Duration::from_secs(i));
        }
        assert_eq!(
            attempts.decide(1, t0 + std::time::Duration::from_secs(10), true, false),
            DriftDecision::SkipCapped
        );

        attempts.reset(1);

        assert_eq!(
            attempts.decide(1, t0 + std::time::Duration::from_secs(10), true, false),
            DriftDecision::Reapply
        );
    }

    #[test]
    fn a_hung_window_is_logged_once_until_it_responds_again() {
        let mut log = HungSkipLog::default();

        assert!(log.note(7, true));
        assert!(!log.note(7, true));
        assert!(log.note(8, true), "another window has its own line");
        assert!(!log.note(7, false));
        assert!(log.note(7, true), "a recovered window logs a new hang");
    }

    #[test]
    fn win32_error_window_hung_is_flagged_and_displays() {
        assert!(Win32Error::WindowHung.is_window_hung());
        assert!(!Win32Error::WindowMinimized.is_window_hung());
        assert!(!Win32Error::WindowHung.is_access_denied());
        assert_eq!(
            Win32Error::WindowHung.to_string(),
            "window is not responding"
        );
    }

    #[test]
    fn drift_decision_skips_a_window_that_has_not_drifted() {
        let attempts = DriftAttempts::default();

        assert_eq!(
            attempts.decide(1, std::time::Instant::now(), false, false),
            DriftDecision::SkipNotDrifted
        );
    }

    #[test]
    fn drift_decision_rate_limits_repeat_attempts_on_one_window() {
        let mut attempts = DriftAttempts::default();
        let t0 = std::time::Instant::now();
        let step = std::time::Duration::from_millis;

        assert_eq!(attempts.decide(1, t0, true, false), DriftDecision::Reapply);
        attempts.record(1, t0);
        assert_eq!(
            attempts.decide(1, t0 + step(400), true, false),
            DriftDecision::SkipRateLimited {
                retry_in: step(600)
            }
        );
        assert_eq!(
            attempts.decide(1, t0 + step(1000), true, false),
            DriftDecision::Reapply
        );
    }

    #[test]
    fn drift_decision_limits_two_windows_independently() {
        let mut attempts = DriftAttempts::default();
        let t0 = std::time::Instant::now();

        attempts.record(1, t0);

        assert!(matches!(
            attempts.decide(1, t0, true, false),
            DriftDecision::SkipRateLimited { .. }
        ));
        assert_eq!(attempts.decide(2, t0, true, false), DriftDecision::Reapply);
    }

    #[test]
    fn drift_decision_does_not_limit_a_window_that_was_never_recorded() {
        let attempts = DriftAttempts::default();
        let t0 = std::time::Instant::now();

        assert_eq!(attempts.decide(1, t0, true, false), DriftDecision::Reapply);
        assert_eq!(attempts.decide(1, t0, true, false), DriftDecision::Reapply);
    }

    #[test]
    fn two_windows_of_one_process_keep_its_pid_when_one_is_removed() {
        let mut entries = vec![(1isize, 42u32), (2, 42), (3, 7)];
        assert_eq!(pids_of(&entries), [7, 42].into_iter().collect());
        entries.retain(|(h, _)| *h != 1);
        assert_eq!(pids_of(&entries), [7, 42].into_iter().collect());
        entries.retain(|(h, _)| *h != 2);
        assert_eq!(pids_of(&entries), [7].into_iter().collect());
    }

    #[test]
    fn unknown_pids_never_count_as_a_process() {
        assert!(pids_of(&[(1, 0)]).is_empty());
    }

    #[test]
    fn reconcile_drops_dead_windows() {
        let mut entries = vec![(1isize, 42u32), (2, 43)];
        let unresolved = reconcile_entries(&mut entries, |h| (h == 2).then_some(43));
        assert_eq!(entries, vec![(2, 43)]);
        assert!(!unresolved);
    }

    #[test]
    fn reconcile_fills_in_a_pid_that_was_unreadable_before() {
        let mut entries = vec![(1isize, 0u32)];
        assert!(!reconcile_entries(&mut entries, |_| Some(42)));
        assert_eq!(entries, vec![(1, 42)]);
    }

    #[test]
    fn reconcile_reports_a_pid_that_is_still_unreadable() {
        let mut entries = vec![(1isize, 0u32)];
        assert!(reconcile_entries(&mut entries, |_| Some(0)));
        assert_eq!(entries, vec![(1, 0)]);
    }

    #[test]
    fn reconcile_follows_a_reused_handle_to_its_new_owner() {
        let mut entries = vec![(1isize, 42u32)];
        reconcile_entries(&mut entries, |_| Some(99));
        assert_eq!(entries, vec![(1, 99)]);
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
        let live_region = Region {
            x: 10,
            y: 20,
            width: 800,
            height: 600,
        };
        let target_region = Region {
            x: 0,
            y: 0,
            width: 960,
            height: 1080,
        };

        let (action, next_state) = plan_toggle(None, live_style, 0, live_region, target_region);

        assert_eq!(
            action,
            ToggleAction::MakeBorderless {
                new_style: strip_border_style(live_style),
                region: target_region
            }
        );
        assert_eq!(
            next_state,
            Some(RestoreState {
                style: live_style,
                ex_style: 0,
                region: live_region
            })
        );
    }

    #[test]
    fn plan_toggle_with_a_remembered_state_restores_it_and_forgets_it() {
        let remembered = RestoreState {
            style: WS_CAPTION | WS_THICKFRAME,
            ex_style: 0,
            region: Region {
                x: 10,
                y: 20,
                width: 800,
                height: 600,
            },
        };
        // Live style/region reflect the currently-borderless window; they're
        // irrelevant to this branch since we're restoring the remembered
        // state, not the live one.
        let live_style = strip_border_style(remembered.style);
        let target_region = Region {
            x: 0,
            y: 0,
            width: 960,
            height: 1080,
        };

        let (action, next_state) = plan_toggle(
            Some(remembered),
            live_style,
            0,
            target_region,
            target_region,
        );

        assert_eq!(action, ToggleAction::Restore(remembered));
        assert_eq!(next_state, None);
    }

    #[test]
    fn pressing_the_hotkey_twice_round_trips_back_to_the_original_style_and_region() {
        let original_style = WS_CAPTION | WS_THICKFRAME | WS_SYSMENU;
        let original_region = Region {
            x: 10,
            y: 20,
            width: 800,
            height: 600,
        };
        let target_region = Region {
            x: 0,
            y: 0,
            width: 960,
            height: 1080,
        };

        let (first_action, state_after_first) =
            plan_toggle(None, original_style, 0, original_region, target_region);
        let ToggleAction::MakeBorderless {
            new_style: stripped_style,
            ..
        } = first_action
        else {
            panic!("expected MakeBorderless on the first press");
        };

        let (second_action, state_after_second) = plan_toggle(
            state_after_first,
            stripped_style,
            0,
            target_region,
            target_region,
        );

        assert_eq!(
            second_action,
            ToggleAction::Restore(RestoreState {
                style: original_style,
                ex_style: 0,
                region: original_region
            })
        );
        assert_eq!(state_after_second, None);
    }

    const BORDERED: u32 = WS_CAPTION | WS_THICKFRAME | WS_SYSMENU;
    const ORIGINAL: Region = Region {
        x: 10,
        y: 20,
        width: 800,
        height: 600,
    };
    const TARGET: Region = Region {
        x: 0,
        y: 0,
        width: 960,
        height: 1080,
    };

    fn press(
        store: &std::sync::Mutex<RestoreStore>,
        window: isize,
        mutate_result: Result<(), ()>,
    ) -> Vec<ToggleAction> {
        let mut seen = Vec::new();
        let _ = toggle_with_store(store, window, BORDERED, 0, ORIGINAL, TARGET, |action| {
            seen.push(action);
            mutate_result
        });
        seen
    }

    #[test]
    fn toggle_with_store_strips_a_window_it_has_no_state_for_and_remembers_it() {
        let store = std::sync::Mutex::new(RestoreStore::new());

        let actions = press(&store, 1, Ok(()));

        assert!(matches!(actions[..], [ToggleAction::MakeBorderless { .. }]));
        assert_eq!(
            lock_store(&store).get(1),
            Some(RestoreState {
                style: BORDERED,
                ex_style: 0,
                region: ORIGINAL
            })
        );
    }

    #[test]
    fn toggle_with_store_restores_a_stripped_window_and_forgets_it() {
        let store = std::sync::Mutex::new(RestoreStore::new());
        press(&store, 1, Ok(()));

        let actions = press(&store, 1, Ok(()));

        assert_eq!(
            actions,
            vec![ToggleAction::Restore(RestoreState {
                style: BORDERED,
                ex_style: 0,
                region: ORIGINAL
            })]
        );
        assert_eq!(lock_store(&store).get(1), None);
    }

    #[test]
    fn toggle_with_store_keeps_the_entry_when_a_restore_fails() {
        let store = std::sync::Mutex::new(RestoreStore::new());
        press(&store, 1, Ok(()));

        press(&store, 1, Err(()));

        assert!(lock_store(&store).get(1).is_some());
    }

    #[test]
    fn toggle_with_store_keeps_the_entry_when_a_strip_fails_partway() {
        let store = std::sync::Mutex::new(RestoreStore::new());

        press(&store, 1, Err(()));

        assert!(lock_store(&store).get(1).is_some());
    }

    #[test]
    fn toggle_with_store_treats_windows_independently() {
        let store = std::sync::Mutex::new(RestoreStore::new());
        press(&store, 1, Ok(()));

        let actions = press(&store, 2, Ok(()));

        assert!(matches!(actions[..], [ToggleAction::MakeBorderless { .. }]));
        assert!(lock_store(&store).get(1).is_some());
        assert!(lock_store(&store).get(2).is_some());
    }

    #[test]
    fn toggle_with_store_strips_again_after_the_entry_is_forgotten() {
        let store = std::sync::Mutex::new(RestoreStore::new());
        press(&store, 1, Ok(()));
        lock_store(&store).forget(1);

        let actions = press(&store, 1, Ok(()));

        assert!(matches!(actions[..], [ToggleAction::MakeBorderless { .. }]));
    }

    #[test]
    fn record_if_absent_does_not_overwrite_the_original_state() {
        let mut store = RestoreStore::new();
        let original = RestoreState {
            style: BORDERED,
            ex_style: 0,
            region: ORIGINAL,
        };
        store.record_if_absent(1, original);

        store.record_if_absent(
            1,
            RestoreState {
                style: BORDERED | WS_MINIMIZEBOX,
                ex_style: 0,
                region: TARGET,
            },
        );

        assert_eq!(store.get(1), Some(original));
    }

    #[test]
    fn record_if_absent_ignores_a_window_that_is_already_borderless() {
        let mut store = RestoreStore::new();

        store.record_if_absent(
            1,
            RestoreState {
                style: strip_border_style(BORDERED),
                ex_style: 0,
                region: ORIGINAL,
            },
        );

        assert_eq!(store.get(1), None);
    }

    #[test]
    fn toggle_with_store_does_not_claim_a_restore_for_an_already_borderless_window() {
        let store = std::sync::Mutex::new(RestoreStore::new());
        let borderless = strip_border_style(BORDERED);

        let _ = toggle_with_store(&store, 1, borderless, 0, ORIGINAL, TARGET, |_| {
            Ok::<(), ()>(())
        });

        assert_eq!(lock_store(&store).get(1), None);
    }

    #[test]
    fn a_hotkey_restore_suppresses_auto_apply_until_the_window_is_stripped_again() {
        let store = std::sync::Mutex::new(RestoreStore::new());
        press(&store, 1, Ok(()));

        press(&store, 1, Ok(()));
        assert!(lock_store(&store).opted_out.contains(&1));

        press(&store, 1, Ok(()));
        assert!(!lock_store(&store).opted_out.contains(&1));
    }

    #[test]
    fn a_failed_restore_does_not_suppress_auto_apply() {
        let store = std::sync::Mutex::new(RestoreStore::new());
        press(&store, 1, Ok(()));

        press(&store, 1, Err(()));

        assert!(!lock_store(&store).opted_out.contains(&1));
    }

    #[test]
    fn forgetting_a_window_clears_its_suppression() {
        let store = std::sync::Mutex::new(RestoreStore::new());
        press(&store, 1, Ok(()));
        press(&store, 1, Ok(()));

        lock_store(&store).forget(1);

        assert!(!lock_store(&store).opted_out.contains(&1));
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

        let chosen =
            choose_candidate_window(&[(foreground, 10), (larger, 10_000)], Some(foreground));

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
        assert_eq!(
            TaskbarAutohideState::default(),
            TaskbarAutohideState::AlwaysVisible
        );
    }

    #[test]
    fn apply_autohide_state_sets_the_autohide_bit_when_no_bits_are_currently_set() {
        assert_eq!(
            apply_autohide_state(0, TaskbarAutohideState::AutoHidden),
            ABS_AUTOHIDE
        );
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
    fn apply_autohide_state_restoring_always_visible_does_not_add_an_always_on_top_bit_that_was_not_already_set()
     {
        let current_flags = ABS_AUTOHIDE;

        let new_flags = apply_autohide_state(current_flags, TaskbarAutohideState::AlwaysVisible);

        assert_eq!(new_flags, 0);
    }

    const MONITOR: (i32, i32, i32, i32) = (0, 0, 1920, 1080);
    const BAR: (i32, i32, i32, i32) = (0, 1040, 1920, 1080);

    #[test]
    fn judge_taskbar_hidden_reports_a_non_visible_taskbar_as_hidden() {
        assert_eq!(
            judge_taskbar_hidden(false, false, BAR, MONITOR, true),
            Some(TaskbarHiddenReason::NotVisible)
        );
    }

    #[test]
    fn judge_taskbar_hidden_reports_a_visible_on_screen_topmost_taskbar_as_not_hidden() {
        assert_eq!(judge_taskbar_hidden(false, true, BAR, MONITOR, true), None);
    }

    #[test]
    fn judge_taskbar_hidden_reports_a_visible_taskbar_off_its_monitor_as_hidden() {
        assert_eq!(
            judge_taskbar_hidden(false, true, (0, 1080, 1920, 1120), MONITOR, true),
            Some(TaskbarHiddenReason::OffMonitor)
        );
    }

    #[test]
    fn judge_taskbar_hidden_treats_exactly_half_on_the_monitor_as_on_screen() {
        // 40px tall bar, 20px of it inside the monitor.
        assert_eq!(
            judge_taskbar_hidden(false, true, (0, 1060, 1920, 1100), MONITOR, true),
            None
        );
    }

    #[test]
    fn judge_taskbar_hidden_treats_just_under_half_on_the_monitor_as_off_monitor() {
        assert_eq!(
            judge_taskbar_hidden(false, true, (0, 1061, 1920, 1101), MONITOR, true),
            Some(TaskbarHiddenReason::OffMonitor)
        );
    }

    #[test]
    fn judge_taskbar_hidden_reports_a_zero_area_rect_as_off_monitor() {
        assert_eq!(
            judge_taskbar_hidden(false, true, (0, 1040, 0, 1040), MONITOR, true),
            Some(TaskbarHiddenReason::OffMonitor)
        );
    }

    #[test]
    fn judge_taskbar_hidden_reports_a_visible_on_screen_but_covered_taskbar_as_hidden() {
        assert_eq!(
            judge_taskbar_hidden(false, true, BAR, MONITOR, false),
            Some(TaskbarHiddenReason::Covered)
        );
    }

    #[test]
    fn judge_taskbar_hidden_with_windows_auto_hide_ignores_a_slid_off_taskbar() {
        // Slid off the bottom edge and not under its own center: Windows'
        // auto-hide resting state, so the hotkey must be able to hide it.
        assert_eq!(
            judge_taskbar_hidden(true, true, (0, 1078, 1920, 1118), MONITOR, false),
            None
        );
    }

    #[test]
    fn judge_taskbar_hidden_with_windows_auto_hide_still_reports_not_visible() {
        assert_eq!(
            judge_taskbar_hidden(true, false, BAR, MONITOR, true),
            Some(TaskbarHiddenReason::NotVisible)
        );
    }

    #[test]
    fn resend_appbar_state_sends_exactly_the_flags_it_read_with_no_bit_flipped() {
        for flags in [
            0,
            ABS_AUTOHIDE,
            ABS_ALWAYSONTOP,
            ABS_AUTOHIDE | ABS_ALWAYSONTOP,
        ] {
            let mut sent = None;
            resend_appbar_state(|| flags, |lparam| sent = Some(lparam));
            assert_eq!(sent, Some(flags as isize));
        }
    }

    #[test]
    fn should_rehide_on_join_for_a_fully_hidden_session_whenever_the_taskbar_is_still_ws_visible() {
        let session = Some(TaskbarHideRestore::FullyHidden);
        assert!(should_rehide_on_join(session, None));
        assert!(should_rehide_on_join(
            session,
            Some(TaskbarHiddenReason::Covered)
        ));
        assert!(should_rehide_on_join(
            session,
            Some(TaskbarHiddenReason::OffMonitor)
        ));
        assert!(!should_rehide_on_join(
            session,
            Some(TaskbarHiddenReason::NotVisible)
        ));
    }

    #[test]
    fn should_rehide_on_join_never_for_a_hover_reveal_session_whatever_the_verdict() {
        // The session was started in HoverReveal; a configured mode that has
        // since changed to FullyHidden must not make the join SW_HIDE.
        let session = Some(TaskbarHideRestore::HoverReveal(
            TaskbarAutohideState::AlwaysVisible,
        ));
        for verdict in [
            None,
            Some(TaskbarHiddenReason::Covered),
            Some(TaskbarHiddenReason::OffMonitor),
            Some(TaskbarHiddenReason::NotVisible),
        ] {
            assert!(!should_rehide_on_join(session, verdict));
        }
    }

    #[test]
    fn should_rehide_on_join_never_without_a_session() {
        assert!(!should_rehide_on_join(None, None));
    }

    #[test]
    fn plan_taskbar_hide_in_fully_hidden_mode_always_remembers_fully_hidden_regardless_of_current_appbar_state()
     {
        assert_eq!(
            plan_taskbar_hide(
                TaskbarHideMode::FullyHidden,
                TaskbarAutohideState::AlwaysVisible
            ),
            TaskbarHideRestore::FullyHidden
        );
        assert_eq!(
            plan_taskbar_hide(
                TaskbarHideMode::FullyHidden,
                TaskbarAutohideState::AutoHidden
            ),
            TaskbarHideRestore::FullyHidden
        );
    }

    #[test]
    fn plan_taskbar_hide_in_hover_reveal_mode_remembers_whatever_appbar_state_was_current() {
        assert_eq!(
            plan_taskbar_hide(
                TaskbarHideMode::HoverReveal,
                TaskbarAutohideState::AlwaysVisible
            ),
            TaskbarHideRestore::HoverReveal(TaskbarAutohideState::AlwaysVisible)
        );
        assert_eq!(
            plan_taskbar_hide(
                TaskbarHideMode::HoverReveal,
                TaskbarAutohideState::AutoHidden
            ),
            TaskbarHideRestore::HoverReveal(TaskbarAutohideState::AutoHidden)
        );
    }

    #[test]
    fn taskbar_autohide_state_from_bool_true_maps_to_auto_hidden() {
        assert_eq!(
            taskbar_autohide_state_from_bool(true),
            TaskbarAutohideState::AutoHidden
        );
    }

    #[test]
    fn taskbar_autohide_state_from_bool_false_maps_to_always_visible() {
        assert_eq!(
            taskbar_autohide_state_from_bool(false),
            TaskbarAutohideState::AlwaysVisible
        );
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
    fn win32_error_implements_display_and_std_error() {
        fn assert_is_std_error<E: std::error::Error>(_: &E) {}

        let os_err = Win32Error::Os(std::io::Error::from_raw_os_error(5));

        assert_is_std_error(&os_err);
        assert!(
            os_err
                .to_string()
                .starts_with("Win32 window manipulation failed: ")
        );
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
        assert_eq!(
            err.to_string(),
            "could not find the taskbar window (Shell_TrayWnd)"
        );
    }
}
