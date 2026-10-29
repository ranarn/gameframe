//! The in-game-resize watcher's decisions: when a drifted window is re-applied.
//! Whether a window has drifted at all (`window_drifted`/`window_has_drifted`)
//! is a geometry check and lives in `style.rs`.

use super::with_placed;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Shell::{
    QUERY_USER_NOTIFICATION_STATE, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsIconic};

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
pub(super) struct DriftAttempts {
    recent: std::collections::BTreeMap<isize, Vec<std::time::Instant>>,
    capped: std::collections::BTreeSet<isize>,
    /// Capped windows whose cap is already logged.
    cap_logged: std::collections::BTreeSet<isize>,
    /// Windows whose config/Zone failure on the drift path is already logged.
    failure_logged: std::collections::BTreeSet<isize>,
}

impl DriftAttempts {
    pub(super) const fn new() -> Self {
        Self {
            recent: std::collections::BTreeMap::new(),
            capped: std::collections::BTreeSet::new(),
            cap_logged: std::collections::BTreeSet::new(),
            failure_logged: std::collections::BTreeSet::new(),
        }
    }

    /// Notes a config/Zone failure on `window`'s drift path. True the first
    /// time since [`Self::clear_failure`] or [`Self::forget`], so the caller
    /// logs it once instead of once per location-change event.
    pub(super) fn note_failure(&mut self, window: isize) -> bool {
        self.failure_logged.insert(window)
    }

    /// Re-arms the failure log of `window` (the condition went away).
    pub(super) fn clear_failure(&mut self, window: isize) {
        self.failure_logged.remove(&window);
    }

    /// Decides for `window` at `now`. Does not record an attempt; the caller
    /// does that with [`Self::record`] once it actually re-applies.
    pub(super) fn decide(
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
    pub(super) fn record(&mut self, window: isize, now: std::time::Instant) {
        let attempts = self.recent.entry(window).or_default();
        attempts.retain(|t| now.saturating_duration_since(*t) < DRIFT_REAPPLY_CAP_WINDOW);
        attempts.push(now);
        if attempts.len() >= DRIFT_REAPPLY_CAP {
            self.capped.insert(window);
        }
    }

    /// Notes that a skipped event of `window` is being reported. True the first
    /// time since the cap was set, so the caller logs it once.
    pub(super) fn note_capped(&mut self, window: isize) -> bool {
        self.cap_logged.insert(window)
    }

    /// Forgets `window`'s attempts and lifts its cap.
    pub(super) fn forget(&mut self, window: isize) {
        self.recent.remove(&window);
        self.capped.remove(&window);
        self.cap_logged.remove(&window);
        self.failure_logged.remove(&window);
    }
}

/// Whether `hwnd` is minimized (`IsIconic`).
pub(crate) fn is_window_minimized(hwnd: HWND) -> bool {
    // Safety: `IsIconic` only reads window state and tolerates a stale handle.
    unsafe { IsIconic(hwnd) != 0 }
}

/// Decides whether a drift event on `hwnd` should re-apply the Zone.
pub(crate) fn decide_drift_for_window(hwnd: HWND, drifted: bool) -> DriftDecision {
    // Only a drifted window needs the live fullscreen read.
    let fullscreen = drifted && window_is_fullscreen(hwnd);
    with_placed(|p| {
        p.drift.decide(
            hwnd as isize,
            std::time::Instant::now(),
            drifted,
            fullscreen,
        )
    })
}

/// Records now as the time of `hwnd`'s latest drift re-apply attempt.
pub(crate) fn record_drift_attempt(hwnd: HWND) {
    with_placed(|p| p.drift.record(hwnd as isize, std::time::Instant::now()));
}

/// True the first time a capped `hwnd`'s event is skipped, so the caller logs
/// the cap once, when it actually stops a re-apply.
pub(crate) fn note_drift_capped(hwnd: HWND) -> bool {
    with_placed(|p| p.drift.note_capped(hwnd as isize))
}

/// True the first time a config/Zone failure is reported for `hwnd`'s drift
/// path since [`clear_drift_failure`] (or a reset / destroy).
pub(crate) fn note_drift_failure(hwnd: HWND) -> bool {
    with_placed(|p| p.drift.note_failure(hwnd as isize))
}

/// Re-arms the once-per-window drift failure log of `hwnd`.
pub(crate) fn clear_drift_failure(hwnd: HWND) {
    with_placed(|p| p.drift.clear_failure(hwnd as isize));
}

/// Lifts `hwnd`'s drift re-apply cap and forgets its recent attempts. Called
/// by [`crate::placement::apply_zone`] for a placement the user asked for (the
/// hotkey, the tray picker or Settings' live re-apply).
pub fn reset_drift_cap(hwnd: HWND) {
    with_placed(|p| p.drift.forget(hwnd as isize));
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

#[cfg(test)]
mod tests {
    use super::*;

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

        attempts.forget(1);

        assert!(attempts.note_capped(1), "a reset window reports a new cap");
    }

    #[test]
    fn a_drift_failure_is_reported_once_until_cleared_or_reset() {
        let mut attempts = DriftAttempts::default();

        assert!(attempts.note_failure(1));
        assert!(!attempts.note_failure(1));
        assert!(attempts.note_failure(2), "another window has its own line");

        attempts.clear_failure(1);
        assert!(
            attempts.note_failure(1),
            "a cleared failure is reported anew"
        );

        attempts.forget(1);
        assert!(attempts.note_failure(1), "a destroyed window starts fresh");
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

        attempts.forget(1);

        assert_eq!(
            attempts.decide(1, t0 + std::time::Duration::from_secs(10), true, false),
            DriftDecision::Reapply
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
}
