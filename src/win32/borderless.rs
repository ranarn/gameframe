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

/// One step of an apply request, in execution order. The reads are steps too:
/// a failed read aborts before the window is touched, and the order of reads
/// against writes is what the plan pins down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApplyStep {
    /// Read the normal rect and both styles of a maximized window, before it
    /// is un-maximized.
    ReadPreRestore,
    Unmaximize,
    /// Record the pre-restore read as the window's [`RestoreState`], but only
    /// if the un-maximize took effect (so a window GameFrame cannot change,
    /// e.g. elevated, gets no entry). This is the first mutation, so a later
    /// failure cannot strand the window un-maximized with no way back.
    RecordPreRestoreIfUnmaximized,
    /// Re-read the style after the un-maximize; this, not the pre-restore
    /// read, is what gets written back (#274), so `WS_MAXIMIZE` is not re-added.
    ReadPostRestoreStyle,
    /// Read style, extended style and region of a window that was not maximized.
    ReadLive,
    SetStyle,
    /// Backstop record of the live read once the style change succeeded, for
    /// a window not recorded at its un-maximize. A no-op if already recorded.
    RecordLive,
    SetExStyle,
    SetRegion,
    DisableCorners,
}

/// What one apply request does to a window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApplyPlan {
    /// The window's thread is not responding: no call is made.
    Hung,
    /// The steps to run in order, stopping at the first failure.
    Steps(Vec<ApplyStep>),
}

/// Pure plan for one apply request from the observed facts. A hung window wins
/// over everything else, since even reading it back would be pointless. A
/// window already in place (not `drifted`) only gets the corner preference,
/// before any un-maximize (#272): a game that re-maximizes itself keeps
/// `WS_MAXIMIZE` while sitting exactly in the Zone.
pub(crate) fn plan_apply(hung: bool, drifted: bool, maximized: bool) -> ApplyPlan {
    use ApplyStep::*;
    if hung {
        return ApplyPlan::Hung;
    }
    if !drifted {
        return ApplyPlan::Steps(vec![DisableCorners]);
    }
    let mut steps = if maximized {
        vec![
            ReadPreRestore,
            Unmaximize,
            RecordPreRestoreIfUnmaximized,
            ReadPostRestoreStyle,
        ]
    } else {
        vec![ReadLive]
    };
    steps.extend([SetStyle, RecordLive, SetExStyle, SetRegion, DisableCorners]);
    ApplyPlan::Steps(steps)
}

/// A window's style, extended style and region, as read at one point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) style: u32,
    pub(crate) ex_style: u32,
    pub(crate) region: Region,
}

/// Which read a [`Snapshot`] comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// A maximized window: its normal rect, not the maximized one.
    PreRestore,
    /// The window as it is now.
    Live,
}

/// The Win32 primitives [`run_steps`] calls. `run_steps` owns the values
/// flowing between steps, so an implementation holds no state of its own.
pub(crate) trait ApplyOps {
    fn read(&self, source: Source) -> Result<Snapshot, Win32Error>;
    fn read_style(&self) -> Result<u32, Win32Error>;
    /// Un-maximizes; whether the window is no longer maximized afterwards.
    fn unmaximize(&self) -> bool;
    fn record(&self, snapshot: Snapshot);
    fn set_style(&self, style: u32) -> Result<(), Win32Error>;
    fn set_ex_style(&self, ex_style: u32) -> Result<(), Win32Error>;
    fn set_region(&self) -> Result<(), Win32Error>;
    fn disable_corners(&self) -> Result<(), Win32Error>;
}

/// Executes `steps` in order and stops at the first failing step. A plan that
/// uses a value before the step producing it is an error, not a panic.
pub(crate) fn run_steps(ops: &impl ApplyOps, steps: &[ApplyStep]) -> Result<(), Win32Error> {
    let out_of_order = || Win32Error::Os(std::io::Error::other("apply steps out of order"));
    let mut pre: Option<Snapshot> = None;
    let mut live: Option<Snapshot> = None;
    let mut unmaximized = false;
    for step in steps {
        match step {
            ApplyStep::ReadPreRestore => {
                pre = Some(ops.read(Source::PreRestore)?);
                live = pre;
            }
            ApplyStep::Unmaximize => unmaximized = ops.unmaximize(),
            ApplyStep::RecordPreRestoreIfUnmaximized => {
                if unmaximized {
                    ops.record(pre.ok_or_else(out_of_order)?);
                }
            }
            ApplyStep::ReadPostRestoreStyle => {
                // Re-read rather than clear the bit by hand, so a restore that
                // did not take effect keeps its real state.
                let style = ops.read_style()?;
                live = Some(Snapshot {
                    style,
                    ..pre.ok_or_else(out_of_order)?
                });
            }
            ApplyStep::ReadLive => live = Some(ops.read(Source::Live)?),
            ApplyStep::SetStyle => {
                ops.set_style(borderless_style(live.ok_or_else(out_of_order)?.style))?
            }
            ApplyStep::RecordLive => ops.record(live.ok_or_else(out_of_order)?),
            ApplyStep::SetExStyle => ops.set_ex_style(strip_border_ex_style(
                live.ok_or_else(out_of_order)?.ex_style,
            ))?,
            ApplyStep::SetRegion => ops.set_region()?,
            ApplyStep::DisableCorners => ops.disable_corners()?,
        }
    }
    Ok(())
}

/// [`ApplyOps`] on a real window.
struct WindowOps {
    hwnd: HWND,
    target_region: Region,
}

impl ApplyOps for WindowOps {
    fn read(&self, source: Source) -> Result<Snapshot, Win32Error> {
        Ok(match source {
            Source::PreRestore => {
                let region = get_restorable_region(self.hwnd, true)?;
                Snapshot {
                    region,
                    style: get_window_style(self.hwnd)?,
                    ex_style: get_window_ex_style(self.hwnd)?,
                }
            }
            Source::Live => Snapshot {
                style: get_window_style(self.hwnd)?,
                ex_style: get_window_ex_style(self.hwnd)?,
                region: get_window_region(self.hwnd)?,
            },
        })
    }

    fn read_style(&self) -> Result<u32, Win32Error> {
        get_window_style(self.hwnd)
    }

    fn unmaximize(&self) -> bool {
        restore_maximized_window(self.hwnd);
        !is_maximized(self.hwnd)
    }

    fn record(&self, s: Snapshot) {
        with_placed(|p| {
            p.restore.record_if_absent(
                self.hwnd as isize,
                RestoreState::new(s.style, s.ex_style, s.region),
            );
        });
    }

    fn set_style(&self, style: u32) -> Result<(), Win32Error> {
        set_window_style(self.hwnd, style)
    }

    fn set_ex_style(&self, ex_style: u32) -> Result<(), Win32Error> {
        set_window_ex_style(self.hwnd, ex_style)
    }

    fn set_region(&self) -> Result<(), Win32Error> {
        set_window_region(self.hwnd, self.target_region)
    }

    fn disable_corners(&self) -> Result<(), Win32Error> {
        disable_rounded_corners(self.hwnd)
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
    // The in-place decision comes first: un-maximizing a game that re-maximizes
    // itself on every focus switch would snap it to its normal rect and back,
    // making the game resize its swapchain twice. A hung window is not probed.
    let drifted = !hung && window_has_drifted(hwnd, target_region).unwrap_or(true);
    // Only a window that will be applied is probed for its maximized state.
    let maximized = !hung && drifted && is_maximized(hwnd);
    match plan_apply(hung, drifted, maximized) {
        ApplyPlan::Hung => Err(Win32Error::WindowHung),
        ApplyPlan::Steps(steps) => run_steps(
            &WindowOps {
                hwnd,
                target_region,
            },
            &steps,
        ),
    }
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
    use std::cell::RefCell;
    use windows_sys::Win32::UI::WindowsAndMessaging::WS_MAXIMIZE;

    use ApplyStep::*;

    fn steps(hung: bool, drifted: bool, maximized: bool) -> Vec<ApplyStep> {
        match plan_apply(hung, drifted, maximized) {
            ApplyPlan::Steps(s) => s,
            ApplyPlan::Hung => panic!("expected steps"),
        }
    }

    #[test]
    fn a_hung_window_gets_no_steps_whatever_its_state() {
        for drifted in [false, true] {
            for maximized in [false, true] {
                assert_eq!(plan_apply(true, drifted, maximized), ApplyPlan::Hung);
            }
        }
    }

    // #272: no un-maximize (or any read/write) for a window already in place.
    #[test]
    fn a_window_in_place_only_gets_the_corner_preference() {
        assert_eq!(steps(false, false, false), vec![DisableCorners]);
        assert_eq!(steps(false, false, true), vec![DisableCorners]);
    }

    #[test]
    fn a_drifted_window_that_is_not_maximized_is_read_then_stripped() {
        assert_eq!(
            steps(false, true, false),
            vec![
                ReadLive,
                SetStyle,
                RecordLive,
                SetExStyle,
                SetRegion,
                DisableCorners
            ]
        );
    }

    // Pins both #282 (the early record right after the un-maximize, before
    // every later fallible step) and #274 (the style written comes from the
    // post-restore re-read).
    #[test]
    fn a_drifted_maximized_window_is_unmaximized_and_recorded_before_the_strip() {
        assert_eq!(
            steps(false, true, true),
            vec![
                ReadPreRestore,
                Unmaximize,
                RecordPreRestoreIfUnmaximized,
                ReadPostRestoreStyle,
                SetStyle,
                RecordLive,
                SetExStyle,
                SetRegion,
                DisableCorners
            ]
        );
    }

    const PRE: Snapshot = Snapshot {
        style: WS_MAXIMIZE | 0x1,
        ex_style: 0x10,
        region: Region {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        },
    };
    const POST_STYLE: u32 = 0x2;

    /// Records the calls it gets, with the values passed; fails the call named
    /// in `fail_at`, and reports `unmaximized` from `unmaximize`. Reads return
    /// [`PRE`], except `read_style`, which returns the distinct [`POST_STYLE`].
    struct FakeOps {
        calls: RefCell<Vec<String>>,
        fail_at: Option<&'static str>,
        unmaximized: bool,
    }

    impl FakeOps {
        fn new(fail_at: Option<&'static str>, unmaximized: bool) -> Self {
            FakeOps {
                calls: RefCell::new(Vec::new()),
                fail_at,
                unmaximized,
            }
        }

        fn step(&self, name: &str, arg: String) -> Result<(), Win32Error> {
            self.calls.borrow_mut().push(format!("{name}{arg}"));
            if self.fail_at == Some(name) {
                Err(Win32Error::WindowHung)
            } else {
                Ok(())
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl ApplyOps for FakeOps {
        fn read(&self, source: Source) -> Result<Snapshot, Win32Error> {
            self.step("read", String::new())?;
            assert_eq!(source, Source::PreRestore);
            Ok(PRE)
        }

        fn read_style(&self) -> Result<u32, Win32Error> {
            self.step("read_style", String::new())?;
            Ok(POST_STYLE)
        }

        fn unmaximize(&self) -> bool {
            self.calls.borrow_mut().push("unmaximize".into());
            self.unmaximized
        }

        fn record(&self, s: Snapshot) {
            self.calls
                .borrow_mut()
                .push(format!("record({:#x})", s.style));
        }

        fn set_style(&self, style: u32) -> Result<(), Win32Error> {
            self.step("set_style", format!("({style:#x})"))
        }

        fn set_ex_style(&self, _: u32) -> Result<(), Win32Error> {
            self.step("set_ex_style", String::new())
        }

        fn set_region(&self) -> Result<(), Win32Error> {
            self.step("set_region", String::new())
        }

        fn disable_corners(&self) -> Result<(), Win32Error> {
            self.step("disable_corners", String::new())
        }
    }

    fn run(ops: &FakeOps) -> Result<(), Win32Error> {
        run_steps(ops, &steps(false, true, true))
    }

    // #282: a failure in the step right after the un-maximize leaves the
    // pre-restore state recorded (the executor stops at that failure).
    #[test]
    fn a_failure_after_the_unmaximize_leaves_the_original_recorded() {
        let ops = FakeOps::new(Some("read_style"), true);
        assert!(run(&ops).is_err());
        assert_eq!(
            ops.calls(),
            [
                "read".to_string(),
                "unmaximize".into(),
                format!("record({:#x})", PRE.style),
                "read_style".into()
            ]
        );
    }

    #[test]
    fn an_unmaximize_that_did_not_take_effect_records_nothing_early() {
        let ops = FakeOps::new(None, false);
        assert!(run(&ops).is_ok());
        let calls = ops.calls();
        let at = calls.iter().position(|c| c == "unmaximize").unwrap();
        assert_eq!(calls[at + 1], "read_style", "no record right after");
        assert!(calls.iter().any(|c| c.starts_with("record")), "backstop");
    }

    #[test]
    fn a_failed_pre_restore_read_aborts_before_the_window_is_touched() {
        let ops = FakeOps::new(Some("read"), true);
        assert!(run(&ops).is_err());
        assert_eq!(ops.calls(), ["read"]);
    }

    #[test]
    fn the_executor_stops_at_the_first_failing_step() {
        let ops = FakeOps::new(Some("set_style"), true);
        assert!(run(&ops).is_err());
        let calls = ops.calls();
        assert!(calls.last().unwrap().starts_with("set_style"));
        assert!(!calls.iter().any(|c| c == "set_ex_style"));
    }

    // #274: the style written after an un-maximize is the post-restore
    // re-read, not the pre-restore style (which still has WS_MAXIMIZE).
    #[test]
    fn the_executor_writes_the_post_restore_style() {
        let ops = FakeOps::new(None, true);
        assert!(run(&ops).is_ok());
        let written = format!("set_style({:#x})", borderless_style(POST_STYLE));
        assert!(ops.calls().contains(&written), "{:?}", ops.calls());
    }

    #[test]
    fn a_plan_that_strips_before_reading_is_an_error_not_a_panic() {
        let ops = FakeOps::new(None, true);
        assert!(run_steps(&ops, &[SetStyle]).is_err());
        assert!(ops.calls().is_empty());
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
            let mut needed = 0u32;
            let ok = GetUserObjectInformationW(
                station,
                UOI_FLAGS,
                (&mut flags as *mut USEROBJECTFLAGS).cast(),
                std::mem::size_of::<USEROBJECTFLAGS>() as u32,
                &mut needed,
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
}
