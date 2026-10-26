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
use std::time::{Duration, Instant};
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
}

impl StallTracker {
    /// Records one ping. `ping_started` is when it was sent, `now` when it
    /// returned, `answered` whether the main thread replied in time.
    ///
    /// A stall is dated from the last answered ping, not from the first missed
    /// one: the thread may have been stuck since just after that answer, so
    /// the reported time can overshoot by up to one ping interval but never
    /// undershoot.
    pub fn observe(
        &mut self,
        ping_started: Instant,
        now: Instant,
        answered: bool,
    ) -> Option<Verdict> {
        if answered {
            self.last_answered = Some(now);
            let since = self.stalled_since.take()?;
            // A stall that never reached a log line has no recovery line either.
            self.last_logged.take()?;
            return Some(Verdict::Recovered {
                stalled_for: now.saturating_duration_since(since),
            });
        }
        let since = *self
            .stalled_since
            .get_or_insert(self.last_answered.unwrap_or(ping_started));
        let stalled_for = now.saturating_duration_since(since);
        let due = self
            .last_logged
            .is_none_or(|t| now.saturating_duration_since(t) >= LOG_INTERVAL);
        if stalled_for >= STALL_THRESHOLD && due {
            self.last_logged = Some(now);
            return Some(Verdict::Stalled { stalled_for });
        }
        None
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
    *c = (label, Some(Instant::now()));
    Scoped(previous)
}

/// Restores the previous breadcrumb on drop; see [`scoped`].
pub struct Scoped((&'static str, Option<Instant>));

impl Drop for Scoped {
    fn drop(&mut self) {
        *crumb() = self.0;
    }
}

/// Sender whose drop tells the running watchdog thread to stop.
static STOP: Mutex<Option<Sender<()>>> = Mutex::new(None);
/// Watchdog threads currently alive (0 or 1 in practice; tests read it).
static ALIVE: AtomicUsize = AtomicUsize::new(0);

/// Starts (`true`) or stops (`false`) the watchdog. Call on the main thread:
/// that is the thread it watches. Idempotent; takes effect at once.
pub fn set_enabled(enabled: bool) {
    let mut stop = STOP.lock().unwrap_or_else(|e| e.into_inner());
    if !enabled {
        // Dropping the sender wakes the thread out of its sleep, and it exits.
        *stop = None;
        return;
    }
    if stop.is_some() {
        return;
    }
    // SAFETY: plain Win32 call.
    let main_thread = unsafe { GetCurrentThreadId() };
    let (tx, rx) = mpsc::channel();
    ALIVE.fetch_add(1, Ordering::SeqCst);
    let spawned = std::thread::Builder::new()
        .name("hang-watchdog".into())
        .spawn(move || {
            run(main_thread, rx);
            ALIVE.fetch_sub(1, Ordering::SeqCst);
        });
    match spawned {
        Ok(_) => *stop = Some(tx),
        Err(e) => {
            ALIVE.fetch_sub(1, Ordering::SeqCst);
            log!("hang watchdog: could not start its thread: {e}");
        }
    }
}

fn run(main_thread: u32, stop: mpsc::Receiver<()>) {
    let mut tracker = StallTracker::default();
    let mut warned_no_window = false;
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
                continue; // Not the main thread's window; nothing to ping.
            }
            let started = Instant::now();
            let mut result = 0usize;
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
        assert!(miss(&mut t, base, 0, 3).is_some());
        assert_eq!(miss(&mut t, base, 3, 6), None);
        assert_eq!(miss(&mut t, base, 6, 9), None);
        assert_eq!(
            miss(&mut t, base, 10, 13),
            Some(Verdict::Stalled {
                stalled_for: Duration::from_secs(13)
            })
        );
        assert_eq!(miss(&mut t, base, 13, 16), None);
    }

    #[test]
    fn recovery_after_a_logged_stall_reports_the_total_time_once() {
        let base = Instant::now();
        let mut t = StallTracker::default();
        assert!(miss(&mut t, base, 0, 3).is_some());
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
        assert!(miss(&mut t, base, 0, 3).is_some());
        assert!(t.observe(secs(base, 4), secs(base, 5), true).is_some());
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
        // The next ping fails at once (hung window): 3 s after the last answer.
        assert_eq!(
            miss(&mut t, base, 4, 4),
            Some(Verdict::Stalled {
                stalled_for: Duration::from_secs(3)
            })
        );
        assert_eq!(
            t.observe(secs(base, 8), secs(base, 9), true),
            Some(Verdict::Recovered {
                stalled_for: Duration::from_secs(8)
            })
        );
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
