//! Border/title-bar style bits, window bounds and the DWM corner preference:
//! the primitives the borderless toggle is built from.

use super::Win32Error;
use crate::region_calc::Region;
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DEFAULT, DWMWCP_DONOTROUND, DwmSetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GWL_STYLE, GetWindowLongPtrW, GetWindowPlacement, GetWindowRect, IsZoomed,
    SET_WINDOW_POS_FLAGS, SW_RESTORE, SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos, ShowWindow, WINDOW_LONG_PTR_INDEX,
    WINDOWPLACEMENT, WS_CAPTION, WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME, WS_EX_TOOLWINDOW,
    WS_EX_WINDOWEDGE, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME,
};

/// The style bits removed to strip a window's border for the borderless
/// toggle: `WS_CAPTION` is the title bar itself, `WS_THICKFRAME` the
/// resizable sizing border, and `WS_MINIMIZEBOX`/`WS_MAXIMIZEBOX`/`WS_SYSMENU`
/// the title-bar buttons/system menu that have no purpose once the title bar
/// is gone.
/// `WS_MAXIMIZE` is deliberately *not* in the mask: it is window state owned by
/// `SW_RESTORE` ([`restore_maximized_window`]), and clearing it by hand on a
/// still-maximized window would leave the OS state inconsistent.
const BORDERLESS_STYLE_MASK: u32 =
    WS_CAPTION | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX | WS_SYSMENU;

/// The extended-style edge bits removed to strip a window: the sunken client
/// edge, the raised window edge and the dialog frame all draw a border that
/// survives removing `WS_CAPTION`/`WS_THICKFRAME`.
const BORDERLESS_EX_STYLE_MASK: u32 = WS_EX_CLIENTEDGE | WS_EX_WINDOWEDGE | WS_EX_DLGMODALFRAME;

/// Removes the border/title-bar style bits from a raw `GWL_STYLE` value,
/// leaving every other bit (e.g. `WS_VISIBLE`, `WS_POPUP`, `WS_CHILD`,
/// `WS_MAXIMIZE`) untouched. Pure bit manipulation -- no `HWND`
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

/// Reads a window's current `GWL_STYLE` value.
///
/// Per `GetWindowLongPtrW`'s documented contract, a return value of zero is
/// ambiguous (it can be either a genuine style value of `0` or a failure),
/// so -- mirroring [`set_window_style`] below -- `SetLastError(0)` is called
/// first and a non-zero `GetLastError()` afterwards is the real failure
/// signal.
pub(super) fn get_window_style(hwnd: HWND) -> Result<u32, Win32Error> {
    get_window_long(hwnd, GWL_STYLE)
}

/// Reads a window's current `GWL_EXSTYLE` value (see [`get_window_style`]).
pub(super) fn get_window_ex_style(hwnd: HWND) -> Result<u32, Win32Error> {
    get_window_long(hwnd, GWL_EXSTYLE)
}

/// Reads a window long (`GWL_STYLE` or `GWL_EXSTYLE`) with the
/// `SetLastError(0)` / zero-return protocol described on [`get_window_style`].
/// The `isize` result is truncated to `u32`; all style bits live in the low 32.
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
pub(super) fn set_window_style(hwnd: HWND, style: u32) -> Result<(), Win32Error> {
    set_window_long(hwnd, GWL_STYLE, style)
}

/// Writes a new `GWL_EXSTYLE` value to a window (see [`set_window_style`]).
pub(super) fn set_window_ex_style(hwnd: HWND, ex_style: u32) -> Result<(), Win32Error> {
    set_window_long(hwnd, GWL_EXSTYLE, ex_style)
}

/// Writes a window long (`GWL_STYLE` or `GWL_EXSTYLE`) with the
/// `SetLastError(0)` / zero-return protocol described on [`set_window_style`].
fn set_window_long(hwnd: HWND, index: WINDOW_LONG_PTR_INDEX, value: u32) -> Result<(), Win32Error> {
    // Safety: `hwnd` is a live handle; `SetWindowLongPtrW` performs no reads
    // besides `hwnd`/`value` and takes no pointers.
    unsafe {
        windows_sys::Win32::Foundation::SetLastError(0);
        let previous_style = SetWindowLongPtrW(hwnd, index, value as isize);
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
pub(super) fn get_window_region(hwnd: HWND) -> Result<Region, Win32Error> {
    // SAFETY: all-zero bytes are a valid value of this plain C struct.
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
pub(super) fn get_restorable_region(hwnd: HWND, maximized: bool) -> Result<Region, Win32Error> {
    if !maximized {
        return get_window_region(hwnd);
    }
    // SAFETY: all-zero bytes are a valid value of this plain C struct.
    let mut placement: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
    placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
    // Safety: `hwnd` is a live handle; `placement` is a valid out-pointer
    // with `length` set, as `GetWindowPlacement` requires.
    if unsafe { GetWindowPlacement(hwnd, &mut placement) } == 0 {
        // No fallback to the full-screen rect: remembering it would restore
        // the window full-screen without its maximize state.
        log!("GetWindowPlacement failed for maximized window {hwnd:?}; aborting");
        return Err(Win32Error::Os(std::io::Error::last_os_error()));
    }
    let rect = placement.rcNormalPosition;
    let normal = Region {
        x: rect.left,
        y: rect.top,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
    };
    // `rcNormalPosition` is in workspace coordinates for tool windows.
    if get_window_ex_style(hwnd)? & WS_EX_TOOLWINDOW != 0 {
        let origin = work_area_origin(hwnd)?;
        return Ok(workspace_to_screen(normal, origin));
    }
    Ok(normal)
}

/// Pure: converts a rect in workspace coordinates (relative to the top-left of
/// the monitor's work area, as `rcNormalPosition` is for `WS_EX_TOOLWINDOW`
/// windows) to screen coordinates, given the work area's screen origin.
fn workspace_to_screen(region: Region, work_origin: (i32, i32)) -> Region {
    Region {
        x: region.x + work_origin.0,
        y: region.y + work_origin.1,
        ..region
    }
}

/// The screen position of the top-left of the work area of the monitor `hwnd`
/// is on.
fn work_area_origin(hwnd: HWND) -> Result<(i32, i32), Win32Error> {
    // SAFETY: all-zero bytes are a valid value of this plain C struct.
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    // Safety: `hwnd` is a live handle; `info` is a valid out-pointer with
    // `cbSize` set, as `GetMonitorInfoW` requires.
    let ok = unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        GetMonitorInfoW(monitor, &mut info)
    };
    if ok == 0 {
        return Err(Win32Error::Os(std::io::Error::last_os_error()));
    }
    Ok((info.rcWork.left, info.rcWork.top))
}

/// Un-maximizes `hwnd` synchronously, so the style/rect writes that follow
/// never race a still-queued restore. The caller has already passed the hung
/// check (`is_window_hung_logged`), so this cannot block on a hung window.
pub(super) fn restore_maximized_window(hwnd: HWND) {
    // Safety: `hwnd` is a live handle; `ShowWindow` takes plain integers.
    unsafe {
        ShowWindow(hwnd, SW_RESTORE);
    }
}

/// Whether `hwnd` is currently maximized.
pub(super) fn is_maximized(hwnd: HWND) -> bool {
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
pub(super) fn set_window_region(hwnd: HWND, region: Region) -> Result<(), Win32Error> {
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
pub(super) fn disable_rounded_corners(hwnd: HWND) -> Result<(), Win32Error> {
    set_corner_preference(hwnd, DWMWCP_DONOTROUND)
}

/// Resets `DWMWA_WINDOW_CORNER_PREFERENCE` back to `DWMWCP_DEFAULT` when a
/// window is restored to its normal bordered state -- undoes
/// [`disable_rounded_corners`] so a toggle-twice cycle doesn't permanently
/// leave the window's corners square.
pub(super) fn reset_corner_preference(hwnd: HWND) -> Result<(), Win32Error> {
    set_corner_preference(hwnd, DWMWCP_DEFAULT)
}

/// Pure decision: has a window that was previously made borderless at
/// `target` drifted from it? True if its bounds differ from `target`, or if
/// any border/title-bar style bit (see [`strip_border_style`]) came back.
/// Used by the in-game-resize watcher so it only re-applies when the game
/// actually changed the window -- which also keeps GameFrame's own
/// `SetWindowPos` (which fires another location-change event) from looping.
///
/// `WS_MAXIMIZE` and the leftover extended-style edges
/// ([`strip_border_ex_style`]) are deliberately not drift: a game that
/// maximizes itself or re-adds an edge would otherwise be re-applied on every
/// location event (a maximized window already differs by bounds). They are
/// still handled whenever an apply does run.
pub fn window_drifted(current_region: Region, current_style: u32, target: Region) -> bool {
    current_region != target || strip_border_style(current_style) != current_style
}

/// Reads `hwnd`'s live bounds/style and reports [`window_drifted`] against
/// `target`. OS-facing.
pub fn window_has_drifted(hwnd: HWND, target: Region) -> Result<bool, Win32Error> {
    Ok(window_drifted(
        get_window_region(hwnd)?,
        get_window_style(hwnd)?,
        target,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::WS_MAXIMIZE;

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

        assert!(window_drifted(drifted, 0x14CF0000, target));
    }

    #[test]
    fn window_drifted_detects_a_restored_caption_even_when_bounds_match() {
        let target = Region {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };

        assert!(window_drifted(target, 0x14CF0000, target));
    }

    #[test]
    fn window_drifted_is_false_for_the_state_gameframe_itself_applied() {
        let target = Region {
            x: 1660,
            y: 0,
            width: 3460,
            height: 1440,
        };

        assert!(!window_drifted(target, 0x14000000, target));
    }

    #[test]
    fn strip_border_ex_style_removes_the_leftover_edge_bits() {
        const WS_EX_TOPMOST: u32 = 0x8;
        let ex = WS_EX_CLIENTEDGE | WS_EX_WINDOWEDGE | WS_EX_DLGMODALFRAME | WS_EX_TOPMOST;

        assert_eq!(strip_border_ex_style(ex), WS_EX_TOPMOST);
    }

    #[test]
    fn strip_border_style_leaves_the_maximized_state_to_sw_restore() {
        const WS_VISIBLE: u32 = 0x10000000;

        assert_eq!(
            strip_border_style(WS_MAXIMIZE | WS_CAPTION | WS_VISIBLE),
            WS_MAXIMIZE | WS_VISIBLE
        );
    }

    #[test]
    fn window_drifted_ignores_a_maximized_flag_when_bounds_match() {
        let target = Region {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };

        assert!(!window_drifted(target, 0x14000000 | WS_MAXIMIZE, target));
    }

    #[test]
    fn a_tool_window_rect_is_moved_from_workspace_to_screen_coordinates() {
        let rect = Region {
            x: 10,
            y: 20,
            width: 300,
            height: 200,
        };

        let screen = workspace_to_screen(rect, (1920, 40));

        assert_eq!(
            screen,
            Region {
                x: 1930,
                y: 60,
                width: 300,
                height: 200,
            }
        );
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
}
