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
/// OS-facing: acts on the real window identified by `hwnd`. The maximized-window
/// paths are covered by tests that run it on real windows.
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
        // The un-maximize is the first mutation: record the original now, so a
        // later failure cannot strand the window un-maximized with no way back.
        // Only when it really took effect, so a window GameFrame cannot change
        // (e.g. elevated) gets no entry.
        if !is_maximized(hwnd) {
            with_placed(|p| {
                p.restore
                    .record_if_absent(hwnd as isize, RestoreState::new(style, ex_style, region));
            });
        }
        // Write back the style as it is after the restore, so a window
        // GameFrame un-maximized is not re-flagged `WS_MAXIMIZE`. Re-read
        // rather than clear the bit by hand, so a restore that did not take
        // effect keeps its real state. The recorded original above stays the
        // pre-restore read.
        let style_now = get_window_style(hwnd)?;
        Some((style_now, ex_style, region))
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
/// automated tests.
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
    use windows_sys::Win32::UI::WindowsAndMessaging::WS_MAXIMIZE;

    #[test]
    fn apply_is_skipped_for_a_window_already_in_place() {
        assert_eq!(decide_apply(false, false), ApplyDecision::SkipInPlace);
    }

    #[test]
    fn apply_goes_ahead_for_a_window_that_drifted() {
        assert_eq!(decide_apply(false, true), ApplyDecision::Apply);
    }

    use std::cell::Cell;

    thread_local! {
        // Window messages reach the proc synchronously on the creating thread,
        // so a per-thread counter keeps parallel tests apart.
        static MOVES: Cell<u32> = const { Cell::new(0) };
    }

    unsafe extern "system" fn counting_proc(
        h: HWND,
        m: u32,
        w: windows_sys::Win32::Foundation::WPARAM,
        l: windows_sys::Win32::Foundation::LPARAM,
    ) -> windows_sys::Win32::Foundation::LRESULT {
        use windows_sys::Win32::UI::WindowsAndMessaging::{DefWindowProcW, WM_WINDOWPOSCHANGED};
        if m == WM_WINDOWPOSCHANGED {
            MOVES.with(|c| c.set(c.get() + 1));
        }
        // Safety: forwards the message unchanged.
        unsafe { DefWindowProcW(h, m, w, l) }
    }

    /// Whether this process runs on an interactive (visible) window station.
    /// Real top-level windows only behave as the tests expect there; a service
    /// or other non-interactive window station gets `false`. A failed query
    /// counts as not interactive. `windows-latest` CI has an interactive
    /// desktop, so the real-window tests run there. A skipped test reports as
    /// passed; run with `--nocapture` to see the skip line.
    fn has_interactive_desktop() -> bool {
        use windows_sys::Win32::System::StationsAndDesktops::{
            GetProcessWindowStation, GetUserObjectInformationW, UOI_FLAGS, USEROBJECTFLAGS,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::WSF_VISIBLE;
        // Safety: the station handle is owned by the process (not closed); the
        // buffer is a USEROBJECTFLAGS and its size is passed.
        unsafe {
            let station = GetProcessWindowStation();
            if station.is_null() {
                return false;
            }
            let mut flags = USEROBJECTFLAGS::default();
            let mut _needed = 0u32;
            let ok = GetUserObjectInformationW(
                station,
                UOI_FLAGS,
                (&mut flags as *mut USEROBJECTFLAGS).cast(),
                std::mem::size_of::<USEROBJECTFLAGS>() as u32,
                &mut _needed,
            );
            ok != 0 && flags.dwFlags & WSF_VISIBLE as u32 != 0
        }
    }

    /// A real popup window, destroyed and its class unregistered on drop so a
    /// failed assertion in a test cannot leak either.
    ///
    /// These tests create, show and maximize real top-level windows, so they
    /// need an interactive desktop (CI provides one on `windows-latest`). The
    /// constructors return `None`, after printing why, when there is none;
    /// tests then return early: `let Some(w) = ... else { return };`.
    struct TestWindow {
        hwnd: HWND,
        class: Vec<u16>,
    }

    /// Where `TestWindow` is created, before it is maximized: its normal rect.
    const CREATED: Region = Region {
        x: 100,
        y: 100,
        width: 400,
        height: 300,
    };

    #[derive(Clone, Copy, PartialEq)]
    enum Frame {
        Borderless,
        Bordered,
    }

    impl TestWindow {
        /// A borderless popup flagged `WS_MAXIMIZE`, moved to `placed` if given.
        fn maximized(class_name: &str, placed: Option<Region>) -> Option<Self> {
            Self::create(class_name, placed, Frame::Borderless)
        }

        /// A maximized window that keeps its normal border (caption and frame).
        fn bordered_maximized(class_name: &str) -> Option<Self> {
            Self::create(class_name, None, Frame::Bordered)
        }

        /// `None` (with one line on stderr) when there is no interactive desktop.
        fn create(class_name: &str, placed: Option<Region>, frame: Frame) -> Option<Self> {
            if !has_interactive_desktop() {
                eprintln!(
                    "skipping {class_name}: no interactive desktop (window station not visible)"
                );
                return None;
            }
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                CreateWindowExW, RegisterClassW, SW_SHOWMAXIMIZED, SWP_NOACTIVATE, SWP_NOZORDER,
                SetWindowPos, ShowWindow, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
            };
            let base = match frame {
                Frame::Bordered => WS_OVERLAPPEDWINDOW,
                Frame::Borderless => WS_POPUP,
            };
            let class: Vec<u16> = format!("{class_name}\0").encode_utf16().collect();
            // Safety: all-zero is a valid WNDCLASSW; the fields set are valid.
            let hwnd = unsafe {
                let mut wc: WNDCLASSW = std::mem::zeroed();
                wc.lpfnWndProc = Some(counting_proc);
                wc.lpszClassName = class.as_ptr();
                assert_ne!(RegisterClassW(&wc), 0, "class registration");
                CreateWindowExW(
                    0,
                    class.as_ptr(),
                    class.as_ptr(),
                    base | WS_VISIBLE,
                    CREATED.x,
                    CREATED.y,
                    CREATED.width,
                    CREATED.height,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            };
            // Built before the assert so a null handle still unregisters the class.
            let window = TestWindow { hwnd, class };
            assert!(!hwnd.is_null(), "test window must be created");
            // Safety: `hwnd` is the live window created above.
            unsafe {
                ShowWindow(hwnd, SW_SHOWMAXIMIZED);
                if let Some(r) = placed {
                    SetWindowPos(
                        hwnd,
                        std::ptr::null_mut(),
                        r.x,
                        r.y,
                        r.width,
                        r.height,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
            }
            if frame == Frame::Borderless {
                let style = get_window_style(hwnd).unwrap();
                set_window_style(hwnd, borderless_style(style)).unwrap();
            }
            Some(window)
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyWindow, UnregisterClassW};
            // The store entry apply may have recorded, so a test never leaks it.
            super::super::forget_window(self.hwnd);
            // Safety: the window and class created in `create`.
            unsafe {
                if !self.hwnd.is_null() {
                    DestroyWindow(self.hwnd);
                }
                UnregisterClassW(self.class.as_ptr(), std::ptr::null_mut());
            }
        }
    }

    const ZONE: Region = Region {
        x: 50,
        y: 60,
        width: 640,
        height: 480,
    };

    // Regression: a game that re-maximizes itself keeps `WS_MAXIMIZE`
    // while sitting exactly in the Zone. Applying on every focus switch
    // un-maximized it (snapping it to its normal rect) before the in-place
    // check ran, so the game visibly shrank and snapped back. The end state
    // looks the same, so the test counts the window's own `WM_WINDOWPOSCHANGED`
    // messages, which the un-maximize delivers synchronously.
    #[test]
    fn apply_leaves_a_maximized_window_already_in_the_zone_untouched() {
        let Some(w) = TestWindow::maximized("GameFrameTestMaximizedInZone", Some(ZONE)) else {
            return;
        };
        assert!(
            is_maximized(w.hwnd),
            "precondition: still flagged maximized"
        );
        assert_eq!(
            get_window_region(w.hwnd).unwrap(),
            ZONE,
            "precondition: in the Zone"
        );
        let before = MOVES.with(Cell::get);

        let result = apply_borderless_to_window(w.hwnd, ZONE);

        let moves = MOVES.with(Cell::get) - before;
        assert!(result.is_ok(), "apply failed: {result:?}");
        assert_eq!(
            moves, 0,
            "an in-place window must not be moved or un-maximized"
        );
    }

    // Regression (#274): the style read before the un-maximize carried
    // `WS_MAXIMIZE`, and writing it back re-flagged the placed window as
    // maximized.
    #[test]
    fn apply_does_not_leave_ws_maximize_on_a_window_it_unmaximized() {
        let Some(w) = TestWindow::maximized("GameFrameTestMaximizeBitCleared", None) else {
            return;
        };
        assert!(is_maximized(w.hwnd), "precondition: flagged maximized");

        let result = apply_borderless_to_window(w.hwnd, ZONE);

        assert!(result.is_ok(), "apply failed: {result:?}");
        assert!(
            !is_maximized(w.hwnd),
            "a placed window must not carry WS_MAXIMIZE"
        );
    }

    // A maximized window outside the Zone still has to be un-maximized and
    // placed; the in-place shortcut must not swallow it.
    #[test]
    fn apply_unmaximizes_and_places_a_maximized_window_outside_the_zone() {
        let Some(w) = TestWindow::maximized("GameFrameTestMaximizedOutsideZone", None) else {
            return;
        };
        assert!(is_maximized(w.hwnd), "precondition: flagged maximized");
        assert_ne!(
            get_window_region(w.hwnd).unwrap(),
            ZONE,
            "precondition: not in the Zone"
        );
        let before = MOVES.with(Cell::get);

        let result = apply_borderless_to_window(w.hwnd, ZONE);

        let moves = MOVES.with(Cell::get) - before;
        assert!(result.is_ok(), "apply failed: {result:?}");
        assert!(moves > 0, "the window must receive the move messages");
        assert_eq!(get_window_region(w.hwnd).unwrap(), ZONE);
    }

    // The record apply keeps for a window it un-maximizes must hold the
    // original bordered style (without `WS_MAXIMIZE`), the original extended
    // style and the normal rect: not the stripped borderless style, the
    // maximized rect or the Zone.
    #[test]
    fn apply_records_the_pre_apply_state_of_a_window_it_unmaximizes() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{WS_CAPTION, WS_THICKFRAME};
        let Some(w) = TestWindow::bordered_maximized("GameFrameTestRecordsOriginal") else {
            return;
        };
        assert!(is_maximized(w.hwnd), "precondition: flagged maximized");
        let original_style = get_window_style(w.hwnd).unwrap() & !WS_MAXIMIZE;
        let original_ex_style = get_window_ex_style(w.hwnd).unwrap();
        assert_eq!(
            original_style & (WS_CAPTION | WS_THICKFRAME),
            WS_CAPTION | WS_THICKFRAME,
            "precondition: bordered"
        );
        assert_ne!(CREATED, ZONE, "precondition: normal rect is not the Zone");

        let result = apply_borderless_to_window(w.hwnd, ZONE);

        assert!(result.is_ok(), "apply failed: {result:?}");
        let recorded = with_placed(|p| p.restore.get(w.hwnd as isize))
            .expect("apply must record a restore state");
        assert_eq!(recorded.style, original_style);
        assert_eq!(recorded.ex_style, original_ex_style);
        assert_eq!(recorded.region, CREATED);
    }

    #[test]
    fn apply_is_skipped_for_a_hung_window_whatever_its_state() {
        assert_eq!(decide_apply(true, true), ApplyDecision::SkipHung);
        assert_eq!(decide_apply(true, false), ApplyDecision::SkipHung);
    }
}
