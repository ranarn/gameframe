//! The taskbar-hide layer: every hide and restore of the real taskbar goes
//! through here, so the crash-recovery marker ([`crate::taskbar_recovery`])
//! is written before a hide takes effect and cleared on every restore path.
//!
//! Layering: [`crate::win32`] holds the unmarked OS primitives and imports
//! neither this module nor `taskbar_recovery`; `taskbar_recovery` sits on
//! `win32` and owns the marker; this module sits on both and owns the
//! hide/restore entry points, the armed-window registry and
//! [`restore_all_for_exit`]. Startup recovery and the panic hook use
//! `taskbar_recovery` + the raw `win32` restore directly (no armed registry).
//!
//! Any new code path that hides the taskbar must go through this module.

use crate::profile_store::TaskbarHideMode;
use crate::taskbar_recovery;
use crate::win32::{
    self, TaskbarAutohideState, TaskbarHideRestore, Win32Error, find_taskbar_window,
    gather_taskbar_judgement, plan_taskbar_hide, read_appbar_flags, should_rehide_on_join,
    state_from_flags, taskbar_autohide_state_from_bool, write_appbar_state,
};
use windows_sys::Win32::Foundation::HWND;

/// Hides the real taskbar completely via `ShowWindow(Shell_TrayWnd,
/// SW_HIDE)` -- the `FullyHidden` mode's technique: unlike the
/// native appbar auto-hide mechanism, this does not reappear on mouse hover
/// near the screen edge.
///
/// OS-facing: locates the real taskbar window and sends it a real
/// `ShowWindow` call, unless Explorer is hung. Not exercised by this crate's
/// automated tests -- see the `win32` module docs.
fn hide_taskbar_completely() -> Result<(), Win32Error> {
    mark_and_hide_taskbar_window(find_taskbar_window()?)
}

/// Every call into Explorer for a hide goes through this check: a hung
/// Explorer fails with [`Win32Error::WindowHung`] (logged once) before anything
/// is marked or sent, so no hide is ever queued behind a later show.
fn ensure_taskbar_responsive(hwnd: HWND) -> Result<(), Win32Error> {
    if win32::is_window_hung_logged(hwnd) {
        Err(Win32Error::WindowHung)
    } else {
        Ok(())
    }
}

/// [`hide_taskbar_completely`] on an already-resolved taskbar handle.
fn mark_and_hide_taskbar_window(hwnd: HWND) -> Result<(), Win32Error> {
    // Not marked when skipped: no hide happens.
    ensure_taskbar_responsive(hwnd)?;
    // Marker first: a kill right after the hide is then covered.
    if taskbar_recovery::mark_hidden(TaskbarHideRestore::FullyHidden).is_some() {
        // A new FullyHidden session (not a join): read and remember the appbar
        // flags, before the hide, to compare at its end.
        if let Some(flags) = read_appbar_flags(hwnd) {
            taskbar_recovery::note_session_start_flags(flags);
        }
    }
    win32::hide_taskbar_window(hwnd);
    Ok(())
}

/// Shows the real taskbar via `ShowWindow(Shell_TrayWnd, SW_SHOW)` -- undoes
/// [`hide_taskbar_completely`] -- and clears its recovery mark. The full show
/// re-sends the current appbar state unchanged and raises the taskbar; it
/// never flips the auto-hide flag (see [`TaskbarHideRestore`]'s doc comment).
///
/// OS-facing: locates the real taskbar window and sends it a real
/// `ShowWindow` call. Not exercised by this crate's automated tests.
fn show_taskbar() -> Result<(), Win32Error> {
    taskbar_recovery::restore_marked(TaskbarHideRestore::FullyHidden)
}

/// Hides the taskbar using whichever technique `mode` selects, remembering
/// which one was used so it can be undone later -- called by the
/// taskbar-autohide hotkey's "on" direction (via [`toggle_taskbar_hide`]),
/// the zone-editor overlay's taskbar toggle, and every
/// apply-path call site that hides the taskbar for a specific window (via
/// [`set_taskbar_hide_absolute_armed`]) -- all callers that need a "hide
/// now, restore exactly this later" round trip rather than a bare absolute
/// on/off set (the sibling [`set_taskbar_hide_absolute`] remains that bare
/// primitive, for callers -- and `set_taskbar_hide_absolute_armed`'s own
/// `hide == false` path -- with nothing to arm). Returns a
/// [`TaskbarHideRestore`] recording which technique was actually used, so a
/// later [`restore_taskbar`] call undoes exactly this even if the mode
/// setting changes in between.
///
/// OS-facing: for `HoverReveal`, resolves the taskbar window and reads its
/// flags once, reusing both for the write below rather than a separate
/// query-then-set round trip. Not exercised by this crate's automated tests
/// -- see the `win32` module docs. The decision of what to remember is pure and tested
/// separately via [`plan_taskbar_hide`].
pub fn hide_taskbar(mode: TaskbarHideMode) -> Result<TaskbarHideRestore, Win32Error> {
    match mode {
        TaskbarHideMode::FullyHidden => {
            hide_taskbar_completely()?;
            Ok(TaskbarHideRestore::FullyHidden)
        }
        TaskbarHideMode::HoverReveal => {
            let hwnd = find_taskbar_window()?;
            ensure_taskbar_responsive(hwnd)?;
            // Unknown flags (Explorer hung since the check): no hide, no mark.
            let current_flags = read_appbar_flags(hwnd).ok_or(Win32Error::WindowHung)?;
            let current = state_from_flags(current_flags);
            let restore = plan_taskbar_hide(mode, current);
            let undo = taskbar_recovery::mark_hidden(restore);
            if let Err(e) =
                write_appbar_state(hwnd, current_flags, TaskbarAutohideState::AutoHidden)
            {
                // The hide never happened: don't claim it did.
                if let Some(undo) = undo {
                    taskbar_recovery::undo_hide(undo);
                }
                return Err(e);
            }
            Ok(restore)
        }
    }
}

/// Restores the taskbar to exactly the state remembered by a prior
/// [`hide_taskbar`] call -- dispatches on which technique [`TaskbarHideRestore`]
/// records rather than assuming whatever [`TaskbarHideMode`] is currently
/// configured, which is exactly the point: the mode may have changed between
/// the hide and this restore.
pub fn restore_taskbar(remembered: TaskbarHideRestore) -> Result<(), Win32Error> {
    taskbar_recovery::restore_marked(remembered)
}

/// Reference-counted arming state for the auto-restore-on-close feature (see
/// [`ArmedTaskbarWindows`] for the transition logic). Tracks every window
/// whose apply-path has hidden the taskbar and is still relying on it staying
/// hidden, plus the single [`TaskbarHideRestore`] needed to undo the *first*
/// hide of the current "hidden session".
/// Consulted by [`take_armed_taskbar_restore_for_window`] when
/// [`crate::launch_watcher`]'s `EVENT_OBJECT_DESTROY` watcher observes an
/// armed window closing, and by [`take_any_armed_taskbar_restore`] when
/// [`crate::app::run`] quits via the tray "Exit" item while a hide is still
/// in effect.
///
/// The armed windows are a set rather than a single slot because several
/// tracked games may run side by side (e.g. `Left`+`Right`), all hiding the
/// taskbar. Tracking only the most recent window would (1) lose the original
/// state -- a second window arming while the first was still armed would
/// re-capture the *already-hidden* appbar state as its "restore to" value in
/// `HoverReveal` mode -- and (2) restore prematurely -- the second window
/// closing would restore the taskbar out from under the first game, which
/// still wants it hidden. So the remembered restore value is captured once,
/// on the empty-to-non-empty transition, and only released once the set
/// drains back to empty.
///
/// Wrapped in `Option` (rather than a directly-constructed
/// [`ArmedTaskbarWindows`]) purely so the `static` initializer can be
/// `Mutex::new(None)`, a `const` expression -- [`with_armed_taskbar_windows`]
/// lazily fills it in on first use via `get_or_insert_with`.
///
/// Stores each window as `isize` rather than `HWND`: `HWND` is `*mut
/// core::ffi::c_void`, which is neither `Send` nor `Sync`, so a `HashSet`
/// of them could not live in a `static` (a `static` requires `Sync`, and
/// `Mutex<T>` is only `Sync` when `T: Send`). Each stored `isize` is a
/// plain pointer-identity value -- compared for equality/hashed only, never
/// dereferenced -- so the round trip through an integer is lossless and
/// safe.
///
/// Fully independent of [`toggle_taskbar_hide`] (which keeps no remembered
/// state of its own at all -- see its doc comment): the manual
/// global taskbar-autohide hotkey's own toggle must stay independent from
/// the Zone-apply paths' taskbar handling, and this static is never read
/// or written by [`toggle_taskbar_hide`] or the zone editor's own
/// session-scoped hide (`crate::zone_overlay`, which keeps its remembered
/// [`TaskbarHideRestore`] locally in its own guard struct, not here).
static ARMED_TASKBAR_RESTORE: std::sync::Mutex<Option<ArmedTaskbarWindows>> =
    std::sync::Mutex::new(None);

/// Pure reference-counted arming state backing [`ARMED_TASKBAR_RESTORE`],
/// factored out so its transitions are unit testable without touching the
/// shared global mutex.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ArmedTaskbarWindows {
    windows: std::collections::HashSet<isize>,
    restore: Option<TaskbarHideRestore>,
}

impl ArmedTaskbarWindows {
    /// True if a hidden session is currently active, i.e. at least one
    /// window is currently armed.
    fn is_active(&self) -> bool {
        !self.windows.is_empty()
    }

    /// True only if `hwnd_id` is currently armed *and* is the sole armed
    /// window -- i.e. removing it would end the hidden session and require
    /// an actual OS-level taskbar restore, as opposed to just leaving other
    /// windows' hides in effect.
    fn is_last(&self, hwnd_id: isize) -> bool {
        self.windows.len() == 1 && self.windows.contains(&hwnd_id)
    }

    /// The technique the active session was started with (its remembered
    /// restore value), read-only; `None` when no session is active.
    fn session_restore(&self) -> Option<TaskbarHideRestore> {
        self.restore
    }

    /// Starts a brand new hidden session: arms `hwnd_id` and remembers
    /// `restore` as what to restore to once the session ends. Callers must
    /// only call this when [`Self::is_active`] was `false` beforehand (a
    /// fresh session) -- see [`Self::join`] for the already-active case.
    fn start(&mut self, hwnd_id: isize, restore: TaskbarHideRestore) {
        self.windows.insert(hwnd_id);
        self.restore = Some(restore);
    }

    /// Joins an already-active hidden session: arms `hwnd_id` without
    /// touching the remembered restore value -- it still reflects the true
    /// pre-first-hide state, and must never be overwritten by a later
    /// window's hide (see [`ARMED_TASKBAR_RESTORE`]'s doc comment).
    fn join(&mut self, hwnd_id: isize) {
        self.windows.insert(hwnd_id);
    }

    /// Removes `hwnd_id` from the session. Returns `Some(restore)` --
    /// clearing the remembered value -- only if `hwnd_id` was armed and
    /// removing it emptied the set (the last window relying on the hide
    /// just left); returns `None` otherwise, whether because `hwnd_id`
    /// wasn't armed at all (an unrelated window) or because other windows
    /// are still relying on the hide (see [`ARMED_TASKBAR_RESTORE`]'s doc
    /// comment).
    fn leave(&mut self, hwnd_id: isize) -> Option<TaskbarHideRestore> {
        self.windows.remove(&hwnd_id);
        if self.windows.is_empty() {
            self.restore.take()
        } else {
            None
        }
    }

    /// Unconditionally ends the session (if any), returning the remembered
    /// restore value regardless of how many windows were still armed --
    /// backs the tray "Exit" path's semantics: the process is quitting
    /// regardless of how many tracked games are still running, so "wait for
    /// the rest" doesn't apply.
    fn take_all(&mut self) -> Option<TaskbarHideRestore> {
        self.windows.clear();
        self.restore.take()
    }
}

/// Runs `f` against the shared [`ArmedTaskbarWindows`] state, initializing
/// it on first use, and recovering (rather than silently no-op'ing) if the
/// mutex was poisoned by a panic while held --
/// mirroring `crate::window_filter::known_steam_install_dirs`'s identical
/// `unwrap_or_else(|poisoned| poisoned.into_inner())` pattern for a
/// comparable `Mutex<Option<...>>` static. The guarded data (a `HashSet`
/// plus an `Option`) has no invariant a panic mid-hold could actually
/// violate, so recovering it is safe and strictly better than disabling
/// this whole feature for the rest of the process's life the moment any
/// panic anywhere happens to occur while the lock is held.
fn with_armed_taskbar_windows<R>(f: impl FnOnce(&mut ArmedTaskbarWindows) -> R) -> R {
    let mut guard = ARMED_TASKBAR_RESTORE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(guard.get_or_insert_with(ArmedTaskbarWindows::default))
}

/// True only if `hwnd` is currently armed *and* is the sole armed window --
/// see [`ArmedTaskbarWindows::is_last`].
fn is_last_armed_window(hwnd: HWND) -> bool {
    with_armed_taskbar_windows(|state| state.is_last(hwnd as isize))
}

/// Starts a brand new hidden session for `hwnd`, remembering `restore` as
/// what to restore to once the session ends. Called only by
/// [`set_taskbar_hide_absolute_armed`]'s `hide == true` path when
/// no session was active beforehand ([`ArmedTaskbarWindows::is_active`]).
fn start_taskbar_hide_session(hwnd: HWND, restore: TaskbarHideRestore) {
    with_armed_taskbar_windows(|state| state.start(hwnd as isize, restore));
}

/// Joins `hwnd` to an already-active hidden session, without touching the
/// remembered restore value. Called only by
/// [`set_taskbar_hide_absolute_armed`]'s `hide == true` path when
/// a session was already active ([`ArmedTaskbarWindows::is_active`]).
fn join_taskbar_hide_session(hwnd: HWND) {
    with_armed_taskbar_windows(|state| state.join(hwnd as isize));
}

/// Takes (removes and returns) the armed restore if -- and only if --
/// removing `hwnd` from the currently-armed set empties it, via
/// [`ArmedTaskbarWindows::leave`]. Called by [`crate::launch_watcher`]'s
/// `EVENT_OBJECT_DESTROY` callback when `hwnd` closes: an empty result
/// means that was the last window whose Zone-apply had hidden the
/// taskbar, so the caller should restore it immediately with no hotkey
/// press required. Returns `None` for every
/// other window closing (unarmed, or other windows still relying on the
/// hide), leaving the armed state's restore value untouched -- so an
/// unrelated window's destroy event has no effect, and another tracked
/// game's still-active hide is never prematurely undone.
///
/// Also used, with `hide == false`, by [`set_taskbar_hide_absolute_armed`]
/// to disarm `hwnd` when its own apply-path explicitly shows the taskbar
/// again: without this, a stale armed restore for a window whose
/// `hide_taskbar_on_apply` setting later flips to `false` (e.g. edited live in
/// Settings) would still fire an incorrect restore when that window
/// eventually closes.
///
/// A null `hwnd` never matches (defensive): both call paths above --
/// `crate::launch_watcher`'s destroy callback, and
/// `set_taskbar_hide_absolute_armed`'s `hide == false` branch -- reach this
/// function, and while neither of today's real callers can realistically
/// pass a null `HWND`, this guard makes that safe regardless of caller.
pub(crate) fn take_armed_taskbar_restore_for_window(hwnd: HWND) -> Option<TaskbarHideRestore> {
    if hwnd.is_null() {
        return None;
    }
    with_armed_taskbar_windows(|state| state.leave(hwnd as isize))
}

/// Takes (removes and returns) whatever restore is currently armed,
/// unconditionally ending the hidden session regardless of how many windows
/// were still relying on it -- see [`ArmedTaskbarWindows::take_all`].
/// Called by [`crate::app::run`] on the tray "Exit" path: the process is
/// about to quit, so "wait for the rest" doesn't apply -- any still-armed
/// hide must be undone before the process exits, full stop, or the desktop
/// is left with a hidden taskbar and no running GameFrame instance left to
/// restore it.
pub(crate) fn take_any_armed_taskbar_restore() -> Option<TaskbarHideRestore> {
    with_armed_taskbar_windows(|state| state.take_all())
}

/// Sets the taskbar's hidden/visible state to exactly `hide` (an absolute
/// set, not a toggle), using whichever technique `mode` selects. No restore
/// memory is kept here (unlike [`hide_taskbar`]/[`restore_taskbar`]): this
/// is the low-level primitive for a caller that already knows the absolute
/// value it wants and has no need to later "undo the last hide" for a
/// specific window.
///
/// Every apply-path call site that hides the taskbar for a
/// specific window (the tray/launch-detection/borderless-toggle apply paths
/// and Settings' live-reapply path) calls [`set_taskbar_hide_absolute_armed`]
/// instead of this function directly, so that window's later close can
/// automatically restore the taskbar. This function remains the plain
/// absolute-set primitive that wrapper builds on, and is also still called
/// directly wherever there genuinely is no window to arm (e.g. showing the
/// taskbar back on the `hide == false` path).
///
/// Deliberately independent of [`toggle_taskbar_hide`]/its state above: this
/// never reads or writes a caller-tracked [`TaskbarHideRestore`] variable, so
/// it can't drift out of sync with (or be confused for) the existing global
/// taskbar-autohide hotkey's own toggle mechanic, which must stay untouched and
/// independent of the Zone-apply paths.
///
/// OS-facing: locates the real taskbar window and sends it a real
/// `ShowWindow`/`SHAppBarMessage` call depending on `mode`. Not exercised by
/// this crate's automated tests -- see the `win32` module docs; the `bool` ->
/// [`TaskbarAutohideState`] interpretation itself is pure and unit tested via
/// [`taskbar_autohide_state_from_bool`].
pub fn set_taskbar_hide_absolute(hide: bool, mode: TaskbarHideMode) -> Result<(), Win32Error> {
    match mode {
        TaskbarHideMode::FullyHidden => {
            if hide {
                hide_taskbar_completely()
            } else {
                show_taskbar()
            }
        }
        TaskbarHideMode::HoverReveal => {
            let target = taskbar_autohide_state_from_bool(hide);
            if !hide {
                // Back to `target`: sets it and clears the mark.
                return taskbar_recovery::restore_marked(TaskbarHideRestore::HoverReveal(target));
            }
            // Marker first, recording the state to go back to.
            let hwnd = find_taskbar_window()?;
            ensure_taskbar_responsive(hwnd)?;
            let prior = state_from_flags(read_appbar_flags(hwnd).ok_or(Win32Error::WindowHung)?);
            let undo = taskbar_recovery::mark_hidden(TaskbarHideRestore::HoverReveal(prior));
            if let Err(e) = win32::set_appbar_state(target) {
                // The hide never happened: don't claim it did.
                if let Some(undo) = undo {
                    taskbar_recovery::undo_hide(undo);
                }
                return Err(e);
            }
            Ok(())
        }
    }
}

/// Sets the taskbar's hidden/visible state to exactly `hide` for `hwnd`'s
/// apply, exactly like [`set_taskbar_hide_absolute`], but additionally arms
/// (`hide == true`) or disarms (`hide == false`) the
/// destroy-watcher restore for `hwnd` via the reference-counted
/// [`ArmedTaskbarWindows`] state -- see [`ARMED_TASKBAR_RESTORE`]'s doc
/// comment. This is the entry point every apply-path call site that hides
/// the taskbar for a specific window should use, instead of calling
/// [`set_taskbar_hide_absolute`] directly, so that window's later close can
/// automatically restore the taskbar with no hotkey press required.
///
/// The `hide == true` path only calls the real OS hide
/// ([`hide_taskbar`], which *does* return a [`TaskbarHideRestore`], unlike
/// [`set_taskbar_hide_absolute`]'s plain absolute set) when no hidden
/// session is currently active; if one already is (another tracked game's
/// hide is already in effect), `hwnd` just joins that session via
/// [`join_taskbar_hide_session`] -- with no re-capture of the remembered
/// restore value (which would silently discard the true pre-first-hide
/// state; see [`ARMED_TASKBAR_RESTORE`]'s doc comment). When the *session* was
/// started in `FullyHidden` (not merely the currently configured `mode`, which
/// may have changed since) it additionally re-hides a taskbar that is still
/// `WS_VISIBLE` (e.g. one the hotkey showed), re-marking the recovery record
/// hidden first.
///
/// The `hide == false` path only issues the real OS show
/// ([`set_taskbar_hide_absolute`]) when `hwnd` is the *last* armed window
/// ([`is_last_armed_window`]): showing the taskbar while another tracked
/// game's hide is still in effect would force it visible out from under
/// that other game, which this deliberately avoids -- the simplest
/// consistent behavior, mirroring the destroy-watcher's own "only restore
/// once the set drains to empty" logic. When the OS show call *is* made but
/// fails, `hwnd` is deliberately left armed rather than disarmed anyway:
/// discarding the one piece of state that could still fix the taskbar later
/// (via a subsequent close or tray Exit) on top of an OS call that's
/// already known to have failed would only compound the failure.
///
/// OS-facing (delegates to [`hide_taskbar`]/[`set_taskbar_hide_absolute`]).
/// Not exercised by this crate's automated tests -- see the `win32` module docs.
pub fn set_taskbar_hide_absolute_armed(
    hwnd: HWND,
    hide: bool,
    mode: TaskbarHideMode,
) -> Result<(), Win32Error> {
    if hide {
        // One lock for both reads, so the session can't drain in between.
        let (active, session) =
            with_armed_taskbar_windows(|state| (state.is_active(), state.session_restore()));
        if active {
            // A taskbar the hotkey showed in the meantime goes away again
            // when the game is returned to. `hide_taskbar_window` marks
            // the recovery record hidden first (the hotkey show cleared it)
            // and hides; the session's remembered restore is not touched.
            let judgement = find_taskbar_window().ok().map(gather_taskbar_judgement);
            let rehide = judgement
                .as_ref()
                .is_some_and(|j| should_rehide_on_join(session, j.verdict));
            log!(
                "taskbar hide for {hwnd:?}: joining the active session (session={session:?}, re-hide={rehide}); {}",
                judgement
                    .as_ref()
                    .map_or_else(|| "Shell_TrayWnd not found".to_string(), describe_taskbar)
            );
            // Join first: the re-hide is best-effort and must not leave
            // `hwnd` out of the armed set.
            join_taskbar_hide_session(hwnd);
            // Best-effort: a hung Explorer skips the re-hide.
            if rehide && let Some(j) = judgement {
                let _ = mark_and_hide_taskbar_window(j.taskbar);
            }
            Ok(())
        } else {
            let restore = hide_taskbar(mode)?;
            start_taskbar_hide_session(hwnd, restore);
            log!(
                "taskbar hide for {hwnd:?}: started a session ({mode:?}); {}",
                taskbar_debug_state()
            );
            Ok(())
        }
    } else if is_last_armed_window(hwnd) {
        let result = set_taskbar_hide_absolute(false, mode);
        if result.is_ok() {
            take_armed_taskbar_restore_for_window(hwnd);
        }
        result
    } else {
        // Either `hwnd` was never armed, or other windows are still relying
        // on the hide -- disarm `hwnd` (a no-op if it wasn't armed) without
        // touching the real taskbar.
        take_armed_taskbar_restore_for_window(hwnd);
        Ok(())
    }
}

/// Reads the taskbar's *actual* current hidden/visible state directly from
/// the OS, rather than trusting any caller-tracked flag: for `FullyHidden`,
/// whether the real `Shell_TrayWnd` is actually on screen (see
/// [`crate::win32::judge_taskbar_hidden`]: not visible, mostly off its monitor, or covered
/// at its center all count as hidden); for `HoverReveal`, the
/// real appbar auto-hide flag (via [`read_appbar_flags`]/[`state_from_flags`]).
///
/// This exists because a hide-vs-show decision made from a remembered flag
/// that only the hotkey ever wrote would go stale: the taskbar can also be
/// hidden by an entirely different path (a tracked game's launch-detection
/// auto-hide, [`set_taskbar_hide_absolute_armed`], which re-applies on every
/// foreground switch back to the game, not just the initial launch). That path
/// never touches the hotkey's own memory, so the two could disagree about whether
/// the taskbar was hidden, making the hotkey's next press act on a stale
/// premise (hiding an already-hidden taskbar is a no-op; the *following*
/// press then does the opposite of what was expected). Grounding the
/// decision in the real OS state on every press removes that drift entirely.
///
/// OS-facing: `judgement` is the just-gathered state of the real taskbar
/// (`HoverReveal` additionally reads its appbar flags). Not exercised by this
/// crate's automated tests -- see the `win32` module docs.
///
/// `None` when HoverReveal's flags are unknown (Explorer hung): the caller
/// must not act on a guess.
fn is_taskbar_hidden(mode: TaskbarHideMode, judgement: &win32::TaskbarJudgement) -> Option<bool> {
    match mode {
        TaskbarHideMode::FullyHidden => Some(judgement.verdict.is_some()),
        TaskbarHideMode::HoverReveal => read_appbar_flags(judgement.taskbar)
            .map(|flags| state_from_flags(flags) == TaskbarAutohideState::AutoHidden),
    }
}

/// Toggles the taskbar's hidden/visible state on/off -- the manual global
/// taskbar-autohide hotkey's own independent on/off toggle, grounded in the
/// taskbar's real current state ([`is_taskbar_hidden`]) rather than a
/// remembered flag (see that function's doc comment for why): currently
/// visible hides via
/// [`set_taskbar_hide_absolute`]; currently hidden shows via the same.
///
/// Fully independent of [`crate::win32::toggle_borderless_on_window`]'s
/// `RestoreState`, and of [`set_taskbar_hide_absolute_armed`]'s
/// `ARMED_TASKBAR_RESTORE` bookkeeping -- this reads only the taskbar's real
/// OS-level visibility, never any tracked-game config or armed-window state,
/// so it stays independent of tracked-game state.
///
/// OS-facing: locates the real taskbar window and sends it a real
/// `ShowWindow`/`SHAppBarMessage` call. Not exercised by this crate's
/// automated tests -- see the `win32` module docs.
pub fn toggle_taskbar_hide(mode: TaskbarHideMode) -> Result<(), Win32Error> {
    // The hotkey can appear to do nothing after long uptime. Log what it saw
    // and what it left behind so a report comes with evidence.
    // One gather feeds the log line and the decision, so they cannot disagree.
    let taskbar = find_taskbar_window().inspect_err(|_| {
        log!("taskbar hotkey ({mode:?}): before: Shell_TrayWnd not found");
    })?;
    let judgement = gather_taskbar_judgement(taskbar);
    log!(
        "taskbar hotkey ({mode:?}): before: {}",
        describe_taskbar(&judgement)
    );
    // Explorer hung: neither hide nor show on a guess.
    let Some(hidden) = is_taskbar_hidden(mode, &judgement) else {
        return Err(Win32Error::WindowHung);
    };
    let reason = judgement
        .verdict
        .filter(|_| mode == TaskbarHideMode::FullyHidden);
    let result = set_taskbar_hide_absolute(!hidden, mode);
    log!(
        "taskbar hotkey: judged hidden={hidden} (reason={reason:?}), so {} -> {result:?}; after: {}",
        if hidden { "showing" } else { "hiding" },
        taskbar_debug_state()
    );
    result
}

/// One line describing the taskbar's real state, for the log:
/// `WS_VISIBLE`, the appbar flags, its rect, whether it is actually the
/// topmost window at its own center (it can be `WS_VISIBLE` yet covered),
/// the foreground window, and the armed hide session.
fn taskbar_debug_state() -> String {
    match find_taskbar_window() {
        Ok(taskbar) => describe_taskbar(&gather_taskbar_judgement(taskbar)),
        Err(_) => "Shell_TrayWnd not found".to_string(),
    }
}

/// [`taskbar_debug_state`] for an already-gathered judgement, plus the armed
/// session.
fn describe_taskbar(judgement: &win32::TaskbarJudgement) -> String {
    let armed = with_armed_taskbar_windows(|s| (s.windows.len(), s.restore.is_some()));
    win32::describe_taskbar(judgement, armed)
}

/// The whole graceful-exit restore: the armed session's hide first, then
/// [`taskbar_recovery::restore_on_exit`]. Shared by the tray Exit path and
/// `WM_ENDSESSION`; idempotent, so running both is harmless.
pub fn restore_all_for_exit() {
    if let Some(restore) = take_any_armed_taskbar_restore()
        && let Err(e) = restore_taskbar(restore)
    {
        log!("failed to restore taskbar on exit: {e}");
    }
    taskbar_recovery::restore_on_exit();
}

#[cfg(test)]
mod tests;
