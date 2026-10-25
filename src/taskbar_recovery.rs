//! Taskbar recovery after a crash or kill (issue #68).
//!
//! The release profile is `panic = "abort"`, so RAII guards never run on a
//! panic, and a killed process runs nothing at all. A taskbar hidden with
//! `FullyHidden` (`ShowWindow(SW_HIDE)`) would then stay invisible until
//! Explorer restarts. Three pieces close that gap:
//!
//! - A small **marker file** (`taskbar-hidden.marker`) in the config
//!   directory, written whenever GameFrame hides the taskbar ([`mark_hidden`])
//!   and removed whenever GameFrame restores it ([`mark_restored`]).
//! - A **startup decision** ([`recover_on_startup`]): the pure
//!   [`decide_recovery`] takes the parsed marker plus the taskbar's observed
//!   state and returns a [`RecoveryAction`], which is then performed.
//! - A **panic hook** ([`install_panic_hook`]) that restores the taskbar and
//!   clears the marker before the abort, then defers to the previous hook.
//!
//! Limitation: only the primary taskbar (`Shell_TrayWnd`) is handled;
//! secondary-monitor taskbars are not touched by GameFrame at all.

use crate::win32::{self, TaskbarAutohideState, TaskbarHideRestore};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const MARKER_FILE: &str = "taskbar-hidden.marker";

/// What the marker file said at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerState {
    /// No marker: GameFrame did not leave the taskbar hidden.
    None,
    /// A readable marker recording the hide technique.
    Technique(TaskbarHideRestore),
    /// A marker exists but its content is not understood.
    Unreadable,
}

/// The taskbar as observed at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedTaskbar {
    /// `IsWindowVisible(Shell_TrayWnd)`.
    pub visible: bool,
    /// The appbar auto-hide state.
    pub autohide: TaskbarAutohideState,
}

/// What startup should do about a possibly stranded hidden taskbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    Nothing,
    /// Make the taskbar window visible again (`ShowWindow(SW_SHOW)`).
    ShowTaskbar,
    /// Write this appbar auto-hide state back.
    SetAppbarState(TaskbarAutohideState),
}

/// Serializes a hide technique to the marker file's text.
pub fn serialize_marker(restore: TaskbarHideRestore) -> String {
    match restore {
        TaskbarHideRestore::FullyHidden => "fully-hidden\n".to_string(),
        TaskbarHideRestore::HoverReveal(TaskbarAutohideState::AlwaysVisible) => {
            "hover-reveal always-visible\n".to_string()
        }
        TaskbarHideRestore::HoverReveal(TaskbarAutohideState::AutoHidden) => "hover-reveal auto-hidden\n".to_string(),
    }
}

/// Parses marker text; `None` if it is not one of the serialized forms.
pub fn parse_marker(text: &str) -> Option<TaskbarHideRestore> {
    match text.trim() {
        "fully-hidden" => Some(TaskbarHideRestore::FullyHidden),
        "hover-reveal always-visible" => {
            Some(TaskbarHideRestore::HoverReveal(TaskbarAutohideState::AlwaysVisible))
        }
        "hover-reveal auto-hidden" => Some(TaskbarHideRestore::HoverReveal(TaskbarAutohideState::AutoHidden)),
        _ => None,
    }
}

/// Decides what startup should do, given the marker and the taskbar as
/// observed. Pure.
///
/// - No marker: nothing.
/// - `FullyHidden`: show the taskbar if it is hidden, else nothing.
/// - `HoverReveal(prior)`: write `prior` back only if the taskbar is now
///   auto-hidden and `prior` was not (if the user changed it themselves in
///   the meantime, or it was auto-hide to begin with, leave it).
/// - Unreadable marker: if the taskbar window is hidden, show it (the
///   technique is unknown, so the only safe guess is the visible state); a
///   visible taskbar is left alone, since its auto-hide may be the user's own.
pub fn decide_recovery(marker: MarkerState, observed: ObservedTaskbar) -> RecoveryAction {
    match marker {
        MarkerState::None => RecoveryAction::Nothing,
        MarkerState::Technique(TaskbarHideRestore::FullyHidden) | MarkerState::Unreadable => {
            if observed.visible { RecoveryAction::Nothing } else { RecoveryAction::ShowTaskbar }
        }
        MarkerState::Technique(TaskbarHideRestore::HoverReveal(prior)) => {
            if observed.autohide == TaskbarAutohideState::AutoHidden && prior != TaskbarAutohideState::AutoHidden {
                RecoveryAction::SetAppbarState(prior)
            } else {
                RecoveryAction::Nothing
            }
        }
    }
}

/// True if restoring to `restore` leaves the taskbar hidden anyway (a
/// `HoverReveal` whose prior state was already auto-hide), in which case the
/// marker must stay: something else may still be relying on the hide.
fn restore_keeps_taskbar_hidden(restore: TaskbarHideRestore) -> bool {
    restore == TaskbarHideRestore::HoverReveal(TaskbarAutohideState::AutoHidden)
}

fn marker_path(dir: &Path) -> PathBuf {
    dir.join(MARKER_FILE)
}

fn write_marker_file(dir: &Path, restore: TaskbarHideRestore) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(marker_path(dir), serialize_marker(restore))
}

fn read_marker_file(dir: &Path) -> MarkerState {
    match std::fs::read_to_string(marker_path(dir)) {
        Ok(text) => parse_marker(&text).map_or(MarkerState::Unreadable, MarkerState::Technique),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => MarkerState::None,
        Err(_) => MarkerState::Unreadable,
    }
}

fn clear_marker_file(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(marker_path(dir)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Process-wide marker bookkeeping: where the file lives and what GameFrame
/// currently believes it has hidden (for the panic hook and tray Exit).
struct Tracker {
    dir: Option<PathBuf>,
    hidden: Option<TaskbarHideRestore>,
}

static TRACKER: Mutex<Tracker> = Mutex::new(Tracker { dir: None, hidden: None });

/// Sets the directory the marker file lives in. Until this is called nothing
/// is written to disk (in-memory tracking still works).
pub fn init(dir: &Path) {
    TRACKER.lock().unwrap_or_else(|e| e.into_inner()).dir = Some(dir.to_path_buf());
}

/// Records that GameFrame is about to hide the taskbar using `restore`'s
/// technique. Call this *before* the OS hide so a kill in between is covered.
/// The first hide wins: while a hide is already recorded, a later one (e.g.
/// the zone editor over an armed session) keeps the original record, which
/// holds the true prior state. A write failure is logged, never propagated.
pub fn mark_hidden(restore: TaskbarHideRestore) {
    let mut tracker = TRACKER.lock().unwrap_or_else(|e| e.into_inner());
    if tracker.hidden.is_some() {
        return;
    }
    tracker.hidden = Some(restore);
    if let Some(dir) = &tracker.dir
        && let Err(e) = write_marker_file(dir, restore)
    {
        log!("could not write the taskbar marker: {e}");
    }
}

/// Records that GameFrame restored the taskbar to `restore`'s state, clearing
/// the marker -- unless that restore leaves the taskbar hidden anyway.
pub fn mark_restored(restore: TaskbarHideRestore) {
    if restore_keeps_taskbar_hidden(restore) {
        return;
    }
    clear_tracked();
}

fn clear_tracked() {
    let mut tracker = TRACKER.lock().unwrap_or_else(|e| e.into_inner());
    tracker.hidden = None;
    if let Some(dir) = &tracker.dir
        && let Err(e) = clear_marker_file(dir)
    {
        log!("could not remove the taskbar marker: {e}");
    }
}

/// What GameFrame currently believes it has hidden, if anything.
pub fn currently_hidden() -> Option<TaskbarHideRestore> {
    TRACKER.lock().unwrap_or_else(|e| e.into_inner()).hidden
}

/// Graceful-exit safety net: if the taskbar is still hidden by GameFrame
/// (e.g. the manual hotkey toggle), restore it, which also clears the marker.
pub fn restore_on_exit() {
    if let Some(restore) = currently_hidden()
        && let Err(e) = win32::restore_taskbar(restore)
    {
        log!("failed to restore taskbar on exit: {e}");
    }
    // Whatever happened above, a graceful exit leaves no marker.
    clear_tracked();
}

/// Startup recovery: reads the marker, compares it with the real taskbar,
/// performs [`decide_recovery`]'s action and clears the marker. Call before
/// anything that can hide the taskbar. OS-facing; the decision is tested.
pub fn recover_on_startup(dir: &Path) {
    let marker = read_marker_file(dir);
    if marker == MarkerState::None {
        return;
    }
    let action = match win32::observe_taskbar() {
        Ok((visible, autohide)) => decide_recovery(marker, ObservedTaskbar { visible, autohide }),
        // No taskbar window (e.g. Explorer is not up): nothing to repair.
        Err(_) => RecoveryAction::Nothing,
    };
    log!("taskbar recovery: marker {marker:?} -> {action:?}");
    let result = match action {
        RecoveryAction::Nothing => Ok(()),
        RecoveryAction::ShowTaskbar => win32::show_taskbar(),
        RecoveryAction::SetAppbarState(state) => win32::restore_taskbar(TaskbarHideRestore::HoverReveal(state)),
    };
    if let Err(e) = result {
        log!("taskbar recovery failed: {e}");
    }
    clear_tracked();
    if let Err(e) = clear_marker_file(dir) {
        log!("could not remove the taskbar marker: {e}");
    }
}

/// Installs a panic hook that restores the taskbar and clears the marker
/// (the release build aborts after the hook, so no destructor would), then
/// defers to the previously installed hook.
///
/// Safe from any thread and non-panicking: it only `try_lock`s the tracker (a
/// panic while the lock is held falls back to the on-disk marker), and the
/// restore is plain Win32 calls whose errors are ignored.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_for_panic();
        log!("panic: {info}");
        previous(info);
    }));
}

fn restore_for_panic() {
    let (restore, dir) = match TRACKER.try_lock() {
        Ok(tracker) => (tracker.hidden, tracker.dir.clone()),
        Err(std::sync::TryLockError::Poisoned(p)) => {
            let tracker = p.into_inner();
            (tracker.hidden, tracker.dir.clone())
        }
        // Held by the panicking thread itself: use the marker file instead.
        Err(std::sync::TryLockError::WouldBlock) => (None, None),
    };
    let restore = restore.or_else(|| match dir.as_deref().map(read_marker_file) {
        Some(MarkerState::Technique(r)) => Some(r),
        _ => None,
    });
    if let Some(restore) = restore {
        let _ = win32::restore_taskbar_raw(restore);
        if let Some(dir) = dir {
            let _ = clear_marker_file(&dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TaskbarAutohideState::{AlwaysVisible, AutoHidden};

    const FULLY: TaskbarHideRestore = TaskbarHideRestore::FullyHidden;
    const HOVER_FROM_VISIBLE: TaskbarHideRestore = TaskbarHideRestore::HoverReveal(AlwaysVisible);
    const HOVER_FROM_AUTO: TaskbarHideRestore = TaskbarHideRestore::HoverReveal(AutoHidden);

    fn observed(visible: bool, autohide: TaskbarAutohideState) -> ObservedTaskbar {
        ObservedTaskbar { visible, autohide }
    }

    #[test]
    fn no_marker_does_nothing_even_if_the_taskbar_is_hidden() {
        assert_eq!(decide_recovery(MarkerState::None, observed(false, AutoHidden)), RecoveryAction::Nothing);
    }

    #[test]
    fn fully_hidden_marker_with_a_hidden_taskbar_shows_it() {
        let action = decide_recovery(MarkerState::Technique(FULLY), observed(false, AlwaysVisible));
        assert_eq!(action, RecoveryAction::ShowTaskbar);
    }

    #[test]
    fn fully_hidden_marker_with_a_visible_taskbar_does_nothing() {
        let action = decide_recovery(MarkerState::Technique(FULLY), observed(true, AlwaysVisible));
        assert_eq!(action, RecoveryAction::Nothing);
    }

    #[test]
    fn hover_reveal_marker_with_an_auto_hidden_taskbar_writes_back_the_prior_state() {
        let action = decide_recovery(MarkerState::Technique(HOVER_FROM_VISIBLE), observed(true, AutoHidden));
        assert_eq!(action, RecoveryAction::SetAppbarState(AlwaysVisible));
    }

    #[test]
    fn hover_reveal_marker_with_an_already_visible_taskbar_does_nothing() {
        let action = decide_recovery(MarkerState::Technique(HOVER_FROM_VISIBLE), observed(true, AlwaysVisible));
        assert_eq!(action, RecoveryAction::Nothing);
    }

    #[test]
    fn hover_reveal_marker_whose_prior_state_was_auto_hide_does_nothing() {
        let action = decide_recovery(MarkerState::Technique(HOVER_FROM_AUTO), observed(true, AutoHidden));
        assert_eq!(action, RecoveryAction::Nothing);
    }

    #[test]
    fn unreadable_marker_with_a_hidden_taskbar_shows_it() {
        let action = decide_recovery(MarkerState::Unreadable, observed(false, AlwaysVisible));
        assert_eq!(action, RecoveryAction::ShowTaskbar);
    }

    #[test]
    fn unreadable_marker_with_a_visible_taskbar_does_nothing() {
        assert_eq!(decide_recovery(MarkerState::Unreadable, observed(true, AutoHidden)), RecoveryAction::Nothing);
        assert_eq!(decide_recovery(MarkerState::Unreadable, observed(true, AlwaysVisible)), RecoveryAction::Nothing);
    }

    #[test]
    fn every_technique_round_trips_through_the_marker_text() {
        for restore in [FULLY, HOVER_FROM_VISIBLE, HOVER_FROM_AUTO] {
            assert_eq!(parse_marker(&serialize_marker(restore)), Some(restore));
        }
    }

    #[test]
    fn garbage_and_empty_text_do_not_parse() {
        assert_eq!(parse_marker(""), None);
        assert_eq!(parse_marker("hover-reveal sideways"), None);
        assert_eq!(parse_marker("\u{0}\u{1}binary"), None);
    }

    #[test]
    fn marker_file_round_trips_and_clears() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_marker_file(tmp.path()), MarkerState::None);

        write_marker_file(tmp.path(), HOVER_FROM_VISIBLE).unwrap();
        assert_eq!(read_marker_file(tmp.path()), MarkerState::Technique(HOVER_FROM_VISIBLE));

        clear_marker_file(tmp.path()).unwrap();
        assert_eq!(read_marker_file(tmp.path()), MarkerState::None);
        // Clearing an absent marker is not an error.
        clear_marker_file(tmp.path()).unwrap();
    }

    #[test]
    fn a_corrupt_marker_file_reads_as_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(marker_path(tmp.path()), "???").unwrap();
        assert_eq!(read_marker_file(tmp.path()), MarkerState::Unreadable);
    }

    #[test]
    fn a_restore_to_auto_hide_keeps_the_marker_but_others_clear_it() {
        assert!(restore_keeps_taskbar_hidden(HOVER_FROM_AUTO));
        assert!(!restore_keeps_taskbar_hidden(HOVER_FROM_VISIBLE));
        assert!(!restore_keeps_taskbar_hidden(FULLY));
    }
}
