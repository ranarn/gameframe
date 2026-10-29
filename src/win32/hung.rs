//! Hung-window detection and its once-per-window logging.

use super::with_placed;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::IsHungAppWindow;

/// The most windows [`HungSkipLog`] remembers; past it the log starts over.
const HUNG_LOG_CAP: usize = 64;

/// Windows whose hung skip is already logged, so a hung game costs one log
/// line, not one per foreground event.
#[derive(Debug, Default)]
pub(super) struct HungSkipLog {
    logged: std::collections::BTreeSet<isize>,
}

impl HungSkipLog {
    pub(super) const fn new() -> Self {
        Self {
            logged: std::collections::BTreeSet::new(),
        }
    }

    /// Records one apply request on `window`; true when its skip should be
    /// logged (first time since it was last responsive). A responsive window
    /// re-arms the log.
    pub(super) fn note(&mut self, window: isize, hung: bool) -> bool {
        if hung {
            // Entries normally leave on destroy or recovery; the cap bounds
            // what a missed destroy (or a replaced taskbar window) leaves.
            if self.logged.len() >= HUNG_LOG_CAP && !self.logged.contains(&window) {
                self.logged.clear();
            }
            self.logged.insert(window)
        } else {
            self.logged.remove(&window);
            false
        }
    }

    /// Drops `window` (it is gone), so a reused handle logs afresh.
    pub(super) fn forget(&mut self, window: isize) {
        self.logged.remove(&window);
    }
}

/// How long the drift handler waits before re-checking a window it skipped
/// because it was not responding.
pub(crate) const HUNG_RECHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether `hwnd`'s thread has stopped pumping messages (`IsHungAppWindow`).
/// Logs the first request on a window that is hung. Every blocking call into
/// a foreign window (a game or the taskbar) comes after this check.
pub(crate) fn is_window_hung_logged(hwnd: HWND) -> bool {
    // Safety: `IsHungAppWindow` only reads window state and tolerates a stale
    // handle.
    let hung = unsafe { IsHungAppWindow(hwnd) } != 0;
    let first = with_placed(|p| p.hung.note(hwnd as isize, hung));
    if first {
        log!("window {hwnd:?} is not responding; skipping calls into it until it is");
    }
    hung
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hung_window_is_logged_once_until_it_responds_again() {
        let mut log = HungSkipLog::default();

        assert!(log.note(7, true));
        assert!(!log.note(7, true));
        assert!(log.note(8, true), "another window has its own line");
        assert!(!log.note(7, false));
        assert!(log.note(7, true), "a recovered window logs a new hang");
    }

    #[test]
    fn the_hung_log_stays_bounded() {
        let mut log = HungSkipLog::default();
        for window in 0..(HUNG_LOG_CAP as isize * 3) {
            assert!(log.note(window, true));
        }
        assert!(log.logged.len() <= HUNG_LOG_CAP);
    }
}
