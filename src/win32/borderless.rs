//! The two placement calls: strip a window into the Zone, and restore it.

use super::{
    PLACED_WINDOWS, RestoreState, Win32Error, borderless_style, disable_rounded_corners,
    get_restorable_region, get_window_ex_style, get_window_region, get_window_style, is_maximized,
    is_window_hung_logged, reset_corner_preference, restore_maximized_window, restore_with_store,
    set_window_ex_style, set_window_region, set_window_style, strip_border_ex_style,
    unmark_applied_window, window_has_drifted, with_placed,
};
use crate::region_calc::Region;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::IsIconic;

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

/// Strips a window's border and moves/resizes it to `target_region`: the one
/// placement call behind [`crate::placement::apply_zone`], for all five of its
/// triggers (the hotkey and auto-apply act on the foreground window; the tray
/// picker and live re-apply on an explicit window, which may not be the
/// foreground window at all). Reads `hwnd`'s current style itself, so callers
/// only need a target `HWND` and region.
///
/// After the first mutation that took effect (the un-maximize for a maximized
/// window, otherwise the style change), records the window's pre-strip
/// style/region as its [`RestoreState`] unless it already has one, so the
/// borderless hotkey can later restore a window placed by any path (tray
/// picker, auto-apply, drift re-apply) to its original look. A window that
/// was already borderless has no border to restore and gets no entry (see
/// [`super::placed::RestoreStore::record_if_absent`]).
///
/// Minimized-window guard: a minimized window's `GetWindowRect` reports its off-screen
/// minimized position (roughly `(-32000, -32000)`), not a real, restorable
/// region, so applying a target region to it would move/resize it based on
/// that bogus rect. The eligibility filter
/// ([`crate::window_filter::eligibility_for_hwnd`]) has no minimized check
/// (`IsWindowVisible` stays `TRUE` for minimized windows), so a minimized
/// window can legitimately reach this function.
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
    // The in-place decision comes first: a game that re-maximizes itself keeps
    // `WS_MAXIMIZE` (not drift) while sitting exactly in the Zone, and
    // un-maximizing it on every focus switch would snap it to its normal rect
    // and back, making the game resize its swapchain twice.
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
    // A maximized window is un-maximized first (synchronously, behind the hung
    // check) so the Zone rect is not overridden by the maximized state. Its
    // normal rect and both styles are read before the restore, so a failed
    // read aborts before the window is touched.
    let maximized = is_maximized(hwnd);
    let pre_read = if maximized {
        let region = get_restorable_region(hwnd, true)?;
        let style = get_window_style(hwnd)?;
        let ex_style = get_window_ex_style(hwnd)?;
        restore_maximized_window(hwnd);
        // The un-maximize is the first mutation: record the original now, so
        // a later failure (or a SkipInPlace) cannot strand the window
        // un-maximized with no way back. Only when it really took effect, so
        // a window GameFrame cannot change (e.g. elevated) gets no entry.
        if !is_maximized(hwnd) {
            with_placed(|p| {
                p.restore
                    .record_if_absent(hwnd as isize, RestoreState::new(style, ex_style, region));
            });
        }
        Some((style, ex_style, region))
    } else {
        None
    };

    let (live_style, live_ex_style, live_region) = match pre_read {
        Some(read) => read,
        None => (
            get_window_style(hwnd)?,
            get_window_ex_style(hwnd)?,
            get_window_region(hwnd)?,
        ),
    };
    set_window_style(hwnd, borderless_style(live_style))?;
    // Recorded once the style change succeeded (a no-op for a maximized
    // window, already recorded at its un-maximize), so a window GameFrame
    // could not change (e.g. an elevated game) never gets a restore entry; the
    // remaining calls below can still fail with the original already safe.
    with_placed(|p| {
        p.restore.record_if_absent(
            hwnd as isize,
            RestoreState::new(live_style, live_ex_style, live_region),
        );
    });
    set_window_ex_style(hwnd, strip_border_ex_style(live_ex_style))?;
    set_window_region(hwnd, target_region)?;
    disable_rounded_corners(hwnd)?;
    Ok(())
}

/// Restores `hwnd` to exactly the style/region remembered from its first strip
/// (the borderless hotkey's restore direction); a no-op for a window with no
/// remembered state. The state is kept per window, so other windows never
/// affect the press; see [`restore_with_store`].
///
/// OS-facing: acts on the real window. Not exercised by this crate's
/// automated tests -- see module docs.
pub fn restore_window(hwnd: HWND) -> Result<(), Win32Error> {
    // A minimized window's `GetWindowRect` reports its off-screen minimized
    // position, which would be written back as if it were real.
    //
    // Safety: `hwnd` is a live handle; `IsIconic` performs no writes.
    if unsafe { IsIconic(hwnd) } != 0 {
        return Err(Win32Error::WindowMinimized);
    }
    if is_window_hung_logged(hwnd) {
        return Err(Win32Error::WindowHung);
    }
    restore_with_store(&PLACED_WINDOWS, hwnd as isize, |restore| {
        // Re-maximized since the strip: un-maximize through the OS (hung
        // check already passed) rather than overwriting the style and
        // clearing WS_MAXIMIZE by hand.
        if is_maximized(hwnd) {
            restore_maximized_window(hwnd);
        }
        set_window_style(hwnd, restore.style)?;
        set_window_ex_style(hwnd, restore.ex_style)?;
        set_window_region(hwnd, restore.region)?;
        reset_corner_preference(hwnd)?;
        // A restored window is no longer GameFrame-placed: stop the drift
        // watcher from re-stripping it.
        unmark_applied_window(hwnd);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_is_skipped_for_a_window_already_in_place() {
        assert_eq!(decide_apply(false, false), ApplyDecision::SkipInPlace);
    }

    #[test]
    fn apply_goes_ahead_for_a_window_that_drifted() {
        assert_eq!(decide_apply(false, true), ApplyDecision::Apply);
    }

    // Regression: a game that re-maximizes itself keeps `WS_MAXIMIZE`
    // while sitting exactly in the Zone. Applying on every focus switch
    // un-maximized it (snapping it to its normal rect) before the in-place
    // check ran, so the game visibly shrank and snapped back. The end state
    // looks the same, so the test counts the window's own `WM_WINDOWPOSCHANGED`
    // messages, which the un-maximize delivers synchronously.
    #[test]
    fn apply_leaves_a_maximized_window_already_in_the_zone_untouched() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, SW_SHOWMAXIMIZED,
            SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos, ShowWindow, UnregisterClassW, WNDCLASSW,
            WM_WINDOWPOSCHANGED, WS_POPUP, WS_VISIBLE,
        };

        static MOVES: AtomicU32 = AtomicU32::new(0);
        unsafe extern "system" fn proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            if m == WM_WINDOWPOSCHANGED {
                MOVES.fetch_add(1, Ordering::SeqCst);
            }
            // Safety: forwards the message unchanged.
            unsafe { DefWindowProcW(h, m, w, l) }
        }

        let class: Vec<u16> = "GameFrameTestMaximizedInZone\0".encode_utf16().collect();
        // Safety: all-zero is a valid WNDCLASSW; the fields set below are valid.
        let hwnd = unsafe {
            let mut wc: WNDCLASSW = std::mem::zeroed();
            wc.lpfnWndProc = Some(proc);
            wc.lpszClassName = class.as_ptr();
            assert_ne!(RegisterClassW(&wc), 0, "class registration");
            CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                WS_POPUP | WS_VISIBLE,
                100,
                100,
                400,
                300,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert!(!hwnd.is_null(), "test window must be created");
        let zone = Region {
            x: 50,
            y: 60,
            width: 640,
            height: 480,
        };
        // Safety: `hwnd` is the live window created above.
        unsafe {
            ShowWindow(hwnd, SW_SHOWMAXIMIZED);
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                zone.x,
                zone.y,
                zone.width,
                zone.height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        let style = get_window_style(hwnd).unwrap();
        set_window_style(hwnd, borderless_style(style)).unwrap();
        assert!(is_maximized(hwnd), "precondition: still flagged maximized");
        assert_eq!(get_window_region(hwnd).unwrap(), zone, "precondition: in the Zone");
        let before = MOVES.load(Ordering::SeqCst);

        let result = apply_borderless_to_window(hwnd, zone);

        let moves = MOVES.load(Ordering::SeqCst) - before;
        // Safety: the window and class created above.
        unsafe {
            DestroyWindow(hwnd);
            UnregisterClassW(class.as_ptr(), std::ptr::null_mut());
        }
        assert!(result.is_ok(), "apply failed: {result:?}");
        assert_eq!(moves, 0, "an in-place window must not be moved or un-maximized");
    }
    #[test]
    fn apply_is_skipped_for_a_hung_window_whatever_its_state() {
        assert_eq!(decide_apply(true, true), ApplyDecision::SkipHung);
        assert_eq!(decide_apply(true, false), ApplyDecision::SkipHung);
    }
}
