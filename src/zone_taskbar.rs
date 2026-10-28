//! The zone editor's hold on the taskbar: the control-box toggle and the
//! restore when the editor closes.
//!
//! The editor never touches a taskbar it did not hide itself:
//!
//! - Opening the editor leaves the taskbar alone and reads its state once.
//! - Hidden at open (by a game, the hotkey or the user's own Windows
//!   auto-hide): the toggle is locked and closing does nothing.
//! - Visible at open: the toggle hides it with [`taskbar::hide_taskbar`],
//!   capturing the [`win32::TaskbarHideRestore`], and shows it again with
//!   [`taskbar::restore_taskbar`]. Closing restores it only if the session
//!   currently has it hidden.
//!
//! The toggle label follows the session's own state, never a per-click OS
//! re-read. Only the marked `taskbar` hide/restore entry points are used, so
//! the crash-recovery marker ([`crate::taskbar_recovery`]) stays right and the
//! editor leaves no record behind beyond what they manage. The decisions take
//! the OS actions as closures, so they are unit tested without a real
//! taskbar.

use crate::profile_store::TaskbarHideMode;
use crate::taskbar;
use crate::win32;
use std::cell::RefCell;
use std::rc::Rc;

/// The taskbar as the editing session sees it.
#[derive(Debug, Default)]
pub(crate) struct TaskbarSession {
    /// The taskbar was already hidden at open: the session leaves it alone.
    locked: bool,
    /// The restore captured when the session hid the taskbar; `Some` while
    /// the session has it hidden.
    hidden_by_session: Option<win32::TaskbarHideRestore>,
    /// The last toggle failure, for the control box.
    error: Option<String>,
}

/// What closing the editor did about the taskbar.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CloseOutcome {
    NothingToDo,
    Restored,
    /// The session state was borrowed elsewhere, so nothing was done.
    Busy,
    Failed(String),
}

impl TaskbarSession {
    /// A session over a taskbar that is `hidden_at_open` when the editor
    /// opens.
    pub(crate) fn new(hidden_at_open: bool) -> Self {
        Self {
            locked: hidden_at_open,
            ..Self::default()
        }
    }

    /// Whether the toggle is disabled because the taskbar was hidden at open.
    pub(crate) fn is_locked(&self) -> bool {
        self.locked
    }

    /// Whether the session itself currently has the taskbar hidden: the
    /// toggle label follows this.
    pub(crate) fn is_hidden(&self) -> bool {
        self.hidden_by_session.is_some()
    }

    /// The last toggle failure, if any.
    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Performs the toggle with the given OS actions: shows the taskbar with
    /// the captured restore when the session hid it, else hides it. Does
    /// nothing when locked. A failure leaves the state unchanged and is kept
    /// in `error`; a success clears it.
    pub(crate) fn toggle_with<E: std::fmt::Display>(
        &mut self,
        hide: impl FnOnce() -> Result<win32::TaskbarHideRestore, E>,
        show: impl FnOnce(win32::TaskbarHideRestore) -> Result<(), E>,
    ) {
        if self.locked {
            return;
        }
        let result = match self.hidden_by_session {
            Some(captured) => show(captured)
                .map(|()| self.hidden_by_session = None)
                .map_err(|e| format!("could not show the taskbar: {e}")),
            None => hide()
                .map(|captured| self.hidden_by_session = Some(captured))
                .map_err(|e| format!("could not hide the taskbar: {e}")),
        };
        self.error = result.err();
    }

    /// Performs the toggle with `mode` against the real taskbar, logging a
    /// failure.
    pub(crate) fn toggle(&mut self, mode: TaskbarHideMode) {
        self.toggle_with(|| taskbar::hide_taskbar(mode), taskbar::restore_taskbar);
        if let Some(error) = &self.error {
            log!("zone editor: {error}");
        }
    }
}

/// Returns the taskbar to its state at open through `restore`: only a hide
/// the session itself made is undone. Never panics (it runs from a
/// destructor), so a busy `session` is reported rather than unwrapped.
pub(crate) fn restore_on_close<E: std::fmt::Display>(
    session: &RefCell<TaskbarSession>,
    restore: impl FnOnce(win32::TaskbarHideRestore) -> Result<(), E>,
) -> CloseOutcome {
    let Ok(mut session) = session.try_borrow_mut() else {
        return CloseOutcome::Busy;
    };
    match session.hidden_by_session.take() {
        None => CloseOutcome::NothingToDo,
        Some(captured) => match restore(captured) {
            Ok(()) => CloseOutcome::Restored,
            Err(e) => CloseOutcome::Failed(e.to_string()),
        },
    }
}

/// RAII guard that returns the taskbar to its state at open (see
/// [`restore_on_close`]) when dropped, whether that happens via normal return
/// or early `?` propagation. It does not cover a panic in the release build
/// (`panic = "abort"` skips destructors); the panic hook in
/// `crate::taskbar_recovery` does.
///
/// The OS errors are logged rather than propagated or unwrapped -- a `Drop`
/// impl must not panic (it could run during a debug build's unwind, where a
/// second panic aborts).
pub(crate) struct TaskbarRestoreGuard {
    session: Rc<RefCell<TaskbarSession>>,
}

impl TaskbarRestoreGuard {
    pub(crate) fn new(session: Rc<RefCell<TaskbarSession>>) -> Self {
        Self { session }
    }
}

impl Drop for TaskbarRestoreGuard {
    fn drop(&mut self) {
        match restore_on_close(&self.session, taskbar::restore_taskbar) {
            CloseOutcome::NothingToDo | CloseOutcome::Restored => {}
            CloseOutcome::Busy => log!("zone editor: taskbar state busy at close; not restored"),
            CloseOutcome::Failed(e) => {
                log!("zone editor: failed to return the taskbar to its state at open: {e}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win32::TaskbarHideRestore;

    const R: TaskbarHideRestore = TaskbarHideRestore::FullyHidden;

    /// Runs a close against a session, reporting whether a restore ran.
    fn close(session: TaskbarSession) -> (CloseOutcome, bool) {
        let cell = RefCell::new(session);
        let restored = std::cell::Cell::new(false);
        let outcome = restore_on_close::<String>(&cell, |r| {
            assert_eq!(r, R);
            restored.set(true);
            Ok(())
        });
        (outcome, restored.get())
    }

    fn hide(s: &mut TaskbarSession) {
        s.toggle_with::<String>(|| Ok(R), |_| panic!("must not show"));
    }

    fn show(s: &mut TaskbarSession) {
        s.toggle_with::<String>(
            || panic!("must not hide"),
            |r| {
                assert_eq!(r, R);
                Ok(())
            },
        );
    }

    #[test]
    fn hidden_at_open_locks_the_toggle_and_close_does_nothing() {
        let mut s = TaskbarSession::new(true);
        assert!(s.is_locked());
        s.toggle_with::<String>(|| panic!("must not hide"), |_| panic!("must not show"));
        assert!(!s.is_hidden());
        assert_eq!(s.error(), None);
        assert_eq!(close(s), (CloseOutcome::NothingToDo, false));
    }

    #[test]
    fn visible_at_open_is_unlocked() {
        assert!(!TaskbarSession::new(false).is_locked());
    }

    #[test]
    fn visible_at_open_untouched_close_does_nothing() {
        assert_eq!(
            close(TaskbarSession::new(false)),
            (CloseOutcome::NothingToDo, false)
        );
    }

    #[test]
    fn hide_then_close_restores_the_sessions_own_hide() {
        let mut s = TaskbarSession::new(false);
        hide(&mut s);
        assert!(s.is_hidden());
        assert_eq!(close(s), (CloseOutcome::Restored, true));
    }

    #[test]
    fn hide_show_then_close_does_nothing() {
        let mut s = TaskbarSession::new(false);
        hide(&mut s);
        show(&mut s);
        assert!(!s.is_hidden());
        assert_eq!(close(s), (CloseOutcome::NothingToDo, false));
    }

    #[test]
    fn hide_show_hide_then_close_restores() {
        let mut s = TaskbarSession::new(false);
        hide(&mut s);
        show(&mut s);
        hide(&mut s);
        assert_eq!(close(s), (CloseOutcome::Restored, true));
    }

    #[test]
    fn a_successful_toggle_clears_a_previous_error() {
        let mut s = TaskbarSession::new(false);
        s.toggle_with(|| Err("boom"), |_: TaskbarHideRestore| Ok(()));
        assert!(s.error().is_some());
        hide(&mut s);
        assert_eq!(s.error(), None);
    }

    #[test]
    fn a_failed_hide_reports_the_error_and_keeps_the_label() {
        let mut s = TaskbarSession::new(false);
        s.toggle_with(|| Err("boom"), |_| panic!("must not show"));
        assert!(!s.is_hidden());
        let error = s.error().unwrap();
        assert!(error.contains("hide") && error.contains("boom"), "{error}");
        assert_eq!(close(s), (CloseOutcome::NothingToDo, false));
    }

    #[test]
    fn a_failed_show_reports_the_error_and_keeps_the_hide() {
        let mut s = TaskbarSession::new(false);
        hide(&mut s);
        s.toggle_with(|| panic!("must not hide"), |_| Err("boom"));
        assert!(s.is_hidden());
        let error = s.error().unwrap();
        assert!(error.contains("show") && error.contains("boom"), "{error}");
        // Close still restores it.
        assert_eq!(close(s), (CloseOutcome::Restored, true));
    }

    #[test]
    fn close_reports_a_failed_restore() {
        let mut s = TaskbarSession::new(false);
        hide(&mut s);
        let cell = RefCell::new(s);
        let outcome = restore_on_close(&cell, |_| Err("boom"));
        assert_eq!(outcome, CloseOutcome::Failed("boom".into()));
    }

    #[test]
    fn close_does_not_panic_or_act_when_the_session_is_busy() {
        let mut s = TaskbarSession::new(false);
        hide(&mut s);
        let cell = RefCell::new(s);
        let _held = cell.borrow_mut();
        let outcome = restore_on_close::<String>(&cell, |_| panic!("must not restore"));
        assert_eq!(outcome, CloseOutcome::Busy);
    }
}
