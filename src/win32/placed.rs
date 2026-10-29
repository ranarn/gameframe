//! Everything GameFrame remembers per window handle, behind one lock.

use super::{DriftAttempts, HungSkipLog, strip_border_ex_style, strip_border_style};
use crate::region_calc::Region;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, WS_MAXIMIZE};

/// What a foreground window looked like immediately before the borderless
/// hotkey most recently stripped its border -- remembered so a second press
/// of the same hotkey can restore it exactly.
///
/// `style` never carries `WS_MAXIMIZE` (see [`RestoreState::new`]) and, for a
/// window that was maximized, `region` is its normal (un-maximized) rect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestoreState {
    pub style: u32,
    pub ex_style: u32,
    pub region: Region,
}

impl RestoreState {
    /// Builds the state to remember for a window with the given live styles.
    /// A maximized window is remembered un-maximized: restoring must not
    /// write `WS_MAXIMIZE` back onto a window that is no longer maximized.
    pub fn new(style: u32, ex_style: u32, region: Region) -> Self {
        Self {
            style: style & !WS_MAXIMIZE,
            ex_style,
            region,
        }
    }
}

/// The pre-strip [`RestoreState`] of every window GameFrame has stripped,
/// keyed by window handle, plus the windows the user restored by hand. Pure
/// data -- no OS calls -- so the per-window restore decision
/// ([`restore_with_store`]) is unit testable.
#[derive(Debug, Default)]
pub(super) struct RestoreStore {
    states: std::collections::BTreeMap<isize, RestoreState>,
    /// Windows the user restored with the hotkey: auto-apply and drift
    /// re-apply leave them alone until the window is destroyed or the user
    /// strips it again.
    opted_out: std::collections::BTreeSet<isize>,
}

impl RestoreStore {
    const fn new() -> Self {
        Self {
            states: std::collections::BTreeMap::new(),
            opted_out: std::collections::BTreeSet::new(),
        }
    }

    fn get(&self, window: isize) -> Option<RestoreState> {
        self.states.get(&window).copied()
    }

    /// Records `state` unless the window already has one: the first state
    /// recorded is the window's original bordered look and must survive any
    /// later re-apply.
    ///
    /// A state whose style has no border to strip is not recorded: a window
    /// that is already borderless the first time GameFrame sees it has no
    /// truer original to go back to, and the hotkey must not claim a restore
    /// it cannot do.
    pub(super) fn record_if_absent(&mut self, window: isize, state: RestoreState) {
        // `WS_MAXIMIZE` is not a border: a maximized borderless window has
        // nothing to restore either.
        let borderless = strip_border_style(state.style) == state.style & !WS_MAXIMIZE
            && strip_border_ex_style(state.ex_style) == state.ex_style;
        if borderless {
            return;
        }
        self.states.entry(window).or_insert(state);
    }

    /// Drops everything remembered about `window` (it was destroyed).
    fn forget(&mut self, window: isize) {
        self.states.remove(&window);
        self.opted_out.remove(&window);
    }
}

/// What a deferred re-check re-runs (see [`PlacedWindows::set_recheck`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RecheckKind {
    /// The drift handler, for a window GameFrame placed.
    Drift,
    /// Auto-apply, for a tracked game that was hung when it took the
    /// foreground and so was never placed.
    AutoApply,
}

/// Everything GameFrame remembers per window handle, in one place behind one
/// lock ([`PLACED_WINDOWS`]) so [`Self::forget`] is the single cleanup for a
/// window that is gone: a reused handle can never inherit a stale restore
/// state, auto-apply opt-out, rate-limit time, hung-log entry or timer.
///
/// Pure data (no OS calls) so it is unit testable. The only other per-window
/// state, the reference-counted armed-taskbar set (`crate::taskbar`), is
/// shared across windows (it owns one taskbar restore for all of them), so it
/// stays there; [`crate::launch_watcher`]'s destroy callback releases it next
/// to [`forget_window`].
#[derive(Debug, Default)]
pub(crate) struct PlacedWindows {
    /// Pre-strip restore states and the windows the user restored by hand.
    pub(super) restore: RestoreStore,
    /// Windows the Zone was applied to, with their owning pid (the drift
    /// watcher's registry).
    applied: Vec<(isize, u32)>,
    /// Drift re-apply rate limit, cap and once-only failure logs.
    pub(super) drift: DriftAttempts,
    /// Hung-window skips already logged.
    pub(super) hung: HungSkipLog,
    /// The one pending deferred re-check (thread timer id) per window and kind.
    rechecks: std::collections::BTreeMap<(isize, RecheckKind), usize>,
}

impl PlacedWindows {
    const fn new() -> Self {
        Self {
            restore: RestoreStore::new(),
            applied: Vec::new(),
            drift: DriftAttempts::new(),
            hung: HungSkipLog::new(),
            rechecks: std::collections::BTreeMap::new(),
        }
    }

    /// Drops everything remembered about `window` (it is gone) and returns the
    /// thread timer ids of its pending re-checks, which the caller must kill
    /// (this type makes no OS calls).
    fn forget(&mut self, window: isize) -> Vec<usize> {
        self.restore.forget(window);
        self.applied.retain(|(h, _)| *h != window);
        self.drift.reset(window);
        self.hung.logged.remove(&window);
        let mut timers = Vec::new();
        for kind in [RecheckKind::Drift, RecheckKind::AutoApply] {
            timers.extend(self.rechecks.remove(&(window, kind)));
        }
        timers
    }

    /// Brings the applied-window registry in line with the live windows (see
    /// [`reconcile_entries`]) and fully [`Self::forget`]s every window that
    /// turned out to be dead (killed without an `EVENT_OBJECT_DESTROY`), so its
    /// restore state, rate-limit time and timers go too. Returns the pids
    /// owning applied windows, whether some pid is still unknown, and the
    /// timer ids the caller must kill.
    fn reconcile_applied(
        &mut self,
        probe: impl Fn(isize) -> Option<u32>,
    ) -> (std::collections::BTreeSet<u32>, bool, Vec<usize>) {
        let before: Vec<isize> = self.applied.iter().map(|(h, _)| *h).collect();
        let unresolved = reconcile_entries(&mut self.applied, probe);
        let mut timers = Vec::new();
        for window in before {
            if !self.applied.iter().any(|(h, _)| *h == window) {
                timers.extend(self.forget(window));
            }
        }
        (pids_of(&self.applied), unresolved, timers)
    }

    /// Whether the Zone was applied to `window`.
    fn is_applied(&self, window: isize) -> bool {
        self.applied.iter().any(|(h, _)| *h == window)
    }

    /// Whether a re-check of `kind` is already pending for `window`.
    fn has_recheck(&self, window: isize, kind: RecheckKind) -> bool {
        self.rechecks.contains_key(&(window, kind))
    }

    /// Records `timer` as the pending re-check of `kind` for `window`. Kinds
    /// are tracked separately, so a pending drift re-check never swallows an
    /// auto-apply retry of the same window (or the reverse).
    /// Returns the timer it displaced, if any.
    fn set_recheck(&mut self, window: isize, kind: RecheckKind, timer: usize) -> Option<usize> {
        self.rechecks.insert((window, kind), timer)
    }

    /// Removes and returns the re-check whose thread timer is `timer`.
    fn take_recheck_by_timer(&mut self, timer: usize) -> Option<(isize, RecheckKind)> {
        let key = self
            .rechecks
            .iter()
            .find_map(|(key, id)| (*id == timer).then_some(*key))?;
        self.rechecks.remove(&key);
        Some(key)
    }
}

pub(super) static PLACED_WINDOWS: std::sync::Mutex<PlacedWindows> =
    std::sync::Mutex::new(PlacedWindows::new());

fn lock_store(store: &std::sync::Mutex<PlacedWindows>) -> std::sync::MutexGuard<'_, PlacedWindows> {
    store.lock().unwrap_or_else(|p| p.into_inner())
}

/// Runs `f` on the process-wide [`PlacedWindows`] under its lock. Poison
/// tolerant: the maps and sets hold no invariant a panic could break. Never
/// call anything that pumps messages, or takes this lock, from inside `f`.
pub(super) fn with_placed<R>(f: impl FnOnce(&mut PlacedWindows) -> R) -> R {
    f(&mut lock_store(&PLACED_WINDOWS))
}

/// Whether a re-check of `kind` is already pending for `hwnd`.
pub(crate) fn recheck_pending(hwnd: HWND, kind: RecheckKind) -> bool {
    with_placed(|p| p.has_recheck(hwnd as isize, kind))
}

/// Records `timer` as the pending re-check of `kind` for `hwnd`. Returns the
/// timer it displaced, if a concurrent caller armed one first; the caller
/// must kill it. The timer itself is created and killed outside the registry
/// lock.
pub(crate) fn store_recheck(hwnd: HWND, kind: RecheckKind, timer: usize) -> Option<usize> {
    with_placed(|p| p.set_recheck(hwnd as isize, kind, timer))
}

/// Removes and returns the re-check whose thread timer is `timer`.
pub(crate) fn take_recheck(timer: usize) -> Option<(HWND, RecheckKind)> {
    with_placed(|p| p.take_recheck_by_timer(timer)).map(|(window, kind)| (window as HWND, kind))
}

/// Whether `hwnd` has a remembered pre-strip state, i.e. the next borderless
/// hotkey press on it restores rather than strips.
pub fn has_restore_state(hwnd: HWND) -> bool {
    with_placed(|p| p.restore.get(hwnd as isize).is_some())
}

/// Restores `window` from its remembered pre-strip state through `mutate`; a
/// no-op for a window with none.
///
/// The entry is removed (and the window marked as restored by the user, which
/// keeps auto-apply away from it) only if `mutate` fully succeeded, so a
/// failure partway keeps the only copy of the original for a retry. Other
/// windows' entries never influence the press.
///
/// `store` is locked only around each read/write, never while `mutate` runs.
pub(super) fn restore_with_store<E>(
    store: &std::sync::Mutex<PlacedWindows>,
    window: isize,
    mutate: impl FnOnce(RestoreState) -> Result<(), E>,
) -> Result<(), E> {
    let Some(remembered) = lock_store(store).restore.get(window) else {
        return Ok(());
    };
    mutate(remembered)?;
    let mut guard = lock_store(store);
    guard.restore.states.remove(&window);
    guard.restore.opted_out.insert(window);
    Ok(())
}

/// Whether the user restored `hwnd` with the borderless hotkey, so auto-apply
/// and drift re-apply must leave it alone until it is destroyed or stripped
/// again.
pub fn auto_apply_suppressed(hwnd: HWND) -> bool {
    with_placed(|p| p.restore.opted_out.contains(&(hwnd as isize)))
}

/// Lifts the suppression set by a hotkey restore: the user explicitly asked
/// for `hwnd` to be placed again (tray picker).
pub fn allow_auto_apply(hwnd: HWND) {
    with_placed(|p| {
        p.restore.opted_out.remove(&(hwnd as isize));
    });
}

/// Remembers `hwnd` as one GameFrame applied the Zone to, so the
/// location-change watcher can cheaply (one mutex lock + scan) ignore every
/// other window's move events. The pid is captured now because a destroyed
/// window can no longer be asked for it.
pub fn remember_applied_window(hwnd: HWND) {
    let pid = window_process_id(hwnd);
    with_placed(|p| {
        match p.applied.iter_mut().find(|(h, _)| *h == hwnd as isize) {
            // A reused handle (or one first seen with an unreadable pid) takes
            // the live owner's pid instead of keeping a stale one.
            Some(entry) => {
                if pid != 0 {
                    entry.1 = pid;
                }
            }
            None => p.applied.push((hwnd as isize, pid)),
        }
    });
    crate::launch_watcher::sync_drift_hooks();
}

/// The process id owning `hwnd`, 0 when it can't be read (stale handle).
fn window_process_id(hwnd: HWND) -> u32 {
    let mut pid: u32 = 0;
    // Safety: `&mut pid` is a valid out-pointer; a stale handle just yields 0.
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    pid
}

/// Whether `hwnd` was previously registered via [`remember_applied_window`].
pub fn is_applied_window(hwnd: HWND) -> bool {
    with_placed(|p| p.is_applied(hwnd as isize))
}

/// Process ids owning at least one applied window (never 0).
fn pids_of(entries: &[(isize, u32)]) -> std::collections::BTreeSet<u32> {
    entries
        .iter()
        .map(|(_, pid)| *pid)
        .filter(|pid| *pid != 0)
        .collect()
}

/// Brings registry `entries` in line with the live windows. `probe` maps a
/// window handle to `None` when it is no longer a window, else its owning pid
/// (0 if unreadable). Dead windows (killed without an `EVENT_OBJECT_DESTROY`)
/// are dropped, and an entry whose pid was unknown or changed takes the live
/// one. Returns whether any entry still has no pid, so the caller re-checks later.
fn reconcile_entries(
    entries: &mut Vec<(isize, u32)>,
    probe: impl Fn(isize) -> Option<u32>,
) -> bool {
    entries.retain_mut(|(hwnd, pid)| match probe(*hwnd) {
        None => false,
        Some(live) => {
            if live != 0 {
                *pid = live;
            }
            true
        }
    });
    entries.iter().any(|(_, pid)| *pid == 0)
}

/// Reconciles the applied-window registry with the live windows (see
/// [`reconcile_entries`]) and returns the pids owning applied windows plus
/// whether some window's pid is still unknown.
pub fn refresh_applied_window_pids() -> (std::collections::BTreeSet<u32>, bool) {
    let (pids, unresolved, timers) = with_placed(|p| {
        p.reconcile_applied(|h| {
            let hwnd = h as HWND;
            // Safety: `IsWindow` tolerates any handle value.
            if unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsWindow(hwnd) } == 0 {
                None
            } else {
                Some(window_process_id(hwnd))
            }
        })
    });
    kill_timers(&timers);
    (pids, unresolved)
}

/// Kills the thread timers `timers` (ids this process created with a null
/// window); a no-op for an empty list.
fn kill_timers(timers: &[usize]) {
    for timer in timers {
        // Safety: a thread timer id this process created; `KillTimer` fails
        // harmlessly for one that already fired.
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::KillTimer(std::ptr::null_mut(), *timer);
        }
    }
}

/// Forgets `hwnd` (called when it is destroyed): the single cleanup for
/// everything remembered per window (see [`PlacedWindows::forget`]), including
/// killing its pending re-check timers. The drift hooks are re-synced only if
/// the window was applied (only then can a hook have become unneeded).
pub fn forget_window(hwnd: HWND) {
    let (was_applied, timers) = with_placed(|p| {
        let was_applied = p.is_applied(hwnd as isize);
        (was_applied, p.forget(hwnd as isize))
    });
    kill_timers(&timers);
    if was_applied {
        crate::launch_watcher::sync_drift_hooks();
    }
}

/// Stops the drift watcher treating `hwnd` as GameFrame-placed and drops its
/// drift attempt time.
pub(super) fn unmark_applied_window(hwnd: HWND) {
    with_placed(|p| {
        p.drift.reset(hwnd as isize);
        p.applied.retain(|(h, _)| *h != hwnd as isize);
    });
    crate::launch_watcher::sync_drift_hooks();
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        WS_CAPTION, WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME, WS_EX_WINDOWEDGE, WS_MINIMIZEBOX,
        WS_SYSMENU, WS_THICKFRAME,
    };

    #[test]
    fn a_maximized_window_is_remembered_un_maximized() {
        let state = RestoreState::new(WS_CAPTION | WS_MAXIMIZE, WS_EX_WINDOWEDGE, ORIGINAL);

        assert_eq!(state.style, WS_CAPTION);
        assert_eq!(state.ex_style, WS_EX_WINDOWEDGE);
        assert_eq!(state.region, ORIGINAL);
    }

    #[test]
    fn a_window_with_only_leftover_edges_still_gets_a_restore_state() {
        let mut store = RestoreStore::new();
        let state = RestoreState::new(0, WS_EX_CLIENTEDGE, ORIGINAL);

        store.record_if_absent(1, state);

        assert_eq!(store.get(1), Some(state));
    }

    #[test]
    fn a_maximized_but_borderless_window_gets_no_restore_state() {
        let mut store = RestoreStore::new();

        store.record_if_absent(1, RestoreState::new(WS_MAXIMIZE, 0, ORIGINAL));

        assert_eq!(store.get(1), None);
    }

    #[test]
    fn the_ex_style_round_trips_through_a_restore() {
        let store = std::sync::Mutex::new(PlacedWindows::new());
        let ex = WS_EX_CLIENTEDGE | WS_EX_DLGMODALFRAME;
        let state = RestoreState::new(BORDERED, ex, ORIGINAL);
        lock_store(&store).restore.record_if_absent(1, state);

        let seen = restore(&store, 1, Ok(()));

        assert_eq!(seen, vec![state]);
    }

    #[test]
    fn two_windows_of_one_process_keep_its_pid_when_one_is_removed() {
        let mut placed = PlacedWindows::new();
        placed.applied = vec![(1isize, 42u32), (2, 42), (3, 7)];
        assert_eq!(pids_of(&placed.applied), [7, 42].into_iter().collect());
        placed.forget(1);
        assert_eq!(pids_of(&placed.applied), [7, 42].into_iter().collect());
        placed.forget(2);
        assert_eq!(pids_of(&placed.applied), [7].into_iter().collect());
    }

    #[test]
    fn is_applied_is_false_once_the_window_is_forgotten() {
        let mut placed = PlacedWindows::new();
        placed.applied = vec![(1isize, 42u32)];
        assert!(placed.is_applied(1));
        assert!(!placed.is_applied(2));
        placed.forget(1);
        assert!(!placed.is_applied(1));
    }

    #[test]
    fn unknown_pids_never_count_as_a_process() {
        assert!(pids_of(&[(1, 0)]).is_empty());
    }

    #[test]
    fn reconcile_drops_dead_windows() {
        let mut entries = vec![(1isize, 42u32), (2, 43)];
        let unresolved = reconcile_entries(&mut entries, |h| (h == 2).then_some(43));
        assert_eq!(entries, vec![(2, 43)]);
        assert!(!unresolved);
    }

    #[test]
    fn reconcile_fills_in_a_pid_that_was_unreadable_before() {
        let mut entries = vec![(1isize, 0u32)];
        assert!(!reconcile_entries(&mut entries, |_| Some(42)));
        assert_eq!(entries, vec![(1, 42)]);
    }

    #[test]
    fn reconcile_reports_a_pid_that_is_still_unreadable() {
        let mut entries = vec![(1isize, 0u32)];
        assert!(reconcile_entries(&mut entries, |_| Some(0)));
        assert_eq!(entries, vec![(1, 0)]);
    }

    #[test]
    fn reconcile_follows_a_reused_handle_to_its_new_owner() {
        let mut entries = vec![(1isize, 42u32)];
        reconcile_entries(&mut entries, |_| Some(99));
        assert_eq!(entries, vec![(1, 99)]);
    }

    const BORDERED: u32 = WS_CAPTION | WS_THICKFRAME | WS_SYSMENU;
    const ORIGINAL: Region = Region {
        x: 10,
        y: 20,
        width: 800,
        height: 600,
    };
    const TARGET: Region = Region {
        x: 0,
        y: 0,
        width: 960,
        height: 1080,
    };

    /// Runs one restore of `window`; returns the states `mutate` was asked to
    /// restore.
    fn restore(
        store: &std::sync::Mutex<PlacedWindows>,
        window: isize,
        mutate_result: Result<(), ()>,
    ) -> Vec<RestoreState> {
        let mut seen = Vec::new();
        let _ = restore_with_store(store, window, |state| {
            seen.push(state);
            mutate_result
        });
        seen
    }

    fn remembered(store: &std::sync::Mutex<PlacedWindows>, window: isize) {
        lock_store(store).restore.record_if_absent(
            window,
            RestoreState {
                style: BORDERED,
                ex_style: 0,
                region: ORIGINAL,
            },
        );
    }

    #[test]
    fn restore_with_store_restores_a_stripped_window_and_forgets_it() {
        let store = std::sync::Mutex::new(PlacedWindows::new());
        remembered(&store, 1);

        let seen = restore(&store, 1, Ok(()));

        assert_eq!(
            seen,
            vec![RestoreState {
                style: BORDERED,
                ex_style: 0,
                region: ORIGINAL
            }]
        );
        assert_eq!(lock_store(&store).restore.get(1), None);
    }

    #[test]
    fn restore_with_store_does_nothing_for_a_window_it_has_no_state_for() {
        let store = std::sync::Mutex::new(PlacedWindows::new());

        let seen = restore(&store, 1, Ok(()));

        assert!(seen.is_empty());
        assert!(!lock_store(&store).restore.opted_out.contains(&1));
    }

    #[test]
    fn restore_with_store_keeps_the_entry_when_a_restore_fails() {
        let store = std::sync::Mutex::new(PlacedWindows::new());
        remembered(&store, 1);

        restore(&store, 1, Err(()));

        assert!(lock_store(&store).restore.get(1).is_some());
    }

    #[test]
    fn restore_with_store_treats_windows_independently() {
        let store = std::sync::Mutex::new(PlacedWindows::new());
        remembered(&store, 1);
        remembered(&store, 2);

        restore(&store, 2, Ok(()));

        assert!(lock_store(&store).restore.get(1).is_some());
        assert!(lock_store(&store).restore.get(2).is_none());
    }

    #[test]
    fn record_if_absent_does_not_overwrite_the_original_state() {
        let mut store = RestoreStore::new();
        let original = RestoreState {
            style: BORDERED,
            ex_style: 0,
            region: ORIGINAL,
        };
        store.record_if_absent(1, original);

        store.record_if_absent(
            1,
            RestoreState {
                style: BORDERED | WS_MINIMIZEBOX,
                ex_style: 0,
                region: TARGET,
            },
        );

        assert_eq!(store.get(1), Some(original));
    }

    #[test]
    fn record_if_absent_ignores_a_window_that_is_already_borderless() {
        let mut store = RestoreStore::new();

        store.record_if_absent(
            1,
            RestoreState {
                style: strip_border_style(BORDERED),
                ex_style: 0,
                region: ORIGINAL,
            },
        );

        assert_eq!(store.get(1), None);
    }

    #[test]
    fn a_hotkey_restore_suppresses_auto_apply_until_it_is_lifted() {
        let store = std::sync::Mutex::new(PlacedWindows::new());
        remembered(&store, 1);

        restore(&store, 1, Ok(()));

        assert!(lock_store(&store).restore.opted_out.contains(&1));
    }

    #[test]
    fn a_failed_restore_does_not_suppress_auto_apply() {
        let store = std::sync::Mutex::new(PlacedWindows::new());
        remembered(&store, 1);

        restore(&store, 1, Err(()));

        assert!(!lock_store(&store).restore.opted_out.contains(&1));
    }

    #[test]
    fn forgetting_a_window_clears_its_suppression() {
        let store = std::sync::Mutex::new(PlacedWindows::new());
        remembered(&store, 1);
        restore(&store, 1, Ok(()));

        lock_store(&store).forget(1);

        assert!(!lock_store(&store).restore.opted_out.contains(&1));
    }

    /// A registry with every kind of per-window state set for `window`.
    fn fully_populated(window: isize) -> PlacedWindows {
        let mut placed = PlacedWindows::new();
        placed.restore.record_if_absent(
            window,
            RestoreState {
                style: BORDERED,
                ex_style: 0,
                region: ORIGINAL,
            },
        );
        placed.restore.opted_out.insert(window);
        placed.applied.push((window, 42));
        placed.drift.record(window, std::time::Instant::now());
        placed.drift.note_failure(window);
        placed.hung.note(window, true);
        placed.set_recheck(window, RecheckKind::Drift, 7);
        placed.set_recheck(window, RecheckKind::AutoApply, 8);
        placed
    }

    #[test]
    fn forget_clears_every_kind_of_per_window_state_and_returns_the_timers() {
        let mut placed = fully_populated(1);
        placed.restore.record_if_absent(
            2,
            RestoreState {
                style: BORDERED,
                ex_style: 0,
                region: ORIGINAL,
            },
        );

        let mut timers = placed.forget(1);
        timers.sort_unstable();

        assert_eq!(timers, [7, 8]);
        assert!(placed.restore.get(1).is_none());
        assert!(placed.restore.opted_out.is_empty());
        assert!(placed.applied.is_empty());
        assert!(placed.drift.recent.is_empty());
        assert!(placed.drift.failure_logged.is_empty());
        assert!(placed.hung.logged.is_empty());
        assert!(!placed.has_recheck(1, RecheckKind::Drift));
        assert!(!placed.has_recheck(1, RecheckKind::AutoApply));
        assert!(placed.restore.get(2).is_some(), "other windows are kept");
    }

    #[test]
    fn a_pending_drift_recheck_does_not_swallow_an_auto_apply_retry() {
        let mut placed = PlacedWindows::new();
        placed.set_recheck(1, RecheckKind::Drift, 7);

        assert!(!placed.has_recheck(1, RecheckKind::AutoApply));
        placed.set_recheck(1, RecheckKind::AutoApply, 8);

        assert_eq!(
            placed.take_recheck_by_timer(8),
            Some((1, RecheckKind::AutoApply))
        );
        assert!(placed.has_recheck(1, RecheckKind::Drift));
        assert_eq!(placed.take_recheck_by_timer(8), None);
    }

    #[test]
    fn reconciling_drops_a_dead_window_with_all_its_state() {
        let mut placed = fully_populated(1);
        placed.applied.push((2, 43));

        let (pids, unresolved, mut timers) = placed.reconcile_applied(|h| (h == 2).then_some(43));
        timers.sort_unstable();

        assert_eq!(pids, [43].into_iter().collect());
        assert!(!unresolved);
        assert_eq!(timers, [7, 8]);
        assert!(placed.restore.get(1).is_none());
        assert!(placed.restore.opted_out.is_empty());
        assert!(placed.drift.recent.is_empty());
        assert!(placed.hung.logged.is_empty());
        assert_eq!(placed.applied, [(2, 43)]);
    }
}
