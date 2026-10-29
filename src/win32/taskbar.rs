//! The OS primitives of the independent taskbar-hide mechanic.

use super::{Win32Error, is_window_hung_logged};
use crate::profile_store::TaskbarHideMode;
use crate::wide::encode_wide;
use crate::window_filter;
use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows_sys::Win32::UI::Shell::{
    ABM_GETSTATE, ABM_SETSTATE, ABS_AUTOHIDE, APPBARDATA, SHAppBarMessage,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GA_ROOT, GetAncestor, GetForegroundWindow, GetWindowRect, HWND_TOP,
    IsHungAppWindow, IsWindowVisible, SW_HIDE, SW_SHOW, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SetWindowPos, ShowWindow, ShowWindowAsync, WindowFromPoint,
};

/// The current on/off state of the taskbar auto-hide toggle -- fully
/// independent of [`super::RestoreState`]/the borderless toggle's state (see this
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
    // SAFETY: all-zero bytes are a valid value of this plain C struct.
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
    // SAFETY: all-zero bytes are a valid value of this plain C struct.
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
    // SAFETY: all-zero bytes are a valid value of this plain C struct.
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

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::Shell::ABS_ALWAYSONTOP;

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
}
