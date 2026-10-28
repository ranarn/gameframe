//! Main-thread hang watchdog: when the UI/event-loop thread stops pumping
//! messages, logs the last breadcrumb it passed and for how long it has
//! been stuck, so a "not responding" report can be traced from the log.
//!
//! A background thread pings the hidden window `instance_channel` owns on the
//! main thread with `SendMessageTimeout`; the main thread only answers while
//! it pumps. Breadcrumbs ([`step`]) are static labels set on entry to each
//! step that could block. No stack walking and no symbols: release builds
//! ship without a PDB. The ping interval is seconds, so the idle cost is
//! negligible. Switched by `Config::log_ui_hangs` through [`set_enabled`].

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_WINDOW_HANDLE, ERROR_TIMEOUT, GetLastError, SetLastError,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetWindowThreadProcessId, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_NULL,
};

/// Time between pings.
const PING_INTERVAL: Duration = Duration::from_secs(3);
/// How long a ping may go unanswered before it counts as a stall.
const STALL_THRESHOLD: Duration = Duration::from_secs(3);
/// Minimum time between "still stalled" lines.
const LOG_INTERVAL: Duration = Duration::from_secs(10);

/// What the watchdog should log after a ping.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The main thread has not answered for `stalled_for`.
    Stalled { stalled_for: Duration },
    /// The main thread answered again after a logged stall of `stalled_for`.
    Recovered { stalled_for: Duration },
}

/// Decides when a stall or a recovery is worth a log line. Pure: it is fed
/// timestamps, never reads the clock.
#[derive(Debug, Default)]
pub struct StallTracker {
    stalled_since: Option<Instant>,
    /// When a ping last came back answered: a stall began no earlier than that.
    last_answered: Option<Instant>,
    last_logged: Option<Instant>,
    /// Consecutive unanswered pings; a stall is only reported from the second.
    misses: u32,
}

impl StallTracker {
    /// Records one ping. `ping_started` is when it was sent, `now` when it
    /// returned, `answered` whether the main thread replied in time.
    ///
    /// A stall is dated from the last answered ping, not from the first missed
    /// one: the thread may have been stuck since just after that answer, so
    /// the reported time can overshoot by up to one ping interval but never
    /// undershoot. One missed ping is never enough to log: it needs two in a
    /// row, so a window torn down mid-ping cannot raise a false alarm.
    pub fn observe(
        &mut self,
        ping_started: Instant,
        now: Instant,
        answered: bool,
    ) -> Option<Verdict> {
        if answered {
            self.misses = 0;
            self.last_answered = Some(now);
            let since = self.stalled_since.take()?;
            // A stall that never reached a log line has no recovery line either.
            self.last_logged.take()?;
            return Some(Verdict::Recovered {
                stalled_for: now.saturating_duration_since(since),
            });
        }
        self.misses += 1;
        self.observe_miss(ping_started, now)
    }

    /// Records a ping that told nothing about the main thread (e.g. the window
    /// vanished mid-ping). It breaks a run of misses: miss, skip, miss is not
    /// two in a row.
    pub fn skip(&mut self) {
        self.misses = 0;
    }

    fn observe_miss(&mut self, ping_started: Instant, now: Instant) -> Option<Verdict> {
        let since = *self
            .stalled_since
            .get_or_insert(self.last_answered.unwrap_or(ping_started));
        let stalled_for = now.saturating_duration_since(since);
        let due = self
            .last_logged
            .is_none_or(|t| now.saturating_duration_since(t) >= LOG_INTERVAL);
        if self.misses >= 2 && stalled_for >= STALL_THRESHOLD && due {
            self.last_logged = Some(now);
            return Some(Verdict::Stalled { stalled_for });
        }
        None
    }
}

/// Remembers whether an unexpected `SendMessageTimeoutW` error code was
/// already reported, so a persistent failure logs one line, not one per ping.
#[derive(Debug, Default)]
pub struct ErrorReporter {
    reported: bool,
}

impl ErrorReporter {
    /// Returns the code if it is the first unexpected one (neither a timeout
    /// nor an invalid window handle) and so should be logged.
    pub fn first_unexpected(&mut self, code: u32) -> Option<u32> {
        if code == ERROR_TIMEOUT || code == ERROR_INVALID_WINDOW_HANDLE || self.reported {
            return None;
        }
        self.reported = true;
        Some(code)
    }
}

/// Last breadcrumb: label and when it was entered.
static CRUMB: Mutex<(&'static str, Option<Instant>)> = Mutex::new(("startup", None));

fn crumb() -> MutexGuard<'static, (&'static str, Option<Instant>)> {
    CRUMB.lock().unwrap_or_else(|e| e.into_inner())
}

/// Marks entry into a step that could block, on the calling thread. Until the
/// next call it stays the "last step", so a long-running step hands back to
/// an idle or enclosing label when it finishes.
pub fn step(label: &'static str) {
    *crumb() = (label, Some(Instant::now()));
}

/// Sets the breadcrumb until the returned guard drops, then restores the
/// previous one. For hook callbacks that run inside whatever the thread was
/// doing: once they return, the report names that again, not the finished hook.
pub fn scoped(label: &'static str) -> Scoped {
    let mut c = crumb();
    let previous = *c;
    let set = (label, Some(Instant::now()));
    *c = set;
    Scoped { previous, set }
}

/// Restores the previous breadcrumb on drop; see [`scoped`].
pub struct Scoped {
    previous: (&'static str, Option<Instant>),
    /// What the scope itself set: only restore if it is still current.
    set: (&'static str, Option<Instant>),
}

impl Drop for Scoped {
    fn drop(&mut self) {
        let mut c = crumb();
        // A `step()` made inside the scope is newer information: keep it.
        if *c == self.set {
            *c = self.previous;
        }
    }
}

/// A running watchdog thread. Dropping `stop` tells it to exit.
struct Running {
    /// Held only for its drop.
    _stop: Sender<()>,
    thread: JoinHandle<()>,
}

static STATE: Mutex<Option<Running>> = Mutex::new(None);
/// Watchdog threads currently alive (0 or 1 in practice; tests read it).
static ALIVE: AtomicUsize = AtomicUsize::new(0);

/// Decrements [`ALIVE`] when the thread ends, including by panic.
struct AliveGuard;

impl Drop for AliveGuard {
    fn drop(&mut self) {
        ALIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Makes sure `slot` holds a live thread: keeps a running one, replaces one
/// that has died (panicked or exited) by calling `start`.
fn ensure_running(slot: &mut Option<Running>, start: impl FnOnce() -> Option<Running>) {
    if slot.as_ref().is_some_and(|r| !r.thread.is_finished()) {
        return;
    }
    *slot = start();
}

/// Starts (`true`) or stops (`false`) the watchdog. Call on the main thread:
/// that is the thread it watches. Idempotent; takes effect at once. Enabling
/// restarts a watchdog thread that has died.
pub fn set_enabled(enabled: bool) {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if !enabled {
        // Dropping the sender wakes the thread out of its sleep, and it exits.
        *state = None;
        return;
    }
    ensure_running(&mut state, || {
        // SAFETY: plain Win32 call.
        let main_thread = unsafe { GetCurrentThreadId() };
        let (tx, rx) = mpsc::channel();
        ALIVE.fetch_add(1, Ordering::SeqCst);
        let spawned = std::thread::Builder::new()
            .name("hang-watchdog".into())
            .spawn(move || {
                let _alive = AliveGuard;
                run(main_thread, rx);
            });
        match spawned {
            Ok(thread) => Some(Running { _stop: tx, thread }),
            Err(e) => {
                ALIVE.fetch_sub(1, Ordering::SeqCst);
                log!("hang watchdog: could not start its thread: {e}");
                None
            }
        }
    });
}

fn run(main_thread: u32, stop: mpsc::Receiver<()>) {
    let mut tracker = StallTracker::default();
    let mut errors = ErrorReporter::default();
    let mut warned_no_window = false;
    let mut warned_wrong_thread = false;
    loop {
        match stop.recv_timeout(PING_INTERVAL) {
            Err(RecvTimeoutError::Timeout) => {}
            // Disconnected (or a message): switched off.
            _ => return,
        }
        let Some(hwnd) = crate::instance_channel::command_window() else {
            // Normal around startup and shutdown.
            if !warned_no_window {
                warned_no_window = true;
                log!("hang watchdog: no window to ping yet; idle until it exists");
            }
            continue;
        };
        warned_no_window = false;
        // SAFETY: plain Win32 calls on a handle just looked up; a stale
        // handle only makes them fail.
        let (ping_started, answered) = unsafe {
            let mut pid = 0u32;
            if GetWindowThreadProcessId(hwnd, &mut pid) != main_thread {
                // Not the main thread's window; nothing to ping. Say so once.
                if !warned_wrong_thread {
                    warned_wrong_thread = true;
                    log!("hang watchdog: the ping window is not on the main thread; not watching");
                }
                continue;
            }
            warned_wrong_thread = false;
            let started = Instant::now();
            let mut result = 0usize;
            // Directly before the call, so a stale error cannot be misread.
            SetLastError(0);
            // Answered only while the main thread pumps.
            let ok = SendMessageTimeoutW(
                hwnd,
                WM_NULL,
                0,
                0,
                SMTO_ABORTIFHUNG,
                STALL_THRESHOLD.as_millis() as u32,
                &mut result,
            );
            if ok == 0 {
                // A timeout (or a hung-window abort, which may leave the error
                // at 0) is a miss. Anything else, e.g. the window destroyed
                // mid-ping, says nothing about the main thread: skip it.
                let err = GetLastError();
                if err != 0 && err != ERROR_TIMEOUT {
                    if let Some(code) = errors.first_unexpected(err) {
                        log!("hang watchdog: ping skipped, unexpected error code {code}");
                    }
                    tracker.skip();
                    continue;
                }
            }
            (started, ok != 0)
        };
        match tracker.observe(ping_started, Instant::now(), answered) {
            Some(Verdict::Stalled { stalled_for }) => {
                // `log!` takes the global log lock: if the main thread is stuck
                // inside a log write, this line waits for it. Accepted.
                let (label, entered) = *crumb();
                log!(
                    "UI thread not responding for ~{:.1}s; last step '{}' entered {} ago",
                    stalled_for.as_secs_f32(),
                    label,
                    entered.map_or_else(
                        || "(never)".to_string(),
                        |t| format!("{:.1}s", t.elapsed().as_secs_f32())
                    )
                );
            }
            Some(Verdict::Recovered { stalled_for }) => {
                log!(
                    "UI thread responding again after a stall of ~{:.1}s",
                    stalled_for.as_secs_f32()
                );
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(base: Instant, s: u64) -> Instant {
        base + Duration::from_secs(s)
    }

    /// One ping that started at `start` and returned at `end`, unanswered.
    fn miss(t: &mut StallTracker, base: Instant, start: u64, end: u64) -> Option<Verdict> {
        t.observe(secs(base, start), secs(base, end), false)
    }

    #[test]
    fn answered_pings_log_nothing() {
        let base = Instant::now();
        let mut t = StallTracker::default();
        assert_eq!(t.observe(base, secs(base, 1), true), None);
    }

    #[test]
    fn a_stall_is_logged_once_it_passes_the_threshold() {
        let base = Instant::now();
        let mut t = StallTracker::default();
        // Returned after 2 s: below the 3 s threshold.
        assert_eq!(miss(&mut t, base, 0, 2), None);
        assert_eq!(
            miss(&mut t, base, 2, 5),
            Some(Verdict::Stalled {
                stalled_for: Duration::from_secs(5)
            })
        );
    }

    #[test]
    fn a_continuing_stall_logs_at_most_every_ten_seconds() {
        let base = Instant::now();
        let mut t = StallTracker::default();
        assert_eq!(miss(&mut t, base, 0, 3), None);
        assert!(miss(&mut t, base, 3, 6).is_some());
        assert_eq!(miss(&mut t, base, 6, 9), None);
        assert_eq!(miss(&mut t, base, 12, 15), None);
        assert_eq!(
            miss(&mut t, base, 13, 16),
            Some(Verdict::Stalled {
                stalled_for: Duration::from_secs(16)
            })
        );
    }

    #[test]
    fn recovery_after_a_logged_stall_reports_the_total_time_once() {
        let base = Instant::now();
        let mut t = StallTracker::default();
        assert_eq!(miss(&mut t, base, 0, 3), None);
        assert!(miss(&mut t, base, 3, 6).is_some());
        assert_eq!(
            t.observe(secs(base, 7), secs(base, 8), true),
            Some(Verdict::Recovered {
                stalled_for: Duration::from_secs(8)
            })
        );
        assert_eq!(t.observe(secs(base, 9), secs(base, 10), true), None);
    }

    #[test]
    fn a_short_stall_that_was_never_logged_has_no_recovery_line() {
        let base = Instant::now();
        let mut t = StallTracker::default();
        assert_eq!(miss(&mut t, base, 0, 1), None);
        assert_eq!(t.observe(secs(base, 2), secs(base, 3), true), None);
    }

    #[test]
    fn a_second_stall_starts_a_fresh_count() {
        let base = Instant::now();
        let mut t = StallTracker::default();
        assert_eq!(miss(&mut t, base, 0, 3), None);
        assert!(miss(&mut t, base, 3, 6).is_some());
        assert!(t.observe(secs(base, 7), secs(base, 8), true).is_some());
        // Healthy in between, then a new stall counted from the last answer.
        assert_eq!(t.observe(secs(base, 19), secs(base, 20), true), None);
        assert_eq!(miss(&mut t, base, 20, 21), None);
        assert_eq!(
            miss(&mut t, base, 21, 24),
            Some(Verdict::Stalled {
                stalled_for: Duration::from_secs(4)
            })
        );
    }

    #[test]
    fn a_stall_is_dated_from_the_last_answered_ping() {
        let base = Instant::now();
        let mut t = StallTracker::default();
        assert_eq!(t.observe(secs(base, 0), secs(base, 1), true), None);
        // Two pings in a row fail at once (hung window): dated from the last answer.
        assert_eq!(miss(&mut t, base, 4, 4), None);
        assert_eq!(
            miss(&mut t, base, 7, 7),
            Some(Verdict::Stalled {
                stalled_for: Duration::from_secs(6)
            })
        );
        assert_eq!(
            t.observe(secs(base, 8), secs(base, 9), true),
            Some(Verdict::Recovered {
                stalled_for: Duration::from_secs(8)
            })
        );
    }

    #[test]
    fn a_single_missed_ping_never_logs_even_when_old_enough() {
        let base = Instant::now();
        let mut t = StallTracker::default();
        assert_eq!(t.observe(secs(base, 0), secs(base, 1), true), None);
        // 30 s later one ping misses: well past the threshold, but only one miss.
        assert_eq!(miss(&mut t, base, 30, 30), None);
        // The window answers again: no stall happened, so no recovery line.
        assert_eq!(t.observe(secs(base, 33), secs(base, 33), true), None);
    }

    #[test]
    fn an_answer_between_misses_resets_the_count() {
        let base = Instant::now();
        let mut t = StallTracker::default();
        assert_eq!(miss(&mut t, base, 0, 3), None);
        assert_eq!(t.observe(secs(base, 3), secs(base, 4), true), None);
        // A later lone miss is again only the first.
        assert_eq!(miss(&mut t, base, 10, 13), None);
    }

    // One test: the breadcrumb is process-global, so parallel tests would race.
    #[test]
    fn step_records_the_latest_label_and_scoped_restores_the_previous() {
        step("test step");
        let (label, entered) = *crumb();
        assert_eq!(label, "test step");
        assert!(entered.is_some());
        {
            let _inner = scoped("inner");
            assert_eq!(crumb().0, "inner");
        }
        assert_eq!(crumb().0, "test step");
        {
            let _inner = scoped("inner");
            step("nested step");
        }
        assert_eq!(crumb().0, "nested step");
    }

    #[test]
    fn a_skipped_ping_breaks_a_run_of_misses() {
        let base = Instant::now();
        let mut t = StallTracker::default();
        assert_eq!(miss(&mut t, base, 0, 3), None);
        t.skip();
        // Would be the second miss, and a stall, without the skip.
        assert_eq!(miss(&mut t, base, 3, 6), None);
    }

    #[test]
    fn an_unexpected_error_code_is_reported_once() {
        let mut r = ErrorReporter::default();
        assert_eq!(r.first_unexpected(ERROR_TIMEOUT), None);
        assert_eq!(r.first_unexpected(ERROR_INVALID_WINDOW_HANDLE), None);
        assert_eq!(r.first_unexpected(5), Some(5));
        assert_eq!(r.first_unexpected(5), None);
        assert_eq!(r.first_unexpected(6), None);
    }

    fn finished_running() -> Running {
        let (stop, _rx) = mpsc::channel();
        let thread = std::thread::spawn(|| {});
        while !thread.is_finished() {
            std::thread::sleep(Duration::from_millis(1));
        }
        Running {
            _stop: stop,
            thread,
        }
    }

    #[test]
    fn enabling_restarts_a_thread_that_has_died() {
        let mut slot = Some(finished_running());
        let mut started = 0;
        ensure_running(&mut slot, || {
            started += 1;
            Some(finished_running())
        });
        assert_eq!(started, 1);
        assert!(slot.is_some());
    }

    #[test]
    fn enabling_keeps_a_thread_that_is_still_running() {
        let (stop, rx) = mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let _ = rx.recv();
        });
        let mut slot = Some(Running {
            _stop: stop,
            thread,
        });
        ensure_running(&mut slot, || panic!("must not restart a live thread"));
        // Dropping the slot drops the sender and ends the thread.
    }

    #[test]
    fn disabling_stops_the_thread_and_enabling_twice_runs_only_one() {
        set_enabled(true);
        set_enabled(true);
        assert_eq!(ALIVE.load(Ordering::SeqCst), 1);
        set_enabled(false);
        let deadline = Instant::now() + Duration::from_secs(5);
        while ALIVE.load(Ordering::SeqCst) != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(ALIVE.load(Ordering::SeqCst), 0);
    }
}
