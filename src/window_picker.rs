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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, PostThreadMessageW, WM_NULL};

/// One picker entry: the window handle (as `isize`, so it is `Send`) and its
/// title.
pub(crate) type PickerEntry = (isize, String);

/// Lets at most one refresh run at a time; a request that arrives meanwhile is
/// dropped, because the running refresh is about to deliver a fresh list.
#[derive(Default)]
struct RefreshGate(AtomicBool);

impl RefreshGate {
    /// `true` if the caller now owns the refresh and must call [`finish`].
    fn try_begin(&self) -> bool {
        !self.0.swap(true, Ordering::AcqRel)
    }

    fn finish(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Releases the gate when dropped, so a worker that panics cannot leave the
/// picker refusing every later request.
struct GateGuard(Arc<RefreshGate>);

impl Drop for GateGuard {
    fn drop(&mut self) {
        self.0.finish();
    }
}

/// Wakes the event loop so it picks up the finished list. Posts to the
/// process's real command window rather than the thread: a thread message is
/// dropped by a modal loop, a window message is dispatched. Falls back to a
/// thread message if the window does not exist.
fn wake_loop(loop_thread: u32) {
    // Safety: plain integer/handle arguments; a failed post only delays the
    // menu rebuild to the next message.
    unsafe {
        match crate::instance_channel::command_window() {
            Some(hwnd) => {
                PostMessageW(hwnd, WM_NULL, 0, 0);
            }
            None => {
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
        if !self.gate.try_begin() {
            return;
        }
        let gate = Arc::clone(&self.gate);
        let sender = self.sender.clone();
        let loop_thread = self.loop_thread;
        let spawned = std::thread::Builder::new()
            .name("window-picker".into())
            .spawn(move || {
                // Released when the closure ends -- also on a panic -- and
                // after the send below, so a newer worker can never queue its
                // list ahead of this one.
                let _guard = GateGuard(gate);
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
        assert!(gate.try_begin());
        assert!(!gate.try_begin());
    }

    #[test]
    fn a_request_after_the_refresh_finished_is_accepted() {
        let gate = RefreshGate::default();
        assert!(gate.try_begin());
        gate.finish();
        assert!(gate.try_begin());
    }

    #[test]
    fn a_panicking_worker_still_releases_the_gate() {
        let gate = Arc::new(RefreshGate::default());
        assert!(gate.try_begin());
        let worker_gate = Arc::clone(&gate);
        let worker = std::thread::spawn(move || {
            let _guard = GateGuard(worker_gate);
            panic!("worker failed");
        });
        assert!(worker.join().is_err());
        assert!(gate.try_begin());
    }
}
