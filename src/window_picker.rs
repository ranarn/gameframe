//! Builds the "Apply to Window" picker's window list off the UI thread.
//!
//! Listing the windows is slow: it walks the desktop, resolves every
//! window's executable, snapshots process modules and may rescan the Steam
//! libraries (which can stall for seconds on a spun-down or unreachable
//! drive). A tray hover therefore only calls [`WindowPicker::request`], which
//! starts one worker thread; the finished list comes back through a channel
//! and a no-op message to the command window wakes the event loop, which then
//! rebuilds the menu from [`WindowPicker::take_latest`]. A menu that is
//! already open (its modal loop does not run the event loop) keeps showing the
//! previous list; the next open shows the new one.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, PostThreadMessageW, WM_NULL};

/// One picker entry: the window handle (as `isize`, so it is `Send`) and its
/// title.
pub(crate) type PickerEntry = (isize, String);

/// How long a refresh may hold the gate before a new request starts its own
/// worker: a worker that hangs (a stalled drive) must not block the picker
/// forever.
const GATE_TIMEOUT: Duration = Duration::from_secs(30);

/// Identifies one refresh that holds the gate.
type Ticket = u64;

#[derive(Default)]
struct GateState {
    /// The refresh in flight and when it started.
    holder: Option<(Ticket, Instant)>,
    next_ticket: Ticket,
}

/// Lets at most one refresh run at a time; a request that arrives meanwhile is
/// dropped, because the running refresh is about to deliver a fresh list. A
/// refresh older than [`GATE_TIMEOUT`] is presumed hung and is superseded.
#[derive(Default)]
struct RefreshGate(Mutex<GateState>);

impl RefreshGate {
    /// The ticket the caller must hand to [`finish`] if it now owns the
    /// refresh, `None` if a younger refresh is running.
    fn try_begin(&self, now: Instant) -> Option<Ticket> {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((_, started)) = state.holder
            && now.saturating_duration_since(started) < GATE_TIMEOUT
        {
            return None;
        }
        state.next_ticket += 1;
        let ticket = state.next_ticket;
        state.holder = Some((ticket, now));
        Some(ticket)
    }

    /// Releases the gate, unless a newer refresh superseded `ticket` (a late
    /// finish of a hung worker must not free the gate under its replacement).
    fn finish(&self, ticket: Ticket) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if state.holder.is_some_and(|(held, _)| held == ticket) {
            state.holder = None;
        }
    }
}

/// Releases the gate when dropped, so a worker that panics cannot leave the
/// picker refusing every later request.
struct GateGuard(Arc<RefreshGate>, Ticket);

impl Drop for GateGuard {
    fn drop(&mut self) {
        self.0.finish(self.1);
    }
}

/// Where [`wake_loop`] posts its no-op message.
#[derive(Debug, PartialEq, Eq)]
enum WakeTarget {
    Window(HWND),
    Thread,
}

/// The command window when it exists, else the loop thread.
fn wake_target(command_window: Option<HWND>) -> WakeTarget {
    command_window.map_or(WakeTarget::Thread, WakeTarget::Window)
}

/// Wakes the event loop so it picks up the finished list. Posts to the
/// process's real command window rather than the thread: a thread message is
/// dropped by a modal loop, a window message is dispatched. Falls back to a
/// thread message if the window does not exist.
fn wake_loop(loop_thread: u32) {
    // Safety: plain integer/handle arguments; a failed post only delays the
    // menu rebuild to the next message.
    unsafe {
        match wake_target(crate::instance_channel::command_window()) {
            WakeTarget::Window(hwnd) => {
                PostMessageW(hwnd, WM_NULL, 0, 0);
            }
            WakeTarget::Thread => {
                PostThreadMessageW(loop_thread, WM_NULL, 0, 0);
            }
        }
    }
}

/// The event loop's handle to the background picker refresh. Created on the
/// loop thread.
pub(crate) struct WindowPicker {
    loop_thread: u32,
    gate: Arc<RefreshGate>,
    sender: mpsc::Sender<Vec<PickerEntry>>,
    receiver: mpsc::Receiver<Vec<PickerEntry>>,
}

impl WindowPicker {
    pub(crate) fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            // Safety: `GetCurrentThreadId` takes no arguments and cannot fail.
            loop_thread: unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() },
            gate: Arc::new(RefreshGate::default()),
            sender,
            receiver,
        }
    }

    /// Starts a background refresh unless one is already running. Never
    /// blocks: the config read, the window walk and the Steam scan all happen
    /// on the worker.
    pub(crate) fn request(&self, config_dir: PathBuf) {
        let Some(ticket) = self.gate.try_begin(Instant::now()) else {
            return;
        };
        let gate = Arc::clone(&self.gate);
        let sender = self.sender.clone();
        let loop_thread = self.loop_thread;
        let spawned = std::thread::Builder::new()
            .name("window-picker".into())
            .spawn(move || {
                // Released when the closure ends -- also on a panic -- and
                // after the send below, so a newer worker can never queue its
                // list ahead of this one.
                let _guard = GateGuard(gate, ticket);
                let tracked_exes = match crate::profile_store::load(&config_dir) {
                    Ok(config) => config
                        .tracked_games
                        .into_iter()
                        .map(|game| game.exe_path)
                        .collect(),
                    Err(e) => {
                        log!("window picker: couldn't load the config: {e}");
                        Vec::new()
                    }
                };
                let entries: Vec<PickerEntry> = crate::win32::list_visible_windows(tracked_exes)
                    .into_iter()
                    .map(|(hwnd, title)| (hwnd as isize, title))
                    .collect();
                if sender.send(entries).is_ok() {
                    wake_loop(loop_thread);
                }
            });
        // A failed spawn drops the closure, and with it the gate guard.
        if let Err(e) = spawned {
            log!("couldn't start the window picker refresh: {e}");
        }
    }

    /// The newest finished list, if a refresh completed since the last call.
    pub(crate) fn take_latest(&self) -> Option<Vec<PickerEntry>> {
        self.receiver.try_iter().last()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_request_while_one_runs_is_dropped() {
        let gate = RefreshGate::default();
        let t0 = Instant::now();
        assert!(gate.try_begin(t0).is_some());
        assert!(gate.try_begin(t0).is_none());
    }

    #[test]
    fn a_request_after_the_refresh_finished_is_accepted() {
        let gate = RefreshGate::default();
        let t0 = Instant::now();
        let ticket = gate.try_begin(t0).unwrap();
        gate.finish(ticket);
        assert!(gate.try_begin(t0).is_some());
    }

    #[test]
    fn a_panicking_worker_still_releases_the_gate() {
        let gate = Arc::new(RefreshGate::default());
        let ticket = gate.try_begin(Instant::now()).unwrap();
        let worker_gate = Arc::clone(&gate);
        let worker = std::thread::spawn(move || {
            let _guard = GateGuard(worker_gate, ticket);
            panic!("worker failed");
        });
        assert!(worker.join().is_err());
        assert!(gate.try_begin(Instant::now()).is_some());
    }

    #[test]
    fn a_hung_refresh_is_superseded_after_the_timeout() {
        let gate = RefreshGate::default();
        let t0 = Instant::now();
        gate.try_begin(t0).unwrap();

        let almost = t0 + GATE_TIMEOUT - Duration::from_secs(1);
        assert!(gate.try_begin(almost).is_none(), "still within the timeout");
        assert!(gate.try_begin(t0 + GATE_TIMEOUT).is_some());
    }

    #[test]
    fn a_superseded_worker_finishing_late_does_not_free_the_new_one() {
        let gate = RefreshGate::default();
        let t0 = Instant::now();
        let hung = gate.try_begin(t0).unwrap();
        let later = t0 + GATE_TIMEOUT;
        gate.try_begin(later).unwrap();

        gate.finish(hung);

        assert!(
            gate.try_begin(later).is_none(),
            "the replacement still holds it"
        );
    }

    #[test]
    fn take_latest_returns_the_newest_list_then_nothing() {
        let picker = WindowPicker::new();
        picker.sender.send(vec![(1, "old".into())]).unwrap();
        picker.sender.send(vec![(2, "new".into())]).unwrap();

        assert_eq!(picker.take_latest(), Some(vec![(2, "new".to_string())]));
        assert_eq!(picker.take_latest(), None);
    }

    #[test]
    fn the_loop_is_woken_through_the_command_window_when_it_exists() {
        let hwnd = 5usize as HWND;
        assert_eq!(wake_target(Some(hwnd)), WakeTarget::Window(hwnd));
        assert_eq!(wake_target(None), WakeTarget::Thread);
    }
}
