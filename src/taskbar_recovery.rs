//! Taskbar recovery after a crash or kill.
//!
//! The release profile is `panic = "abort"`, so RAII guards never run on a
//! panic, and a killed process runs nothing at all. A taskbar hidden with
//! `FullyHidden` (`ShowWindow(SW_HIDE)`) would then stay invisible until
//! Explorer restarts. Three pieces close that gap:
//!
//! - A small **marker file** (`taskbar-hidden.marker`) in the config
//!   directory, written whenever GameFrame hides the taskbar ([`mark_hidden`])
//!   and updated whenever GameFrame restores it ([`mark_restored`]); removed
//!   once no technique remains. The two techniques are tracked independently,
//!   so restoring one leaves the other recorded. Neither is reference counted
//!   here: several armed windows share one record entry per technique.
//! - A **startup decision** ([`recover_on_startup`]): the pure
//!   [`decide_recovery`] takes the parsed marker plus the taskbar's observed
//!   state and returns a [`RecoveryAction`], which is then performed.
//! - A **panic hook** ([`install_panic_hook`]) that restores the taskbar and
//!   clears the marker before the abort, then defers to the previous hook.
//!
//! Limitation: only the primary taskbar (`Shell_TrayWnd`) is handled;
//! secondary-monitor taskbars are not touched by GameFrame at all.

use crate::win32::{self, ShowKind, TaskbarAutohideState, TaskbarHideRestore};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

pub const MARKER_FILE: &str = "taskbar-hidden.marker";

/// What the marker file said at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerState {
    /// No marker: GameFrame did not leave the taskbar hidden.
    None,
    /// A readable marker recording the hide technique.
    Technique(TaskbarHideRestore),
    /// Both techniques are in force: `FullyHidden` plus a `HoverReveal` that
    /// goes back to `hover_prior`.
    Combined { hover_prior: TaskbarAutohideState },
    /// A marker exists but its content is not understood.
    Unreadable,
}

/// The taskbar as observed at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedTaskbar {
    /// `IsWindowVisible(Shell_TrayWnd)`.
    pub visible: bool,
    /// The appbar auto-hide state; None when it could not be read (a hung
    /// Explorer), which is not the same as AlwaysVisible.
    pub autohide: Option<TaskbarAutohideState>,
}

/// What startup should do about a possibly stranded hidden taskbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    Nothing,
    /// Make the taskbar window visible again (`ShowWindow(SW_SHOW)`).
    ShowTaskbar,
    /// Write this appbar auto-hide state back.
    SetAppbarState(TaskbarAutohideState),
    /// The taskbar's state needed for the decision is unknown (Explorer is
    /// hung): neither act nor clear the marker, recover later.
    Undecidable,
}

/// The restore paths that do not go through the normal in-memory bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawRestorePath {
    /// Startup recovery of a marker a kill or crash left behind.
    Startup,
    /// The panic hook, which must stay minimal (decided in #52).
    PanicHook,
}

/// Which show a [`RawRestorePath`] uses. Startup recovery has the time and the
/// locks for the full show (a bare `SW_SHOW` can leave an auto-hidden taskbar
/// collapsed); the panic hook must not do more than one plain call.
pub fn show_kind_for(path: RawRestorePath) -> ShowKind {
    match path {
        RawRestorePath::Startup => ShowKind::Full,
        RawRestorePath::PanicHook => ShowKind::Bare,
    }
}

/// Whether a graceful exit must show the taskbar although the record had
/// nothing (left) to restore: this run hid it, and it is still not visible.
/// `visible` is `None` when the taskbar could not be observed.
pub fn needs_exit_safety_show(hid_this_run: bool, visible: Option<bool>) -> bool {
    hid_this_run && visible == Some(false)
}

/// The appbar flags at session start and at session end, if they differ (a
/// user toggled Windows' auto-hide or always-on-top while GameFrame had the
/// taskbar hidden). `None` when there was no session start or nothing changed.
pub fn appbar_flags_change(at_start: Option<u32>, at_end: u32) -> Option<(u32, u32)> {
    at_start
        .filter(|&start| start != at_end)
        .map(|start| (start, at_end))
}

/// Serializes a hide technique to the marker file's text.
pub fn serialize_marker(restore: TaskbarHideRestore) -> String {
    match restore {
        TaskbarHideRestore::FullyHidden => "fully-hidden\n".to_string(),
        TaskbarHideRestore::HoverReveal(TaskbarAutohideState::AlwaysVisible) => {
            "hover-reveal always-visible\n".to_string()
        }
        TaskbarHideRestore::HoverReveal(TaskbarAutohideState::AutoHidden) => {
            "hover-reveal auto-hidden\n".to_string()
        }
    }
}

/// Parses marker text; `None` if it is not one of the serialized forms.
pub fn parse_marker(text: &str) -> Option<TaskbarHideRestore> {
    match text.trim() {
        "fully-hidden" => Some(TaskbarHideRestore::FullyHidden),
        "hover-reveal always-visible" => Some(TaskbarHideRestore::HoverReveal(
            TaskbarAutohideState::AlwaysVisible,
        )),
        "hover-reveal auto-hidden" => Some(TaskbarHideRestore::HoverReveal(
            TaskbarAutohideState::AutoHidden,
        )),
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
/// - `Combined`: callers split it into its techniques first (see
///   `recovery_inputs`); decided here as `Nothing`.
/// - HoverReveal(prior) with an unknown auto-hide state: Undecidable.
/// - Unreadable marker: if the taskbar window is hidden, show it (the
///   technique is unknown, so the only safe guess is the visible state); a
///   visible taskbar is left alone, since its auto-hide may be the user's own.
pub fn decide_recovery(marker: MarkerState, observed: ObservedTaskbar) -> RecoveryAction {
    match marker {
        MarkerState::None => RecoveryAction::Nothing,
        MarkerState::Technique(TaskbarHideRestore::FullyHidden) | MarkerState::Unreadable => {
            if observed.visible {
                RecoveryAction::Nothing
            } else {
                RecoveryAction::ShowTaskbar
            }
        }
        MarkerState::Combined { .. } => RecoveryAction::Nothing,
        MarkerState::Technique(TaskbarHideRestore::HoverReveal(prior)) => match observed.autohide {
            None => RecoveryAction::Undecidable,
            Some(TaskbarAutohideState::AutoHidden) if prior != TaskbarAutohideState::AutoHidden => {
                RecoveryAction::SetAppbarState(prior)
            }
            Some(_) => RecoveryAction::Nothing,
        },
    }
}

/// What GameFrame currently believes it has hidden: the in-memory half of the
/// bookkeeping. The two techniques are tracked independently, so restoring one
/// clears only its own part while the other still hides the taskbar. Pure state
/// transitions only, no IO, so it is cheap to hold behind a mutex and easy to
/// test.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct HideRecord {
    /// The taskbar window is hidden (`FullyHidden`).
    fully: bool,
    /// The appbar auto-hide was forced on (`HoverReveal`); holds the state to
    /// go back to.
    hover: Option<TaskbarAutohideState>,
}

impl HideRecord {
    const EMPTY: HideRecord = HideRecord {
        fully: false,
        hover: None,
    };

    /// Records a hide. Returns the record as it was before if this changed it
    /// (the caller must then persist the new record), `None` if unchanged.
    ///
    /// A nested `HoverReveal` keeps the first one's prior state, which is the
    /// true one (a later hide reads `AutoHidden` as its "prior" only because
    /// the first hide already set it).
    fn hide(&mut self, restore: TaskbarHideRestore) -> Option<HideRecord> {
        let previous = *self;
        match restore {
            TaskbarHideRestore::FullyHidden => self.fully = true,
            TaskbarHideRestore::HoverReveal(prior) => {
                self.hover.get_or_insert(prior);
            }
        }
        (previous != *self).then_some(previous)
    }

    /// Forgets the hide of `restore`'s technique only. Any restore returns that
    /// part of the taskbar to a state GameFrame is no longer responsible for,
    /// even a restore to the user's own auto-hide.
    fn restore(&mut self, restore: TaskbarHideRestore) {
        match restore {
            TaskbarHideRestore::FullyHidden => self.fully = false,
            TaskbarHideRestore::HoverReveal(_) => self.hover = None,
        }
    }

    fn is_empty(&self) -> bool {
        *self == Self::EMPTY
    }

    /// The techniques still in force, in restore order.
    fn techniques(&self) -> Vec<TaskbarHideRestore> {
        let mut v = Vec::new();
        if let Some(prior) = self.hover {
            v.push(TaskbarHideRestore::HoverReveal(prior));
        }
        if self.fully {
            v.push(TaskbarHideRestore::FullyHidden);
        }
        v
    }

    /// The marker file text: one line per technique. Empty record: `None`
    /// (no marker file).
    fn to_marker_text(self) -> Option<String> {
        (!self.is_empty()).then(|| {
            let mut s = String::new();
            if self.fully {
                s.push_str(&serialize_marker(TaskbarHideRestore::FullyHidden));
            }
            if let Some(prior) = self.hover {
                s.push_str(&serialize_marker(TaskbarHideRestore::HoverReveal(prior)));
            }
            s
        })
    }
}

/// Parses the marker file text (one technique per line); `None` if empty or
/// any line is not understood.
fn parse_record(text: &str) -> Option<HideRecord> {
    let mut rec = HideRecord::EMPTY;
    let mut any = false;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        rec.hide(parse_marker(line)?);
        any = true;
    }
    any.then_some(rec)
}

/// The techniques a marker asks startup to undo.
fn marker_techniques(marker: MarkerState) -> Vec<TaskbarHideRestore> {
    match marker {
        MarkerState::None | MarkerState::Unreadable => Vec::new(),
        MarkerState::Technique(t) => vec![t],
        MarkerState::Combined { hover_prior } => {
            vec![
                TaskbarHideRestore::HoverReveal(hover_prior),
                TaskbarHideRestore::FullyHidden,
            ]
        }
    }
}

/// The single-technique (or unreadable) markers whose decisions together
/// recover `marker`.
fn recovery_inputs(marker: MarkerState) -> Vec<MarkerState> {
    match marker {
        MarkerState::None => Vec::new(),
        MarkerState::Unreadable => vec![MarkerState::Unreadable],
        other => marker_techniques(other)
            .into_iter()
            .map(MarkerState::Technique)
            .collect(),
    }
}

/// What the panic hook should restore. `in_memory` is `None` when the lock on
/// the in-memory record could not be taken (held by the panicking thread or
/// another one); the marker file in `dir` is then the source of truth.
fn panic_restore_target(
    in_memory: Option<HideRecord>,
    dir: Option<&Path>,
) -> Vec<TaskbarHideRestore> {
    match in_memory {
        Some(known) => known.techniques(),
        None => dir
            .map(read_marker_file)
            .map(|marker| match marker {
                // Technique unknown: the bare show is harmless on a visible
                // taskbar, and the hook cannot afford to observe first.
                MarkerState::Unreadable => vec![TaskbarHideRestore::FullyHidden],
                other => marker_techniques(other),
            })
            .unwrap_or_default(),
    }
}

fn marker_path(dir: &Path) -> PathBuf {
    dir.join(MARKER_FILE)
}

fn write_record_file(dir: &Path, text: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    // Write a temp file next to the marker, then rename over it, so the panic
    // hook (which reads the file without the record lock) sees the old or the
    // new content, never a truncated one. The fixed temp name is safe because
    // every writer holds the `RECORD` lock. `sync_all` before the rename keeps
    // a power loss from leaving a renamed-but-empty marker.
    let tmp = tmp_marker_path(dir);
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut file, text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, marker_path(dir))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The temp file [`write_record_file`] renames over the marker.
fn tmp_marker_path(dir: &Path) -> std::path::PathBuf {
    dir.join(format!("{MARKER_FILE}.tmp"))
}

fn read_marker_file(dir: &Path) -> MarkerState {
    match std::fs::read_to_string(marker_path(dir)) {
        Ok(text) => match parse_record(&text) {
            None => MarkerState::Unreadable,
            Some(HideRecord {
                fully: true,
                hover: Some(hover_prior),
            }) => MarkerState::Combined { hover_prior },
            Some(HideRecord {
                fully: true,
                hover: None,
            }) => MarkerState::Technique(TaskbarHideRestore::FullyHidden),
            Some(HideRecord {
                fully: false,
                hover: Some(p),
            }) => MarkerState::Technique(TaskbarHideRestore::HoverReveal(p)),
            Some(_) => MarkerState::Unreadable,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => MarkerState::None,
        Err(_) => MarkerState::Unreadable,
    }
}

fn clear_marker_file(dir: &Path) -> std::io::Result<()> {
    // A temp file left by a kill between write and rename is stale too.
    let _ = std::fs::remove_file(tmp_marker_path(dir));
    match std::fs::remove_file(marker_path(dir)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Process-wide state. `RECORD` is held across the marker file write or
/// removal, so file writes land in exactly the order the in-memory transitions
/// happened (no stale marker after a graceful restore). The panic hook only
/// `try_lock`s it; the marker directory sits outside that lock so the hook can
/// always reach the marker file when the lock is busy.
static RECORD: Mutex<HideRecord> = Mutex::new(HideRecord::EMPTY);
static MARKER_DIR: OnceLock<PathBuf> = OnceLock::new();
/// Set when startup recovery failed and the marker was kept. While set the
/// marker file is left untouched (a later hide or the exit path would
/// otherwise destroy the only record of a stranded hide); recovery is retried
/// before the next hide and at exit.
static RECOVERY_PENDING: AtomicBool = AtomicBool::new(false);
/// Set once this run has hidden the taskbar window (`FullyHidden`), so a
/// graceful exit knows a still-hidden taskbar is its doing. Never reset on
/// purpose: any later exit that finds the taskbar not visible shows it.
static HID_FULLY_THIS_RUN: AtomicBool = AtomicBool::new(false);
/// The appbar flags read when the current `FullyHidden` session started.
static SESSION_START_FLAGS: Mutex<Option<u32>> = Mutex::new(None);

/// Remembers the appbar flags as a `FullyHidden` session starts, for
/// [`finish_session_flags`].
pub fn note_session_start_flags(flags: u32) {
    *SESSION_START_FLAGS
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(flags);
}

/// When a marked `FullyHidden` hide is undone through `restore_marked` (the
/// hotkey show, a session end or exit -- never startup recovery or the panic
/// hook): logs both values if the appbar flags differ from the session start
/// (Windows' auto-hide was toggled meanwhile).
/// Log only; the normal show happens regardless.
pub fn finish_session_flags(flags_now: u32) {
    let start = SESSION_START_FLAGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some((before, after)) = appbar_flags_change(start, flags_now) {
        log!(
            "taskbar appbar flags changed while hidden: {before:#x} at session start, {after:#x} at session end"
        );
    }
}

fn record() -> std::sync::MutexGuard<'static, HideRecord> {
    RECORD.lock().unwrap_or_else(|e| e.into_inner())
}

/// Writes (or removes) the marker for `rec`. The caller holds the `RECORD`
/// lock, which is what orders the file operations.
fn persist(rec: HideRecord) {
    if RECOVERY_PENDING.load(Ordering::SeqCst) {
        return;
    }
    let Some(dir) = MARKER_DIR.get() else { return };
    match rec.to_marker_text() {
        Some(text) => {
            if let Err(e) = write_record_file(dir, &text) {
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
pub struct HideUndo(HideRecord);

/// Records that GameFrame is about to hide the taskbar using `restore`'s
/// technique. Call this *before* the OS hide so a kill in between is covered.
/// See [`HideRecord::hide`] for how nested hides combine. A write failure is
/// logged, never propagated. Returns `Some` if the record changed; if the OS
/// hide then fails, pass it to [`undo_hide`].
pub fn mark_hidden(restore: TaskbarHideRestore) -> Option<HideUndo> {
    retry_pending_recovery();
    if restore == TaskbarHideRestore::FullyHidden {
        HID_FULLY_THIS_RUN.store(true, Ordering::SeqCst);
    }
    let mut rec = record();
    let previous = rec.hide(restore)?;
    persist(*rec);
    Some(HideUndo(previous))
}

/// Rolls back a [`mark_hidden`] whose OS hide failed, so GameFrame does not
/// believe it hid a taskbar it did not.
pub fn undo_hide(undo: HideUndo) {
    let HideUndo(previous) = undo;
    let mut rec = record();
    *rec = previous;
    persist(*rec);
}

/// Records that GameFrame restored the taskbar from `restore`'s technique,
/// clearing that part of the record; the marker is removed once no technique
/// remains. (A restore to the user's own auto-hide state clears it too: the
/// taskbar is then back in a state GameFrame is not responsible for.)
fn mark_restored(restore: TaskbarHideRestore) {
    let mut rec = record();
    rec.restore(restore);
    persist(*rec);
}

/// What GameFrame currently believes it has hidden, if anything.
fn currently_hidden() -> HideRecord {
    *record()
}

/// Graceful-exit safety net: if the taskbar is still hidden by GameFrame
/// (e.g. the manual hotkey toggle), restore it, which also clears the marker.
/// A marker kept by a failed startup recovery is left alone unless the retry
/// now succeeds.
///
/// Call `crate::taskbar::restore_all_for_exit` instead: it also restores the
/// armed session first, which a direct call here would skip.
pub(crate) fn restore_on_exit() {
    retry_pending_recovery();
    for restore in currently_hidden().techniques() {
        if let Err(e) = restore_marked(restore) {
            log!("failed to restore taskbar on exit: {e}");
        }
    }
    // The record can drift from reality (e.g. Windows' auto-hide toggled while
    // hidden): if this run hid the taskbar and it is still not visible, show it.
    let visible = os_taskbar_visible();
    if needs_exit_safety_show(HID_FULLY_THIS_RUN.load(Ordering::SeqCst), visible) {
        log!("taskbar still hidden at exit although this run hid it; showing it");
        if let Err(e) = restore_marked(TaskbarHideRestore::FullyHidden) {
            log!("failed to show the taskbar at exit: {e}");
        }
    }
    // Successful restores cleared their marks above; a technique whose
    // restore failed (e.g. Explorer hung) keeps its mark so the next start
    // can still recover it. `persist` also keeps one that recovery owes.
    let rec = record();
    persist(*rec);
}

/// Restores the technique `restore` names and clears its mark. The single
/// marked restore: `crate::taskbar::restore_taskbar`/`show_taskbar` and the
/// exit path all go through it.
pub(crate) fn restore_marked(restore: TaskbarHideRestore) -> Result<(), win32::Win32Error> {
    restore_marked_with(restore, os_show_full, os_set_appbar, mark_restored)
}

/// The full show that ends a `FullyHidden` session: this caller, not
/// `show_taskbar_full`, ends the session for the flags-changed log, so a
/// startup-recovery show can never consume the session-start flags.
#[cfg(not(test))]
fn os_show_full() -> Result<(), win32::Win32Error> {
    win32::show_taskbar_full().map(finish_session_flags)
}

#[cfg(not(test))]
fn os_set_appbar(state: TaskbarAutohideState) -> Result<(), win32::Win32Error> {
    win32::set_appbar_state(state)
}

/// The visibility the exit safety net sees; `None` when unobservable.
#[cfg(not(test))]
fn os_taskbar_visible() -> Option<bool> {
    win32::observe_taskbar().ok().map(|(visible, _)| visible)
}

/// Test builds fake the OS-facing steps (no-ops) so the real exit path
/// (`restore_on_exit`) can run against a temp marker directory, unless an
/// `#[ignore]`d real-taskbar test opts in here: those hide the real taskbar
/// and need the real show to bring it back.
#[cfg(test)]
pub(crate) static REAL_OS_IN_TESTS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
fn real_os_in_tests() -> bool {
    REAL_OS_IN_TESTS.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
fn os_show_full() -> Result<(), win32::Win32Error> {
    if real_os_in_tests() {
        win32::show_taskbar_full().map(finish_session_flags)
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn os_set_appbar(state: TaskbarAutohideState) -> Result<(), win32::Win32Error> {
    if real_os_in_tests() {
        win32::set_appbar_state(state)
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn os_taskbar_visible() -> Option<bool> {
    if real_os_in_tests() {
        win32::observe_taskbar().ok().map(|(visible, _)| visible)
    } else {
        None
    }
}

/// Core of [`restore_marked`] with the OS steps and the marker bookkeeping
/// injected, so the dispatch-then-clear contract is testable: pick the OS step
/// for `restore`, and only if it succeeds, clear that technique's mark.
fn restore_marked_with<E>(
    restore: TaskbarHideRestore,
    show_full: impl FnOnce() -> Result<(), E>,
    set_appbar: impl FnOnce(TaskbarAutohideState) -> Result<(), E>,
    mark: impl FnOnce(TaskbarHideRestore),
) -> Result<(), E> {
    match restore {
        TaskbarHideRestore::FullyHidden => show_full()?,
        TaskbarHideRestore::HoverReveal(state) => set_appbar(state)?,
    }
    mark(restore);
    Ok(())
}

/// Performs the recovery for `marker` against the real taskbar and clears the
/// marker. Uses the raw restore calls: no in-memory bookkeeping is involved.
fn recover_marker(dir: &Path, marker: MarkerState) -> Result<(), win32::Win32Error> {
    let (visible, autohide) = win32::observe_taskbar()?;
    let observed = ObservedTaskbar { visible, autohide };
    let mut undecidable = false;
    for input in recovery_inputs(marker) {
        let action = decide_recovery(input, observed);
        log!("taskbar recovery: marker {input:?} -> {action:?}");
        match action {
            RecoveryAction::Nothing => {}
            RecoveryAction::Undecidable => undecidable = true,
            RecoveryAction::ShowTaskbar => win32::restore_taskbar_raw(
                TaskbarHideRestore::FullyHidden,
                show_kind_for(RawRestorePath::Startup),
            )?,
            RecoveryAction::SetAppbarState(state) => win32::restore_taskbar_raw(
                TaskbarHideRestore::HoverReveal(state),
                show_kind_for(RawRestorePath::Startup),
            )?,
        }
    }
    if undecidable {
        // Keep the marker: the state to decide on is unknown until Explorer
        // answers again.
        return Err(win32::Win32Error::WindowHung);
    }
    RECOVERY_PENDING.store(false, Ordering::SeqCst);
    if let Err(e) = clear_marker_file(dir) {
        log!("could not remove the taskbar marker: {e}");
    }
    Ok(())
}

/// Startup recovery: reads the marker, compares it with the real taskbar,
/// performs [`decide_recovery`]'s action and clears the marker. Call before
/// anything that can hide the taskbar. OS-facing; the decision is tested.
pub fn recover_on_startup(dir: &Path) {
    let marker = read_marker_file(dir);
    if marker == MarkerState::None {
        return;
    }
    // The marker is the only record of a stranded hide: keep it if the
    // taskbar could not be observed or repaired (e.g. `Shell_TrayWnd` is not up
    // yet at autostart); it is retried before the next hide and at exit, and
    // on the next start.
    if let Err(e) = recover_marker(dir, marker) {
        log!("taskbar recovery failed, keeping the marker: {e}");
        RECOVERY_PENDING.store(true, Ordering::SeqCst);
    }
}

/// Retries a recovery that failed at startup. Cheap no-op otherwise.
fn retry_pending_recovery() {
    if !RECOVERY_PENDING.load(Ordering::SeqCst) {
        return;
    }
    let Some(dir) = MARKER_DIR.get() else { return };
    let marker = read_marker_file(dir);
    if marker == MarkerState::None {
        RECOVERY_PENDING.store(false, Ordering::SeqCst);
        return;
    }
    if recover_marker(dir, marker).is_err() {
        log!("taskbar recovery retry failed, keeping the marker");
    }
}

/// Installs a panic hook that restores the taskbar and clears the marker
/// (the release build aborts after the hook, so no destructor would), then
/// defers to the previously installed hook.
///
/// Safe from any thread and non-panicking: it only `try_lock`s the record (if
/// that fails, e.g. the panicking thread holds it, the on-disk marker in the
/// lock-free `MARKER_DIR` is read instead), and the restore is plain Win32
/// calls whose errors are ignored. If a panic does not abort (a debug build),
/// the in-memory record is reset too so later hides are tracked afresh. The
/// restore runs before `log!`, so a logging lock held by the panicking thread
/// can at worst stall the default panic message, never the restore.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_for_panic();
        log!("panic: {info}");
        previous(info);
    }));
}

fn restore_for_panic() {
    let mut guard = match RECORD.try_lock() {
        Ok(g) => Some(g),
        Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    };
    let dir = MARKER_DIR.get().map(PathBuf::as_path);
    let techniques = panic_restore_target(guard.as_deref().copied(), dir);
    if techniques.is_empty() {
        return;
    }
    let mut all_restored = true;
    for restore in techniques {
        // The panic hook shows bare, which never calls back (and must not
        // touch the session lock).
        all_restored &=
            win32::restore_taskbar_raw(restore, show_kind_for(RawRestorePath::PanicHook)).is_ok();
    }
    // A failed restore (e.g. Explorer hung) keeps the marker for the next start.
    if !all_restored {
        return;
    }
    if let Some(dir) = dir {
        let _ = clear_marker_file(dir);
    }
    if let Some(rec) = guard.as_deref_mut() {
        *rec = HideRecord::EMPTY;
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
        ObservedTaskbar {
            visible,
            autohide: Some(autohide),
        }
    }

    /// The taskbar visible or not, but its appbar state unreadable (hung Explorer).
    fn observed_unknown(visible: bool) -> ObservedTaskbar {
        ObservedTaskbar {
            visible,
            autohide: None,
        }
    }

    #[test]
    fn a_hover_reveal_marker_with_an_unknown_appbar_state_cannot_be_decided() {
        for prior in [HOVER_FROM_VISIBLE, HOVER_FROM_AUTO] {
            assert_eq!(
                decide_recovery(MarkerState::Technique(prior), observed_unknown(true)),
                RecoveryAction::Undecidable
            );
        }
    }

    #[test]
    fn an_unknown_appbar_state_does_not_affect_the_fully_hidden_decision() {
        assert_eq!(
            decide_recovery(MarkerState::Technique(FULLY), observed_unknown(false)),
            RecoveryAction::ShowTaskbar
        );
        assert_eq!(
            decide_recovery(MarkerState::Technique(FULLY), observed_unknown(true)),
            RecoveryAction::Nothing
        );
    }

    #[derive(Debug, PartialEq)]
    enum Step {
        ShowFull,
        SetAppbar(TaskbarAutohideState),
        Marked(TaskbarHideRestore),
    }

    /// Runs `restore_marked_with` with recording fakes; the OS steps fail when
    /// `os_fails`. Returns the result and the ordered steps taken.
    fn run_restore(
        restore: TaskbarHideRestore,
        os_fails: bool,
    ) -> (Result<(), &'static str>, Vec<Step>) {
        let steps = std::cell::RefCell::new(Vec::new());
        let outcome = |step: Step| {
            steps.borrow_mut().push(step);
            if os_fails { Err("os") } else { Ok(()) }
        };
        let result = restore_marked_with(
            restore,
            || outcome(Step::ShowFull),
            |s| outcome(Step::SetAppbar(s)),
            |r| steps.borrow_mut().push(Step::Marked(r)),
        );
        (result, steps.into_inner())
    }

    #[test]
    fn restoring_fully_hidden_shows_fully_then_clears_only_that_mark() {
        let (result, steps) = run_restore(FULLY, false);
        assert_eq!(result, Ok(()));
        assert_eq!(steps, vec![Step::ShowFull, Step::Marked(FULLY)]);
    }

    #[test]
    fn restoring_hover_reveal_sets_the_prior_state_then_clears_only_that_mark() {
        let (result, steps) = run_restore(HOVER_FROM_AUTO, false);
        assert_eq!(result, Ok(()));
        assert_eq!(
            steps,
            vec![Step::SetAppbar(AutoHidden), Step::Marked(HOVER_FROM_AUTO)]
        );
    }

    #[test]
    fn a_failed_os_restore_leaves_the_mark_in_place() {
        for restore in [FULLY, HOVER_FROM_VISIBLE] {
            let (result, steps) = run_restore(restore, true);
            assert_eq!(result, Err("os"));
            assert!(
                !steps.iter().any(|s| matches!(s, Step::Marked(_))),
                "{steps:?}"
            );
        }
    }

    #[test]
    fn startup_recovery_uses_the_full_show_and_the_panic_hook_stays_bare() {
        assert_eq!(show_kind_for(RawRestorePath::Startup), ShowKind::Full);
        assert_eq!(show_kind_for(RawRestorePath::PanicHook), ShowKind::Bare);
    }

    #[test]
    fn exit_shows_a_still_hidden_taskbar_only_if_this_run_hid_it() {
        assert!(needs_exit_safety_show(true, Some(false)));
        assert!(!needs_exit_safety_show(false, Some(false)));
        assert!(!needs_exit_safety_show(true, Some(true)));
        assert!(!needs_exit_safety_show(true, None));
    }

    #[test]
    fn appbar_flags_change_reports_both_values_only_when_they_differ() {
        assert_eq!(appbar_flags_change(Some(0), 1), Some((0, 1)));
        assert_eq!(appbar_flags_change(Some(1), 1), None);
        assert_eq!(appbar_flags_change(None, 1), None);
    }

    #[test]
    fn no_marker_does_nothing_even_if_the_taskbar_is_hidden() {
        assert_eq!(
            decide_recovery(MarkerState::None, observed(false, AutoHidden)),
            RecoveryAction::Nothing
        );
    }

    #[test]
    fn fully_hidden_marker_with_a_hidden_taskbar_shows_it() {
        let action = decide_recovery(
            MarkerState::Technique(FULLY),
            observed(false, AlwaysVisible),
        );
        assert_eq!(action, RecoveryAction::ShowTaskbar);
    }

    #[test]
    fn fully_hidden_marker_with_a_visible_taskbar_does_nothing() {
        let action = decide_recovery(MarkerState::Technique(FULLY), observed(true, AlwaysVisible));
        assert_eq!(action, RecoveryAction::Nothing);
    }

    #[test]
    fn hover_reveal_marker_with_an_auto_hidden_taskbar_writes_back_the_prior_state() {
        let action = decide_recovery(
            MarkerState::Technique(HOVER_FROM_VISIBLE),
            observed(true, AutoHidden),
        );
        assert_eq!(action, RecoveryAction::SetAppbarState(AlwaysVisible));
    }

    #[test]
    fn hover_reveal_marker_with_an_already_visible_taskbar_does_nothing() {
        let action = decide_recovery(
            MarkerState::Technique(HOVER_FROM_VISIBLE),
            observed(true, AlwaysVisible),
        );
        assert_eq!(action, RecoveryAction::Nothing);
    }

    #[test]
    fn hover_reveal_marker_whose_prior_state_was_auto_hide_does_nothing() {
        let action = decide_recovery(
            MarkerState::Technique(HOVER_FROM_AUTO),
            observed(true, AutoHidden),
        );
        assert_eq!(action, RecoveryAction::Nothing);
    }

    #[test]
    fn unreadable_marker_with_a_hidden_taskbar_shows_it() {
        let action = decide_recovery(MarkerState::Unreadable, observed(false, AlwaysVisible));
        assert_eq!(action, RecoveryAction::ShowTaskbar);
    }

    #[test]
    fn unreadable_marker_with_a_visible_taskbar_does_nothing() {
        assert_eq!(
            decide_recovery(MarkerState::Unreadable, observed(true, AutoHidden)),
            RecoveryAction::Nothing
        );
        assert_eq!(
            decide_recovery(MarkerState::Unreadable, observed(true, AlwaysVisible)),
            RecoveryAction::Nothing
        );
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

    fn write_marker_file(dir: &Path, rec: HideRecord) {
        write_record_file(dir, &rec.to_marker_text().unwrap()).unwrap();
    }

    fn record_of(techniques: &[TaskbarHideRestore]) -> HideRecord {
        let mut rec = HideRecord::default();
        for t in techniques {
            rec.hide(*t);
        }
        rec
    }

    #[test]
    fn marker_file_round_trips_and_clears() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_marker_file(tmp.path()), MarkerState::None);

        write_marker_file(tmp.path(), record_of(&[HOVER_FROM_VISIBLE]));
        assert_eq!(
            read_marker_file(tmp.path()),
            MarkerState::Technique(HOVER_FROM_VISIBLE)
        );

        clear_marker_file(tmp.path()).unwrap();
        assert_eq!(read_marker_file(tmp.path()), MarkerState::None);
        // Clearing an absent marker is not an error.
        clear_marker_file(tmp.path()).unwrap();
    }

    #[test]
    fn a_marker_with_both_techniques_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        write_marker_file(tmp.path(), record_of(&[HOVER_FROM_VISIBLE, FULLY]));
        let marker = read_marker_file(tmp.path());
        assert_eq!(
            marker,
            MarkerState::Combined {
                hover_prior: AlwaysVisible
            }
        );
        assert_eq!(marker_techniques(marker), vec![HOVER_FROM_VISIBLE, FULLY]);
    }

    #[test]
    fn a_corrupt_marker_file_reads_as_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(marker_path(tmp.path()), "???").unwrap();
        assert_eq!(read_marker_file(tmp.path()), MarkerState::Unreadable);
        std::fs::write(marker_path(tmp.path()), "fully-hidden\n???\n").unwrap();
        assert_eq!(read_marker_file(tmp.path()), MarkerState::Unreadable);
        std::fs::write(marker_path(tmp.path()), "").unwrap();
        assert_eq!(read_marker_file(tmp.path()), MarkerState::Unreadable);
    }

    #[test]
    fn an_empty_record_has_no_marker_text() {
        assert_eq!(HideRecord::default().to_marker_text(), None);
    }

    #[test]
    fn restoring_one_technique_keeps_the_other_recorded() {
        let mut rec = record_of(&[HOVER_FROM_VISIBLE, FULLY]);
        rec.restore(HOVER_FROM_VISIBLE);
        assert_eq!(rec.techniques(), vec![FULLY]);
        assert!(rec.to_marker_text().is_some());

        let mut rec = record_of(&[FULLY, HOVER_FROM_VISIBLE]);
        rec.restore(FULLY);
        assert_eq!(rec.techniques(), vec![HOVER_FROM_VISIBLE]);
        rec.restore(HOVER_FROM_AUTO);
        assert!(rec.is_empty());
    }

    #[test]
    fn a_restore_clears_the_record_even_when_it_returns_to_the_users_auto_hide() {
        let mut rec = HideRecord::default();
        rec.hide(HOVER_FROM_AUTO);
        rec.restore(HOVER_FROM_AUTO);
        assert!(rec.is_empty());
        // A later hide with a different technique is then recorded afresh.
        rec.hide(FULLY);
        assert_eq!(rec.techniques(), vec![FULLY]);
    }

    #[test]
    fn the_first_hover_reveal_wins_over_a_nested_one() {
        let mut rec = HideRecord::default();
        assert_eq!(rec.hide(HOVER_FROM_VISIBLE), Some(HideRecord::EMPTY));
        // A nested hover-reveal sees AutoHidden as its "prior"; ignore it.
        assert_eq!(rec.hide(HOVER_FROM_AUTO), None);
        assert_eq!(rec.techniques(), vec![HOVER_FROM_VISIBLE]);
        assert_eq!(rec.hide(FULLY), Some(record_of(&[HOVER_FROM_VISIBLE])));
        assert_eq!(rec.hide(FULLY), None);
    }

    #[test]
    fn the_panic_hook_falls_back_to_the_marker_file_when_the_lock_is_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        write_marker_file(tmp.path(), record_of(&[FULLY]));
        assert_eq!(panic_restore_target(None, Some(tmp.path())), vec![FULLY]);
    }

    #[test]
    fn the_panic_hook_prefers_the_in_memory_record_and_ignores_an_unrelated_marker() {
        let tmp = tempfile::tempdir().unwrap();
        write_marker_file(tmp.path(), record_of(&[FULLY]));
        assert_eq!(
            panic_restore_target(Some(record_of(&[HOVER_FROM_VISIBLE])), Some(tmp.path())),
            vec![HOVER_FROM_VISIBLE]
        );
        assert!(panic_restore_target(Some(HideRecord::EMPTY), Some(tmp.path())).is_empty());
    }

    #[test]
    fn the_panic_hook_has_nothing_to_restore_without_a_marker() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(panic_restore_target(None, Some(tmp.path())).is_empty());
        assert!(panic_restore_target(None, None).is_empty());
    }

    #[test]
    fn the_panic_hook_shows_the_taskbar_for_an_unreadable_marker_like_startup_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        for content in ["???", "", "fully-hidden\n???\n"] {
            std::fs::write(marker_path(tmp.path()), content).unwrap();
            assert_eq!(panic_restore_target(None, Some(tmp.path())), vec![FULLY]);
        }
        // Startup recovery shows a hidden taskbar for the same marker.
        assert_eq!(
            decide_recovery(MarkerState::Unreadable, observed(false, AlwaysVisible)),
            RecoveryAction::ShowTaskbar
        );
    }

    #[test]
    fn writing_the_marker_is_atomic_and_leaves_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        write_marker_file(tmp.path(), record_of(&[FULLY]));
        write_marker_file(tmp.path(), record_of(&[HOVER_FROM_VISIBLE, FULLY]));
        assert_eq!(
            read_marker_file(tmp.path()),
            MarkerState::Combined {
                hover_prior: AlwaysVisible
            }
        );
        let names: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from(MARKER_FILE)]);
    }
}
