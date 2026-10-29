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

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_WINDOW_HANDLE, ERROR_TIMEOUT, GetLastError, SetLastError,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GUI_INMENUMODE, GUI_INMOVESIZE, GUI_POPUPMENUMODE, GUI_SYSTEMMENUMODE, GUITHREADINFO,
    GetGUIThreadInfo, GetWindowThreadProcessId, IsHungAppWindow, SMTO_ABORTIFHUNG,
    SendMessageTimeoutW, WM_NULL,
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
    /// Last heartbeat count seen and when it was first seen at that value.
    beat_seen: Option<(u64, Instant)>,
}

/// A reading of the heartbeat the UI code bumps (see [`beat`] and [`idle`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Beat {
    /// Bumped at the start of each UI step.
    pub count: u64,
    /// `true` between a [`beat`] and the next [`idle`]: the main thread is
    /// doing work, not waiting for a message. A frozen count only means
    /// something while busy.
    pub busy: bool,
}

impl StallTracker {
    /// Like [`observe`](Self::observe), plus the heartbeat. A ping answered
    /// while the thread is inside an outgoing cross-process `SendMessage` is
    /// no proof of life (Windows lets a sender answer incoming sends), so a
    /// busy heartbeat that has not moved for the stall threshold is a stall
    /// even though the ping came back. It is dated from when the frozen count
    /// was first seen, which can lag the true start by up to one ping interval.
    ///
    /// `user_modal` says the thread sits in a system modal loop the user holds
    /// open (menu, window drag/resize): those pump and answer pings while the
    /// heartbeat is frozen, so the stall timer is reset instead.
    pub fn observe_with_beat(
        &mut self,
        ping_started: Instant,
        now: Instant,
        answered: bool,
        beat: Beat,
        user_modal: bool,
    ) -> Option<Verdict> {
        let changed_at = match self.beat_seen {
            Some((count, at)) if count == beat.count && !user_modal => at,
            _ => {
                self.beat_seen = Some((beat.count, now));
                now
            }
        };
        let frozen = beat.busy && now.saturating_duration_since(changed_at) >= STALL_THRESHOLD;
        if !(answered && frozen) {
            return self.observe(ping_started, now, answered);
        }
        // Answered but frozen: no run of misses needed, the heartbeat spans time.
        self.misses = 0;
        let since = *self.stalled_since.get_or_insert(changed_at);
        let due = self
            .last_logged
            .is_none_or(|t| now.saturating_duration_since(t) >= LOG_INTERVAL);
        if !due {
            return None;
        }
        self.last_logged = Some(now);
        Some(Verdict::Stalled {
            stalled_for: now.saturating_duration_since(since),
        })
    }

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

    /// Records a ping that told nothing about the main thread (no window, the
    /// window vanished mid-ping). It breaks a run of misses: miss, skip, miss
    /// is not two in a row.
    pub fn skip(&mut self) {
        self.misses = 0;
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

/// The heartbeat in one atomic word, so a reader sees count and busy flag
/// together: the count in the upper 63 bits, the busy flag in bit 0.
#[derive(Debug, Default)]
struct Heartbeat(AtomicU64);

impl Heartbeat {
    const BUSY_BIT: u64 = 1;

    /// One relaxed read-modify-write; never blocks.
    fn beat(&self) {
        let _ = self
            .0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some((((v >> 1).wrapping_add(1)) << 1) | Self::BUSY_BIT)
            });
    }

    fn idle(&self) {
        self.0.fetch_and(!Self::BUSY_BIT, Ordering::Relaxed);
    }

    fn read(&self) -> Beat {
        let v = self.0.load(Ordering::Relaxed);
        Beat {
            count: v >> 1,
            busy: v & Self::BUSY_BIT != 0,
        }
    }

    /// Runs `f` (a call that pumps its own message loop, e.g. a modal box)
    /// marked idle when `watched`, so a box the user leaves open is no stall;
    /// busy again afterwards if it was before.
    fn around_modal<R>(&self, watched: bool, f: impl FnOnce() -> R) -> R {
        let was_busy = watched && self.read().busy;
        if was_busy {
            self.idle();
        }
        let result = f();
        if was_busy {
            self.beat();
        }
        result
    }
}

static HEARTBEAT: Heartbeat = Heartbeat(AtomicU64::new(0));
/// Id of the thread the watchdog watches (0 until it is first enabled).
static WATCHED_THREAD: AtomicU32 = AtomicU32::new(0);

/// Heartbeat: call at the start of each UI step on the main thread (message
/// dispatch, Settings' and the zone editor's `ui()`). One relaxed atomic
/// operation, never blocks.
pub fn beat() {
    HEARTBEAT.beat();
}

/// Marks the main thread as waiting (idle) until the next [`beat`], so a quiet
/// window that has nothing to redraw is not mistaken for a stall.
pub fn idle() {
    HEARTBEAT.idle();
}

/// Runs `f`, a call that pumps its own message loop (a modal message box),
/// with the heartbeat idle if the caller is the watched main thread: the user
/// may leave the box open as long as they like. Other threads are untouched.
pub fn modal<R>(f: impl FnOnce() -> R) -> R {
    // SAFETY: plain Win32 call.
    let watched = unsafe { GetCurrentThreadId() } == WATCHED_THREAD.load(Ordering::Relaxed);
    HEARTBEAT.around_modal(watched, f)
}

/// Calls [`idle`] on drop, so every exit from a `ui()` marks the thread idle.
pub struct IdleOnDrop;

impl Drop for IdleOnDrop {
    fn drop(&mut self) {
        idle();
    }
}

fn read_beat() -> Beat {
    HEARTBEAT.read()
}

/// Whether `flags` (`GUITHREADINFO::flags`) show a menu or a move/size loop.
fn is_user_modal(flags: u32) -> bool {
    flags & (GUI_INMENUMODE | GUI_POPUPMENUMODE | GUI_SYSTEMMENUMODE | GUI_INMOVESIZE) != 0
}

/// Whether the watched thread is in a menu or move/size modal loop (tray
/// menu, system menu, dragging the window). False if the query fails.
fn thread_in_user_modal(thread: u32) -> bool {
    // SAFETY: GUITHREADINFO is plain data; cbSize is set before the call.
    unsafe {
        let mut info: GUITHREADINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
        GetGUIThreadInfo(thread, &mut info) != 0 && is_user_modal(info.flags)
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
/// Watchdog threads currently alive (tests read it). Normally 0 or 1; it can
/// read 2 briefly after a disable straight followed by an enable, until the
/// old thread wakes (its channel is already disconnected, so that is at once)
/// and exits. Accepted: there is never a lasting double ping.
static ALIVE: AtomicUsize = AtomicUsize::new(0);

/// Decrements its counter when the thread ends, including by panic.
struct AliveGuard(&'static AtomicUsize);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Makes sure `slot` holds a live thread: keeps a running one, replaces one
/// that has died (panicked or exited) by calling `start`.
fn ensure_running(slot: &mut Option<Running>, start: impl FnOnce() -> Option<Running>) {
    if slot.as_ref().is_some_and(|r| !r.thread.is_finished()) {
        return;
    }
    if slot.is_some() {
        log!("hang watchdog: its thread had died; restarting it");
    }
    *slot = start();
}

/// Starts (`true`) or stops (`false`) the watchdog. Call on the main thread:
/// that is the thread it watches. Idempotent; takes effect at once. Enabling
/// restarts a watchdog thread that has died.
pub fn set_enabled(enabled: bool) {
    set_enabled_for(enabled, Target::TrayProcess);
}

/// [`set_enabled`] for a Settings / zone-editor child process (#186), whose
/// main thread has no command window: it pings the child's own window.
pub fn set_enabled_in_child(enabled: bool) {
    set_enabled_for(enabled, Target::ChildWindow);
}

/// Which window the watchdog pings to see whether the main thread pumps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// The tray process's command window.
    TrayProcess,
    /// The Settings or zone-editor window of a child process.
    ChildWindow,
}

impl Target {
    fn window(self) -> Option<windows_sys::Win32::Foundation::HWND> {
        match self {
            Self::TrayProcess => crate::instance_channel::command_window(),
            Self::ChildWindow => crate::instance_channel::find_own_ui_window(),
        }
    }
}

fn set_enabled_for(enabled: bool, target: Target) {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if !enabled {
        // Dropping the sender wakes the thread out of its sleep, and it exits.
        *state = None;
        return;
    }
    ensure_running(&mut state, || {
        // SAFETY: plain Win32 call.
        let main_thread = unsafe { GetCurrentThreadId() };
        WATCHED_THREAD.store(main_thread, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        ALIVE.fetch_add(1, Ordering::SeqCst);
        let spawned = std::thread::Builder::new()
            .name("hang-watchdog".into())
            .spawn(move || {
                let _alive = AliveGuard(&ALIVE);
                run(main_thread, target, rx);
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

/// Logs once per episode when Windows itself flags the Settings window as hung
/// (`IsHungAppWindow`); `logged` re-arms once it is responsive again.
fn log_if_settings_hung(logged: &mut bool) {
    let hung = crate::instance_channel::find_settings_window()
        // SAFETY: plain Win32 call; a stale handle only makes it return FALSE.
        .is_some_and(|hwnd| unsafe { IsHungAppWindow(hwnd) } != 0);
    if first_hung_report(hung, logged) {
        log!("Windows reports the Settings window as not responding");
    }
}

/// Whether to log now: only on the first `hung` reading of an episode.
/// `logged` re-arms as soon as the window is responsive again.
fn first_hung_report(hung: bool, logged: &mut bool) -> bool {
    let report = hung && !*logged;
    *logged = hung;
    report
}

fn run(main_thread: u32, target: Target, stop: mpsc::Receiver<()>) {
    let mut tracker = StallTracker::default();
    let mut errors = ErrorReporter::default();
    let mut warned_no_window = false;
    let mut warned_wrong_thread = false;
    let mut settings_hung_logged = false;
    loop {
        match stop.recv_timeout(PING_INTERVAL) {
            Err(RecvTimeoutError::Timeout) => {}
            // Disconnected (or a message): switched off.
            _ => return,
        }
        let Some(hwnd) = target.window() else {
            // Normal around startup and shutdown.
            if !warned_no_window {
                warned_no_window = true;
                log!("hang watchdog: no window to ping yet; idle until it exists");
            }
            tracker.skip();
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
                tracker.skip();
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
        log_if_settings_hung(&mut settings_hung_logged);
        let user_modal = thread_in_user_modal(main_thread);
        match tracker.observe_with_beat(
            ping_started,
            Instant::now(),
            answered,
            read_beat(),
            user_modal,
        ) {
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

    fn busy(count: u64) -> Beat {
        Beat { count, busy: true }
    }

    /// An answered ping at second `t`.
    fn answered_at(t: &mut StallTracker, base: Instant, s: u64, beat: Beat) -> Option<Verdict> {
        t.observe_with_beat(secs(base, s), secs(base, s), true, beat, false)
    }

    #[test]
    fn a_frozen_heartbeat_inside_a_user_modal_loop_is_no_stall() {
        let base = Instant::now();
        let mut t = StallTracker::default();
        for s in [0, 3, 6, 30] {
            assert_eq!(
                t.observe_with_beat(secs(base, s), secs(base, s), true, busy(7), true),
                None
            );
        }
        // The menu closes: the timer restarts, so no instant stall.
        assert_eq!(answered_at(&mut t, base, 31, busy(7)), None);
        assert!(answered_at(&mut t, base, 34, busy(7)).is_some());
    }

    #[test]
    fn menu_and_move_size_flags_count_as_user_modal() {
        assert!(is_user_modal(GUI_INMENUMODE));
        assert!(is_user_modal(GUI_POPUPMENUMODE));
        assert!(is_user_modal(GUI_SYSTEMMENUMODE));
        assert!(is_user_modal(GUI_INMOVESIZE));
        assert!(!is_user_modal(0));
    }

    #[test]
    fn a_thread_outside_any_menu_or_drag_is_not_user_modal() {
        // The real GetGUIThreadInfo query on this (test) thread, which is in
        // no menu or move/size loop; an unknown thread id fails and counts as
        // not modal.
        // SAFETY: no arguments or preconditions.
        let current = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
        assert!(!thread_in_user_modal(current));
        assert!(!thread_in_user_modal(0));
    }

    #[test]
    fn a_frozen_busy_heartbeat_is_a_stall_even_when_the_ping_is_answered() {
        let base = Instant::now();
        let mut t = StallTracker::default();
        assert_eq!(answered_at(&mut t, base, 0, busy(7)), None);
        assert_eq!(answered_at(&mut t, base, 2, busy(7)), None);
        assert_eq!(
            answered_at(&mut t, base, 3, busy(7)),
            Some(Verdict::Stalled {
                stalled_for: Duration::from_secs(3)
            })
        );
    }

    #[test]
    fn a_moving_heartbeat_is_no_stall() {
        let base = Instant::now();
        let mut t = StallTracker::default();
        for (s, count) in [(0, 1), (3, 2), (6, 3), (9, 4)] {
            assert_eq!(answered_at(&mut t, base, s, busy(count)), None);
        }
    }

    #[test]
    fn a_frozen_heartbeat_while_idle_is_no_stall() {
        let base = Instant::now();
        let mut t = StallTracker::default();
        let idle = Beat {
            count: 5,
            busy: false,
        };
        assert_eq!(answered_at(&mut t, base, 0, idle), None);
        assert_eq!(answered_at(&mut t, base, 30, idle), None);
    }

    #[test]
    fn a_heartbeat_stall_repeats_every_ten_seconds_then_recovers() {
        let base = Instant::now();
        let mut t = StallTracker::default();
        assert_eq!(answered_at(&mut t, base, 0, busy(1)), None);
        assert!(answered_at(&mut t, base, 3, busy(1)).is_some());
        assert_eq!(answered_at(&mut t, base, 6, busy(1)), None);
        assert_eq!(
            answered_at(&mut t, base, 13, busy(1)),
            Some(Verdict::Stalled {
                stalled_for: Duration::from_secs(13)
            })
        );
        assert_eq!(
            answered_at(&mut t, base, 16, busy(2)),
            Some(Verdict::Recovered {
                stalled_for: Duration::from_secs(16)
            })
        );
    }

    #[test]
    fn an_unanswered_ping_still_works_through_the_heartbeat_path() {
        let base = Instant::now();
        let mut t = StallTracker::default();
        let beat = busy(1);
        assert_eq!(
            t.observe_with_beat(secs(base, 0), secs(base, 3), false, beat, false),
            None
        );
        assert!(
            t.observe_with_beat(secs(base, 3), secs(base, 6), false, beat, false)
                .is_some()
        );
    }

    #[test]
    fn beat_marks_busy_and_idle_clears_it() {
        let hb = Heartbeat::default();
        let at = |count, busy| Beat { count, busy };
        assert_eq!(hb.read(), at(0, false));
        hb.beat();
        assert_eq!(hb.read(), at(1, true));
        hb.idle();
        assert_eq!(hb.read(), at(1, false));
        hb.beat();
        assert_eq!(hb.read(), at(2, true));
    }

    #[test]
    fn a_modal_call_is_idle_inside_and_busy_again_after() {
        let hb = Heartbeat::default();
        hb.beat();
        let inside = hb.around_modal(true, || hb.read());
        assert!(!inside.busy);
        let after = hb.read();
        assert!(after.busy);
        assert!(after.count > inside.count);
    }

    #[test]
    fn a_modal_call_leaves_an_idle_heartbeat_idle() {
        let hb = Heartbeat::default();
        hb.beat();
        hb.idle();
        hb.around_modal(true, || {});
        assert!(!hb.read().busy);
    }

    #[test]
    fn a_modal_call_on_another_thread_does_not_touch_the_heartbeat() {
        let hb = Heartbeat::default();
        hb.beat();
        let before = hb.read();
        let inside = hb.around_modal(false, || hb.read());
        assert_eq!(inside, before);
        assert_eq!(hb.read(), before);
    }

    #[test]
    fn a_hung_settings_window_is_reported_once_per_episode() {
        let mut logged = false;
        assert!(!first_hung_report(false, &mut logged));
        assert!(first_hung_report(true, &mut logged));
        assert!(!first_hung_report(true, &mut logged));
        assert!(!first_hung_report(false, &mut logged));
        // Responsive in between: the next episode reports again.
        assert!(first_hung_report(true, &mut logged));
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
        let deadline = Instant::now() + Duration::from_secs(5);
        while !thread.is_finished() {
            assert!(
                Instant::now() < deadline,
                "helper thread did not finish within 5 s"
            );
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
    fn a_panicking_thread_still_decrements_its_alive_count() {
        static COUNT: AtomicUsize = AtomicUsize::new(1);
        let handle = std::thread::spawn(|| {
            let _alive = AliveGuard(&COUNT);
            // The default hook prints the message to stderr; harmless here.
            panic!("deliberate panic to test the drop guard");
        });
        assert!(handle.join().is_err());
        assert_eq!(COUNT.load(Ordering::SeqCst), 0);
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
