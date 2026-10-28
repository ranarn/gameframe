//! The zone editor's hold on the taskbar: the control-box toggle and the
//! restore when the editor closes.
//!
//! Opening the editor leaves the taskbar alone. Whatever the toggle does
//! while editing, closing returns the taskbar to the state it had at open:
//!
//! - hidden at open, visible at close: hide it again with the configured mode
//!   (for `HoverReveal` this re-enables the auto-hide flag the "Show" turned
//!   off);
//! - visible at open, hidden at close: restore it with the
//!   [`win32::TaskbarHideRestore`] the session captured on its own hide;
//! - otherwise: do nothing.
//!
//! Everything goes through the existing `win32` hide/restore primitives so the
//! crash-recovery marker ([`crate::taskbar_recovery`]) stays right. The
//! decisions are pure and take the OS actions as closures, so they are unit
//! tested without a real taskbar.

use crate::profile_store::TaskbarHideMode;
use crate::win32;
use std::cell::RefCell;
use std::rc::Rc;

/// The taskbar as the editing session sees it. `hidden_at_open` is the state
/// when the editor opened. `hidden` is the [`win32::TaskbarHideRestore`] the
/// session captured on its own last hide. `shown_hidden` is what the toggle
/// label follows, re-synced from the real taskbar at each click. `error` is
/// the last toggle failure, for the control box.
#[derive(Debug, Default)]
pub(crate) struct TaskbarSession {
    hidden_at_open: bool,
    hidden: Option<win32::TaskbarHideRestore>,
    shown_hidden: bool,
    pub(crate) error: Option<String>,
}

/// What the toggle button must do next.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TaskbarToggle {
    /// The taskbar is visible: hide it.
    Hide,
    /// The taskbar is hidden: show it, with the session's own captured state
    /// when it hid it, else with a plain absolute show.
    Show(Option<win32::TaskbarHideRestore>),
}

/// What closing the editor must do to return the taskbar to its state at
/// open.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CloseAction {
    Nothing,
    /// Hidden at open, visible now.
    Rehide,
    /// Visible at open, hidden now: undo the session's own hide.
    Restore(win32::TaskbarHideRestore),
}

/// What closing the editor did about the taskbar.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CloseOutcome {
    NothingToDo,
    Rehidden,
    Restored,
    /// The session state was borrowed elsewhere, so nothing was done.
    Busy,
    Failed(String),
}

/// Pure decision for closing: the state at open against the real state now.
pub(crate) fn close_action(
    hidden_at_open: bool,
    hidden_now: bool,
    remembered: Option<win32::TaskbarHideRestore>,
) -> CloseAction {
    match (hidden_at_open, hidden_now) {
        (true, false) => CloseAction::Rehide,
        (false, true) => remembered.map_or(CloseAction::Nothing, CloseAction::Restore),
        _ => CloseAction::Nothing,
    }
}

impl TaskbarSession {
    /// A session over a taskbar that is `hidden_at_open` when the editor
    /// opens.
    pub(crate) fn new(hidden_at_open: bool) -> Self {
        Self {
            hidden_at_open,
            shown_hidden: hidden_at_open,
            ..Self::default()
        }
    }

    /// Whether the taskbar is hidden as far as the session knows: the toggle
    /// button's label follows this.
    pub(crate) fn is_hidden(&self) -> bool {
        self.shown_hidden
    }

    /// Pure decision for the toggle button, from the taskbar's real state.
    pub(crate) fn toggle_action(&self, hidden_now: bool) -> TaskbarToggle {
        if hidden_now {
            TaskbarToggle::Show(self.hidden)
        } else {
            TaskbarToggle::Hide
        }
    }

    /// Performs the toggle with the given OS actions, given the taskbar's real
    /// state `hidden_now`. A failure leaves the capture unchanged and is kept
    /// in `error` so the control box can show it; a success clears `error`.
    /// Either way the label re-syncs to the real state.
    pub(crate) fn toggle_with<E: std::fmt::Display>(
        &mut self,
        hidden_now: bool,
        hide: impl FnOnce() -> Result<win32::TaskbarHideRestore, E>,
        show: impl FnOnce(Option<win32::TaskbarHideRestore>) -> Result<(), E>,
    ) {
        let result = match self.toggle_action(hidden_now) {
            TaskbarToggle::Hide => hide()
                .map(|remembered| {
                    self.hidden = Some(remembered);
                    self.shown_hidden = true;
                })
                .map_err(|e| format!("could not hide the taskbar: {e}")),
            TaskbarToggle::Show(remembered) => show(remembered)
                .map(|()| {
                    self.hidden = None;
                    self.shown_hidden = false;
                })
                .map_err(|e| format!("could not show the taskbar: {e}")),
        };
        if result.is_err() {
            self.shown_hidden = hidden_now;
        }
        self.error = result.err();
    }

    /// Performs the toggle with `mode` against the real taskbar, logging a
    /// failure.
    pub(crate) fn toggle(&mut self, mode: TaskbarHideMode) {
        self.toggle_with(
            win32::taskbar_hidden_now(mode),
            || win32::hide_taskbar(mode),
            |remembered| match remembered {
                Some(r) => win32::restore_taskbar(r),
                None => win32::set_taskbar_hide_absolute(false, mode),
            },
        );
        if let Some(error) = &self.error {
            log!("zone editor: {error}");
        }
    }
}

/// Returns the taskbar to its state at open (see the module docs), through
/// `rehide` and `restore`. `hidden_now` is the taskbar's real state at close.
/// Never panics (it runs from a destructor), so a busy `session` is reported
/// rather than unwrapped.
pub(crate) fn restore_on_close<E: std::fmt::Display>(
    session: &RefCell<TaskbarSession>,
    hidden_now: bool,
    rehide: impl FnOnce() -> Result<(), E>,
    restore: impl FnOnce(win32::TaskbarHideRestore) -> Result<(), E>,
) -> CloseOutcome {
    let Ok(mut session) = session.try_borrow_mut() else {
        return CloseOutcome::Busy;
    };
    match close_action(session.hidden_at_open, hidden_now, session.hidden.take()) {
        CloseAction::Nothing => CloseOutcome::NothingToDo,
        CloseAction::Rehide => match rehide() {
            Ok(()) => CloseOutcome::Rehidden,
            Err(e) => CloseOutcome::Failed(e.to_string()),
        },
        CloseAction::Restore(remembered) => match restore(remembered) {
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
    mode: TaskbarHideMode,
}

impl TaskbarRestoreGuard {
    pub(crate) fn new(session: Rc<RefCell<TaskbarSession>>, mode: TaskbarHideMode) -> Self {
        Self { session, mode }
    }
}

impl Drop for TaskbarRestoreGuard {
    fn drop(&mut self) {
        let mode = self.mode;
        let outcome = restore_on_close(
            &self.session,
            win32::taskbar_hidden_now(mode),
            || win32::set_taskbar_hide_absolute(true, mode),
            win32::restore_taskbar,
        );
        match outcome {
            CloseOutcome::NothingToDo | CloseOutcome::Rehidden | CloseOutcome::Restored => {}
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

    /// Runs a close against a session, recording which action ran.
    fn close(session: TaskbarSession, hidden_now: bool) -> (CloseOutcome, &'static str) {
        let cell = RefCell::new(session);
        let ran = std::cell::Cell::new("none");
        let outcome = restore_on_close::<String>(
            &cell,
            hidden_now,
            || {
                ran.set("rehide");
                Ok(())
            },
            |r| {
                assert_eq!(r, R);
                ran.set("restore");
                Ok(())
            },
        );
        (outcome, ran.get())
    }

    /// A session that hid a visible taskbar itself.
    fn session_that_hid() -> TaskbarSession {
        let mut s = TaskbarSession::new(false);
        s.toggle_with::<String>(false, || Ok(R), |_| panic!("must not show"));
        s
    }

    #[test]
    fn close_visible_at_open_visible_now_does_nothing() {
        let (outcome, ran) = close(TaskbarSession::new(false), false);
        assert_eq!((outcome, ran), (CloseOutcome::NothingToDo, "none"));
    }

    #[test]
    fn close_hidden_at_open_hidden_now_does_nothing() {
        let (outcome, ran) = close(TaskbarSession::new(true), true);
        assert_eq!((outcome, ran), (CloseOutcome::NothingToDo, "none"));
    }

    #[test]
    fn close_hidden_at_open_visible_now_hides_it_again() {
        let mut s = TaskbarSession::new(true);
        s.toggle_with::<String>(true, || panic!("must not hide"), |_| Ok(()));
        let (outcome, ran) = close(s, false);
        assert_eq!((outcome, ran), (CloseOutcome::Rehidden, "rehide"));
    }

    #[test]
    fn close_visible_at_open_hidden_now_restores_the_sessions_own_hide() {
        let (outcome, ran) = close(session_that_hid(), true);
        assert_eq!((outcome, ran), (CloseOutcome::Restored, "restore"));
    }

    #[test]
    fn close_visible_at_open_hidden_by_someone_else_leaves_it() {
        let (outcome, ran) = close(TaskbarSession::new(false), true);
        assert_eq!((outcome, ran), (CloseOutcome::NothingToDo, "none"));
    }

    #[test]
    fn pre_hidden_show_then_hide_then_close_leaves_it_hidden() {
        let mut s = TaskbarSession::new(true);
        s.toggle_with::<String>(
            true,
            || panic!("must not hide"),
            |r| {
                assert_eq!(r, None);
                Ok(())
            },
        );
        assert!(!s.is_hidden());
        s.toggle_with::<String>(
            false,
            || {
                Ok(TaskbarHideRestore::HoverReveal(
                    crate::win32::TaskbarAutohideState::AlwaysVisible,
                ))
            },
            |_| panic!("must not show"),
        );
        assert!(s.is_hidden());
        let (outcome, ran) = close(s, true);
        assert_eq!((outcome, ran), (CloseOutcome::NothingToDo, "none"));
    }

    #[test]
    fn close_reports_a_failed_action() {
        let cell = RefCell::new(session_that_hid());
        let outcome = restore_on_close(&cell, true, || Err("x"), |_| Err("boom"));
        assert_eq!(outcome, CloseOutcome::Failed("boom".into()));
        let cell = RefCell::new(TaskbarSession::new(true));
        let outcome = restore_on_close(&cell, false, || Err("boom"), |_| Err("x"));
        assert_eq!(outcome, CloseOutcome::Failed("boom".into()));
    }

    #[test]
    fn close_does_not_panic_or_act_when_the_session_is_busy() {
        let cell = RefCell::new(session_that_hid());
        let _held = cell.borrow_mut();
        let outcome = restore_on_close::<String>(
            &cell,
            true,
            || panic!("must not rehide"),
            |_| panic!("must not restore"),
        );
        assert_eq!(outcome, CloseOutcome::Busy);
    }

    #[test]
    fn toggle_follows_the_real_state_not_the_state_at_open() {
        let s = TaskbarSession::new(false);
        assert_eq!(s.toggle_action(false), TaskbarToggle::Hide);
        // Something else hid it meanwhile: the click shows it.
        assert_eq!(s.toggle_action(true), TaskbarToggle::Show(None));
    }

    #[test]
    fn toggle_shows_with_the_captured_state_after_the_session_hid_it() {
        assert_eq!(
            session_that_hid().toggle_action(true),
            TaskbarToggle::Show(Some(R))
        );
    }

    #[test]
    fn hiding_captures_the_state_and_clears_a_previous_error() {
        let mut s = TaskbarSession {
            error: Some("old".into()),
            ..TaskbarSession::default()
        };
        s.toggle_with::<String>(false, || Ok(R), |_| panic!("must not show"));
        assert!(s.is_hidden());
        assert_eq!(s.error, None);
    }

    #[test]
    fn a_failed_hide_reports_the_error_and_keeps_the_label() {
        let mut s = TaskbarSession::new(false);
        s.toggle_with(false, || Err("boom"), |_| panic!("must not show"));
        assert!(!s.is_hidden());
        let error = s.error.as_deref().unwrap();
        assert!(error.contains("hide") && error.contains("boom"), "{error}");
    }

    #[test]
    fn a_failed_show_reports_the_error_and_keeps_the_label() {
        let mut s = TaskbarSession::new(true);
        s.toggle_with(true, || panic!("must not hide"), |_| Err("boom"));
        assert!(s.is_hidden());
        let error = s.error.as_deref().unwrap();
        assert!(error.contains("show") && error.contains("boom"), "{error}");
    }

    #[test]
    fn a_failed_click_resyncs_the_label_to_the_real_state() {
        // The label said "visible", but the taskbar was hidden meanwhile.
        let mut s = TaskbarSession::new(false);
        s.toggle_with(true, || panic!("must not hide"), |_| Err("boom"));
        assert!(s.is_hidden());
    }
}
