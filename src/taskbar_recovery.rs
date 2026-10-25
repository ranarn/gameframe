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
use std::sync::{Mutex, OnceLock};

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

/// What GameFrame currently believes it has hidden: the in-memory half of the
/// bookkeeping. Pure state transitions only, no IO, so it is cheap to hold
/// behind a mutex and easy to test.
#[derive(Debug, Default)]
struct HideRecord {
    hidden: Option<TaskbarHideRestore>,
}

impl HideRecord {
    /// Records a hide. Returns the record as it was before if this changed it
    /// (the caller must then persist the new record), `None` if unchanged.
    ///
    /// While a hide is recorded a later one keeps the original, which holds
    /// the true prior state (a nested `HoverReveal` reads `AutoHidden` as its
    /// "prior" only because the first hide already set it). The exception is
    /// `FullyHidden` over a `HoverReveal`: recovery must then show the
    /// taskbar window, which a `HoverReveal` record would not do.
    fn hide(&mut self, restore: TaskbarHideRestore) -> Option<Option<TaskbarHideRestore>> {
        let previous = self.hidden;
        let next = match (previous, restore) {
            (None, _) => restore,
            (Some(TaskbarHideRestore::HoverReveal(_)), TaskbarHideRestore::FullyHidden) => restore,
            (Some(existing), _) => existing,
        };
        if previous == Some(next) {
            return None;
        }
        self.hidden = Some(next);
        Some(previous)
    }

    /// Forgets the hide. Any restore returns the taskbar to a state GameFrame
    /// is no longer responsible for, even a restore to the user's own
    /// auto-hide.
    fn restore(&mut self) {
        self.hidden = None;
    }
}

/// What the panic hook should restore. `in_memory` is `None` when the lock on
/// the in-memory record could not be taken (held by the panicking thread or
/// another one); the marker file in `dir` is then the source of truth.
fn panic_restore_target(in_memory: Option<Option<TaskbarHideRestore>>, dir: Option<&Path>) -> Option<TaskbarHideRestore> {
    match in_memory {
        Some(known) => known,
        None => match dir.map(read_marker_file) {
            Some(MarkerState::Technique(r)) => Some(r),
            _ => None,
        },
    }
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

/// Process-wide state. The record is only ever locked for in-memory
/// transitions (never across file IO), so the panic hook's `try_lock` fails
/// only in a tiny window. The marker directory sits outside that lock so the
/// hook can always reach the marker file when it does fail.
static RECORD: Mutex<HideRecord> = Mutex::new(HideRecord { hidden: None });
static MARKER_DIR: OnceLock<PathBuf> = OnceLock::new();
/// Serializes marker file writes and removals so they land in the order the
/// in-memory transitions happened. The panic hook never takes it.
static FILE_IO: Mutex<()> = Mutex::new(());

fn record() -> std::sync::MutexGuard<'static, HideRecord> {
    RECORD.lock().unwrap_or_else(|e| e.into_inner())
}

fn persist(restore: Option<TaskbarHideRestore>) {
    let Some(dir) = MARKER_DIR.get() else { return };
    let _io = FILE_IO.lock().unwrap_or_else(|e| e.into_inner());
    match restore {
        Some(r) => {
            if let Err(e) = write_marker_file(dir, r) {
                log!("could not write the taskbar marker: {e}");
            }
        }
        None => {
            if let Err(e) = clear_marker_file(dir) {
                log!("could not remove the taskbar marker: {e}");
            }
        }
    }
}

/// Sets the directory the marker file lives in. Until this is called nothing
/// is written to disk (in-memory tracking still works).
pub fn init(dir: &Path) {
    let _ = MARKER_DIR.set(dir.to_path_buf());
}

/// Undo handle for a [`mark_hidden`] call whose OS hide then failed.
#[must_use]
pub struct HideUndo(Option<TaskbarHideRestore>);

/// Records that GameFrame is about to hide the taskbar using `restore`'s
/// technique. Call this *before* the OS hide so a kill in between is covered.
/// See [`HideRecord::hide`] for how nested hides combine. A write failure is
/// logged, never propagated. Returns `Some` if the record changed; if the OS
/// hide then fails, pass it to [`undo_hide`].
pub fn mark_hidden(restore: TaskbarHideRestore) -> Option<HideUndo> {
    let (changed, now) = {
        let mut rec = record();
        let changed = rec.hide(restore);
        (changed, rec.hidden)
    };
    let previous = changed?;
    persist(now);
    Some(HideUndo(previous))
}

/// Rolls back a [`mark_hidden`] whose OS hide failed, so GameFrame does not
/// believe it hid a taskbar it did not.
pub fn undo_hide(undo: HideUndo) {
    let HideUndo(previous) = undo;
    record().hidden = previous;
    persist(previous);
}

/// Records that GameFrame restored the taskbar, clearing the record and the
/// marker. (A restore to the user's own auto-hide state clears it too: the
/// taskbar is then back in a state GameFrame is not responsible for.)
pub fn mark_restored() {
    record().restore();
    persist(None);
}

/// What GameFrame currently believes it has hidden, if anything.
pub fn currently_hidden() -> Option<TaskbarHideRestore> {
    record().hidden
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
    mark_restored();
}

/// Startup recovery: reads the marker, compares it with the real taskbar,
/// performs [`decide_recovery`]'s action and clears the marker. Call before
/// anything that can hide the taskbar. OS-facing; the decision is tested.
pub fn recover_on_startup(dir: &Path) {
    let marker = read_marker_file(dir);
    if marker == MarkerState::None {
        return;
    }
    let result = match win32::observe_taskbar() {
        Ok((visible, autohide)) => {
            let action = decide_recovery(marker, ObservedTaskbar { visible, autohide });
            log!("taskbar recovery: marker {marker:?} -> {action:?}");
            match action {
                RecoveryAction::Nothing => Ok(()),
                RecoveryAction::ShowTaskbar => win32::show_taskbar(),
                RecoveryAction::SetAppbarState(state) => {
                    win32::restore_taskbar(TaskbarHideRestore::HoverReveal(state))
                }
            }
        }
        Err(e) => Err(e),
    };
    // The marker is the only record of a stranded hide: keep it if the
    // taskbar could not be observed or repaired, so the next start retries.
    match result {
        Ok(()) => {
            mark_restored();
            if let Err(e) = clear_marker_file(dir) {
                log!("could not remove the taskbar marker: {e}");
            }
        }
        Err(e) => log!("taskbar recovery failed, keeping the marker: {e}"),
    }
}

/// Installs a panic hook that restores the taskbar and clears the marker
/// (the release build aborts after the hook, so no destructor would), then
/// defers to the previously installed hook.
///
/// Safe from any thread and non-panicking: it only `try_lock`s the record (if
/// that fails, e.g. the panicking thread holds it, the on-disk marker in the
/// lock-free `MARKER_DIR` is read instead), it never takes the file-IO lock,
/// and the restore is plain Win32 calls whose errors are ignored.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_for_panic();
        log!("panic: {info}");
        previous(info);
    }));
}

fn restore_for_panic() {
    let in_memory = match RECORD.try_lock() {
        Ok(rec) => Some(rec.hidden),
        Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner().hidden),
        Err(std::sync::TryLockError::WouldBlock) => None,
    };
    let dir = MARKER_DIR.get().map(PathBuf::as_path);
    if let Some(restore) = panic_restore_target(in_memory, dir) {
        let _ = win32::restore_taskbar_raw(restore);
        if let Some(dir) = dir {
            let _ = clear_marker_file(dir);
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
    fn a_restore_clears_the_record_even_when_it_returns_to_the_users_auto_hide() {
        let mut rec = HideRecord::default();
        rec.hide(HOVER_FROM_AUTO);
        rec.restore();
        assert_eq!(rec.hidden, None);
        // A later hide with a different technique is then recorded afresh.
        rec.hide(FULLY);
        assert_eq!(rec.hidden, Some(FULLY));
    }

    #[test]
    fn the_first_hide_wins_over_a_nested_hover_reveal() {
        let mut rec = HideRecord::default();
        assert_eq!(rec.hide(HOVER_FROM_VISIBLE), Some(None));
        // A nested hover-reveal sees AutoHidden as its "prior"; ignore it.
        assert_eq!(rec.hide(HOVER_FROM_AUTO), None);
        assert_eq!(rec.hidden, Some(HOVER_FROM_VISIBLE));
        assert_eq!(rec.hide(FULLY), Some(Some(HOVER_FROM_VISIBLE)));
    }

    #[test]
    fn fully_hidden_replaces_a_hover_reveal_record_but_not_the_reverse() {
        let mut rec = HideRecord::default();
        rec.hide(HOVER_FROM_VISIBLE);
        rec.hide(FULLY);
        assert_eq!(rec.hidden, Some(FULLY));
        assert_eq!(rec.hide(HOVER_FROM_VISIBLE), None);
        assert_eq!(rec.hidden, Some(FULLY));
    }

    #[test]
    fn the_panic_hook_falls_back_to_the_marker_file_when_the_lock_is_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        write_marker_file(tmp.path(), FULLY).unwrap();
        assert_eq!(panic_restore_target(None, Some(tmp.path())), Some(FULLY));
    }

    #[test]
    fn the_panic_hook_prefers_the_in_memory_record_and_ignores_an_unrelated_marker() {
        let tmp = tempfile::tempdir().unwrap();
        write_marker_file(tmp.path(), FULLY).unwrap();
        assert_eq!(panic_restore_target(Some(Some(HOVER_FROM_VISIBLE)), Some(tmp.path())), Some(HOVER_FROM_VISIBLE));
        assert_eq!(panic_restore_target(Some(None), Some(tmp.path())), None);
    }

    #[test]
    fn the_panic_hook_has_nothing_to_restore_without_a_readable_marker() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(panic_restore_target(None, Some(tmp.path())), None);
        assert_eq!(panic_restore_target(None, None), None);
        std::fs::write(marker_path(tmp.path()), "???").unwrap();
        assert_eq!(panic_restore_target(None, Some(tmp.path())), None);
    }
}
