//! Window manipulation for the borderless-toggle mechanic: stripping/restoring
//! a window's border/title bar, resizing/repositioning it, and disabling
//! Windows 11's rounded-corner rendering while it's borderless.
//!
//! Split the same way as [`crate::settings_ui`]:
//! - Pure decision logic ([`strip_border_style`], [`plan_toggle`], [`active_preset`],
//!   [`Win32Error::is_access_denied`], [`Win32Error::from_hresult`]) has no
//!   `HWND`/OS dependency and is fully unit tested below.
//! - The actual Win32 calls further down (`GetForegroundWindow`, `IsIconic`,
//!   `SetWindowLongPtrW`, `SetWindowPos`, `DwmSetWindowAttribute`,
//!   `ShellExecuteW`) are OS-facing -- they need a real foreground window to
//!   manipulate, which a headless/CI sandbox cannot provide -- so they are NOT
//!   exercised by this crate's automated tests. See the module's doc comment
//!   in the issue writeup for what could/couldn't be verified.

use crate::region_calc::{Preset, Region};

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

/// Resolves "the active preset" for the borderless-toggle hotkey: the first
/// entry in the config's preset list.
///
/// MVP scope decision (see issue #6): there is no per-hotkey preset-selection
/// UI yet, so the first preset in the list is treated as "the" active preset
/// for this hotkey. Returns `None` if the list is empty.
pub fn active_preset(presets: &[Preset]) -> Option<&Preset> {
    presets.first()
}

use windows_sys::Win32::UI::WindowsAndMessaging::{
    WS_CAPTION, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME,
};

/// Reasons a live Win32 window-manipulation call can fail.
#[derive(Debug)]
pub enum Win32Error {
    /// `GetForegroundWindow` returned a null handle (no foreground window).
    NoForegroundWindow,
    /// The foreground window is minimized (`IsIconic`), so its bounding
    /// rectangle would be its off-screen minimized position (roughly
    /// `(-32000, -32000)`) rather than a real, restorable region.
    WindowMinimized,
    /// A Win32 call failed; wraps the OS error from `GetLastError`.
    Os(std::io::Error),
    /// An `HRESULT` failure that isn't a `FACILITY_WIN32` code (so it can't
    /// be safely reinterpreted as a plain Win32/`GetLastError`-style error
    /// code) -- see [`Win32Error::from_hresult`].
    Hresult(i32),
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
            Win32Error::WindowMinimized => write!(f, "foreground window is minimized"),
            Win32Error::Os(e) => write!(f, "Win32 window manipulation failed: {e}"),
            Win32Error::Hresult(hr) => write!(f, "Win32 window manipulation failed: HRESULT {:#010x}", *hr as u32),
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
        }
    }
}

use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DEFAULT, DWMWCP_DONOTROUND,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, IsIconic, SetWindowLongPtrW, SetWindowPos, GWL_STYLE,
    SET_WINDOW_POS_FLAGS, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOWNORMAL,
};

/// Reads a window's current `GWL_STYLE` value.
///
/// Per `GetWindowLongPtrW`'s documented contract, a return value of zero is
/// ambiguous (it can be either a genuine style value of `0` or a failure),
/// so -- mirroring [`set_window_style`] below -- `SetLastError(0)` is called
/// first and a non-zero `GetLastError()` afterwards is the real failure
/// signal.
fn get_window_style(hwnd: HWND) -> Result<u32, Win32Error> {
    // Safety: `hwnd` is a live handle obtained from `GetForegroundWindow`
    // just before this call; `GetWindowLongPtrW` performs no writes and
    // takes no pointers besides `hwnd` itself.
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
        ToggleAction::MakeBorderless { new_style, region } => {
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
            *state = next_state;
            set_window_style(hwnd, new_style)?;
            set_window_region(hwnd, region)?;
            disable_rounded_corners(hwnd)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::region_calc::Region;

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
    fn active_preset_returns_the_first_preset_in_the_list() {
        let presets = vec![Preset::RightHalf, Preset::LeftHalf, Preset::FullScreen];

        assert_eq!(active_preset(&presets), Some(&Preset::RightHalf));
    }

    #[test]
    fn active_preset_of_an_empty_list_is_none() {
        let presets: Vec<Preset> = vec![];

        assert_eq!(active_preset(&presets), None);
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
        assert_eq!(err.to_string(), "foreground window is minimized");
    }
}
