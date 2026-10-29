//! Detects a tracked game launching while GameFrame is idle in the tray via
//! a `SetWinEventHook(WINEVENT_OUTOFCONTEXT)` registration on
//! `EVENT_SYSTEM_FOREGROUND`, and reacts by applying the Zone and
//! taskbar-auto-hide state the same way the hotkey/tray "Apply to Window"
//! paths do -- with no hotkey press required. A WinEvent hook is used
//! rather than a toolhelp poll loop or a WMI process-creation subscription.
//!
//! # Registration
//!
//! [`register`] must be called once, on the same thread that runs
//! [`crate::app::run_event_loop`]'s `GetMessageW` pump, **before** that
//! thread enters the pump -- per the official docs, "the client thread that
//! calls `SetWinEventHook` must have a message loop." GameFrame's tray-idle
//! thread already satisfies this, so `WINEVENT_OUTOFCONTEXT` delivers
//! events as ordinary messages pumped through the loop that already exists
//! for the tray icon/hotkeys -- no dedicated hidden message-only window or
//! second thread is needed. [`crate::app::run`] calls [`register`] before
//! [`crate::app::run_event_loop`] and [`unregister`] after it returns (the
//! "Exit" path), for clean teardown -- not strictly load-bearing since the
//! process exits right after, but matching this codebase's existing care
//! around explicit teardown (see [`crate::hotkeys::Hotkeys`]'s `Drop` impl
//! unregistering hotkeys).
//!
//! # No user-data parameter
//!
//! The callback signature Windows requires
//! (`unsafe extern "system" fn(HWINEVENTHOOK, u32, HWND, i32, i32, u32,
//! u32)`) has no user-data/lparam parameter, unlike `EnumWindows`'s
//! callback (see [`crate::win32::enum_windows_callback`]) -- so it cannot
//! receive the config directory as a captured closure or a passed-in
//! pointer. Instead, the config directory (which never changes during the
//! process's lifetime) is stashed once, in [`register`], into a
//! process-wide [`OnceLock`], and read back by the callback on every event.
//! The config itself is deliberately *not* stored here -- the callback
//! reads the current (cached) config on every foreground-change event (via
//! `profile_store::load`), as every [`crate::placement::apply_zone`] trigger does.
//!
//! # Cargo features
//!
//! `SetWinEventHook`/`UnhookWinEvent`/`HWINEVENTHOOK` live in
//! `windows_sys::Win32::UI::Accessibility`, gated by the
//! `Win32_UI_Accessibility` Cargo feature. The event constants
//! (`EVENT_SYSTEM_FOREGROUND`, `WINEVENT_OUTOFCONTEXT`) live in
//! `Win32_UI_WindowsAndMessaging`.
//!
//! # Testing
//!
//! OS-facing throughout (`SetWinEventHook`/`UnhookWinEvent` registration and
//! the real callback) -- not exercised by this crate's automated tests, per
//! this crate's convention for OS-facing modules like `tray_ui`/`app`
//! (verified by hand only). The match-against-tracked-games decision the
//! callback delegates to ([`crate::placement::resolve_auto_apply`], via
//! [`crate::placement::handle_auto_apply_launch`]) is the pure, unit-tested
//! part -- see the tests in `crate::placement::tests`.
//!
//! # Taskbar auto-restore on window close
//!
//! [`register_taskbar_restore_watcher`] registers a **second**,
//! independent `SetWinEventHook` scoped to exactly `EVENT_OBJECT_DESTROY`,
//! rather than widening [`register`]'s existing `EVENT_SYSTEM_FOREGROUND`
//! hook's event range to also cover it: `EVENT_OBJECT_DESTROY` (`0x8001`)
//! sits far away from `EVENT_SYSTEM_FOREGROUND` (`0x0003`) in the WinEvent
//! constant space, and `SetWinEventHook`'s `eventMin`/`eventMax` pair
//! delivers every event *in that whole numeric range*, not just the two
//! endpoints -- widening would flood the callback with a large number of
//! unrelated events it would then have to filter, extra dispatch overhead
//! that an unrelated window closing must not cause. A second,
//! narrowly-scoped hook (mirroring [`register`]'s own single-event
//! `hEventMin == hEventMax` range) avoids that entirely.
//!
//! Unlike `EVENT_SYSTEM_FOREGROUND` (already scoped to real top-level
//! windows per [`win_event_callback`]'s own doc comment),
//! `EVENT_OBJECT_DESTROY` fires for many kinds of UI objects being
//! destroyed, not just top-level windows -- per the constant's documented
//! semantics, [`window_destroy_callback`] additionally checks `idObject ==
//! OBJID_WINDOW` and `idChild == CHILDID_SELF`, the standard filter for "a
//! real top-level window itself was destroyed" (as opposed to one of its
//! child UI objects).
//!
//! The callback delegates the actual "does this closed window match
//! whatever's currently armed" decision, and the restore itself, to
//! [`crate::taskbar::take_armed_taskbar_restore_for_window`]/
//! [`crate::taskbar::restore_taskbar`] -- see those functions' doc comments,
//! and `crate::taskbar`'s private `ARMED_TASKBAR_RESTORE` static, for the
//! armed-state design (including why an unrelated window's destroy event
//! is a cheap
//! no-op: a mutex lock plus one pointer comparison, no OS call at all,
//! unless a restore is actually due).

use crate::win32::RecheckKind;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, EVENT_OBJECT_DESTROY, EVENT_OBJECT_LOCATIONCHANGE, EVENT_SYSTEM_FOREGROUND,
    KillTimer, OBJID_WINDOW, SetTimer, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
};

/// The config directory the callback reads the config from on every
/// foreground-change event -- set once, from [`register`], before the hook
/// is registered; never mutated afterward. See module docs ("No user-data
/// parameter") for why this is a `static` rather than a parameter threaded
/// through the callback.
static WATCHED_CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Registers the `EVENT_SYSTEM_FOREGROUND` WinEvent hook on the calling
/// thread. Must be called before that thread enters its `GetMessageW` pump
/// -- see module docs.
///
/// Returns `None` if `SetWinEventHook` fails (returns a null handle),
/// treated as non-fatal by the caller: matches this codebase's existing
/// "hotkey registration failure doesn't stop the app from starting" pattern
/// ([`crate::app::register_hotkeys`]) -- GameFrame still starts, just
/// without automatic launch detection.
///
/// OS-facing: registers a real Win32 hook. Not exercised by this crate's
/// automated tests -- see module docs.
pub fn register(config_dir: &Path) -> Option<HWINEVENTHOOK> {
    // `get_or_init` rather than a bare `set` so a hypothetical second call
    // (there should only ever be one, from `crate::app::run`) can't panic --
    // it just keeps whichever directory was set first.
    WATCHED_CONFIG_DIR.get_or_init(|| config_dir.to_path_buf());

    // Safety: `win_event_callback` matches the required `WINEVENTPROC`
    // signature exactly; `idProcess = 0, idThread = 0` scopes to every
    // process/thread on the desktop, which is unavoidable here -- the
    // target game process doesn't exist yet at registration time, so
    // filtering happens inside the callback instead (see module docs).
    let hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            std::ptr::null_mut(),
            Some(win_event_callback),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };

    if hook.is_null() { None } else { Some(hook) }
}

/// Tears down a hook returned by [`register`]. Called when
/// [`crate::app::run_event_loop`] returns (the "Exit" path) -- see module
/// docs for why this is worth doing even though the process exits right
/// after.
///
/// OS-facing: unregisters a real Win32 hook. Not exercised by this crate's
/// automated tests -- see module docs.
pub fn unregister(hook: HWINEVENTHOOK) {
    // Safety: `hook` is a handle this module obtained from `SetWinEventHook`
    // in `register` and hasn't already been unhooked.
    unsafe {
        UnhookWinEvent(hook);
    }
}

/// Registers the `EVENT_OBJECT_DESTROY` WinEvent hook on the calling
/// thread -- the auto-restore-on-close watcher. Must be called
/// before that thread enters its `GetMessageW` pump, same requirement (and
/// same reasoning) as [`register`]; [`crate::app::run`] calls this
/// alongside [`register`], both before [`crate::app::run_event_loop`].
///
/// Returns `None` if `SetWinEventHook` fails, treated as non-fatal by the
/// caller -- same "hotkey/hook registration failure doesn't stop the app
/// from starting" pattern as [`register`] and
/// [`crate::app::register_hotkeys`]: GameFrame still starts, just without
/// automatic taskbar restore on window close (the manual hotkey-restore
/// path, and the tray-Exit check in [`crate::app::run`], remain available
/// regardless).
///
/// A second, independent hook rather than widening [`register`]'s own
/// range -- see this module's doc comment ("Taskbar auto-restore on window
/// close") for why.
///
/// OS-facing: registers a real Win32 hook. Not exercised by this crate's
/// automated tests -- see module docs.
pub fn register_taskbar_restore_watcher() -> Option<HWINEVENTHOOK> {
    // Safety: `window_destroy_callback` matches the required `WINEVENTPROC`
    // signature exactly; `idProcess = 0, idThread = 0` scopes to every
    // process/thread on the desktop, same as `register` above -- the
    // window that will eventually close isn't known at registration time.
    let hook = unsafe {
        SetWinEventHook(
            EVENT_OBJECT_DESTROY,
            EVENT_OBJECT_DESTROY,
            std::ptr::null_mut(),
            Some(window_destroy_callback),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };

    if hook.is_null() { None } else { Some(hook) }
}

/// The real `WINEVENTPROC` Windows calls on every `EVENT_OBJECT_DESTROY`,
/// delivered on the calling thread's message loop (see [`register_taskbar_restore_watcher`]).
/// Filters defensively (a null `hwnd`), then requires `idobject ==
/// OBJID_WINDOW` and `idchild == CHILDID_SELF` -- unlike
/// `EVENT_SYSTEM_FOREGROUND` ([`win_event_callback`]'s own doc comment),
/// `EVENT_OBJECT_DESTROY` fires for many non-top-level-window UI objects
/// too, so this extra check is required here to scope down to "a real
/// top-level window itself was just destroyed" (see this module's doc
/// comment).
///
/// Delegates the actual armed-window match + restore to
/// [`crate::taskbar::take_armed_taskbar_restore_for_window`]/
/// [`crate::taskbar::restore_taskbar`] -- an unrelated window's destroy event
/// costs one mutex lock and one pointer comparison inside that call, no
/// real Win32 call at all, so the common case adds no measurable cost.
///
/// OS-facing, and the real hook-delivery path is verified by hand only --
/// not exercised by this crate's automated tests (see
/// `crate::taskbar::tests`' `set_taskbar_hide_absolute_armed_then_take_armed_taskbar_restore_for_window_restores_the_real_taskbar_on_a_simulated_window_close`
/// for the closest automated equivalent: it exercises the exact same two
/// calls this callback makes, against the real taskbar, without needing a
/// real second window's `EVENT_OBJECT_DESTROY` to actually fire).
unsafe extern "system" fn window_destroy_callback(
    _hwineventhook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    idobject: i32,
    idchild: i32,
    _ideventthread: u32,
    _dwmseventtime: u32,
) {
    if hwnd.is_null() || idobject != OBJID_WINDOW || idchild != CHILDID_SELF as i32 {
        return;
    }

    let _crumb = crate::watchdog::scoped("destroy hook");
    crate::win32::forget_window(hwnd);

    if let Some(restore) = crate::taskbar::take_armed_taskbar_restore_for_window(hwnd) {
        log!("taskbar auto-restore: armed window {hwnd:?} closed, restoring {restore:?}");
        if let Err(e) = crate::taskbar::restore_taskbar(restore) {
            log!("taskbar auto-restore on window close failed: {e}");
        }
    }
}

/// What [`sync_drift_hooks`] must do to turn the hooks that exist into the
/// ones wanted: one location hook per applied game process, none otherwise.
#[derive(Debug, Default, PartialEq, Eq)]
struct DriftHookPlan {
    register: Vec<u32>,
    unregister: Vec<u32>,
}

/// Pure registration decision. `applied` are the pids owning applied windows,
/// `hooked` the pids that currently have a hook. GameFrame's own pid never
/// gets one (its windows are never applied, and the hook skips it anyway).
fn plan_drift_hooks(
    applied: &std::collections::BTreeSet<u32>,
    hooked: &std::collections::BTreeSet<u32>,
    own_pid: u32,
) -> DriftHookPlan {
    DriftHookPlan {
        register: applied
            .iter()
            .copied()
            .filter(|pid| *pid != own_pid && !hooked.contains(pid))
            .collect(),
        unregister: hooked
            .iter()
            .copied()
            .filter(|pid| *pid == own_pid || !applied.contains(pid))
            .collect(),
    }
}

/// Whether the drift watcher is armed (set by [`enable_drift_watcher`] on the
/// message-loop thread); keeps registry changes from hooking anything before
/// the pump exists or after teardown.
static DRIFT_WATCHER_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Game process id -> its `EVENT_OBJECT_LOCATIONCHANGE` hook handle.
///
/// The mutex is held across `SetWinEventHook`/`UnhookWinEvent`. That is safe
/// because every caller runs on the message-loop thread (asserted in
/// [`sync_drift_hooks`]), the drift callback never takes this lock, and
/// out-of-context hook callbacks are only delivered by that thread's
/// `DispatchMessageW`, never re-entrantly from inside those calls.
static DRIFT_HOOKS: std::sync::Mutex<std::collections::BTreeMap<u32, isize>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Id of the message-loop thread, recorded by [`enable_drift_watcher`].
static DRIFT_THREAD_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// How long a failed hook registration or an unreadable pid waits before
/// [`sync_drift_hooks`] tries again.
const DRIFT_SYNC_RETRY: std::time::Duration = std::time::Duration::from_secs(5);

/// How often [`sync_drift_hooks`] re-runs while any hook exists, so hooks of
/// processes killed without a destroy event are dropped.
const DRIFT_SYNC_SWEEP: std::time::Duration = std::time::Duration::from_secs(60);

/// Fast retries of an unreadable window pid before leaving it to the sweep.
const DRIFT_UNRESOLVED_RETRY_LIMIT: u32 = 5;

/// Consecutive [`sync_drift_hooks`] runs that ended with an unreadable pid.
static DRIFT_UNRESOLVED_RUNS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// When the next [`sync_drift_hooks`] run is due, and the new count of
/// consecutive runs with an unreadable pid.
#[derive(Debug, PartialEq, Eq)]
struct NextSync {
    after: Option<std::time::Duration>,
    unresolved_runs: u32,
}

/// Pure scheduling decision. A failed hook registration is retried every
/// [`DRIFT_SYNC_RETRY`]; an unreadable pid likewise, but only
/// [`DRIFT_UNRESOLVED_RETRY_LIMIT`] runs in a row. Otherwise, while any hook
/// exists or a pid is still unreadable, the run repeats every
/// [`DRIFT_SYNC_SWEEP`]; with neither, never.
fn plan_next_sync(
    register_failed: bool,
    unresolved: bool,
    previous_unresolved_runs: u32,
    hooks_exist: bool,
) -> NextSync {
    let unresolved_runs = if unresolved {
        previous_unresolved_runs.saturating_add(1)
    } else {
        0
    };
    let retry_unresolved = unresolved && unresolved_runs <= DRIFT_UNRESOLVED_RETRY_LIMIT;
    let after = if register_failed || retry_unresolved {
        Some(DRIFT_SYNC_RETRY)
    } else if hooks_exist || unresolved {
        Some(DRIFT_SYNC_SWEEP)
    } else {
        None
    };
    NextSync {
        after,
        unresolved_runs,
    }
}

/// Thread timer id of the pending [`sync_drift_hooks`] retry or sweep, 0 if none.
static DRIFT_SYNC_TIMER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Arms the drift watcher. Hooks are then registered per applied game process
/// by [`sync_drift_hooks`], which must run on this same (message-loop) thread;
/// no hook exists while nothing is applied. OS-facing.
pub fn enable_drift_watcher() {
    // Safety: no arguments or preconditions.
    let thread = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
    DRIFT_THREAD_ID.store(thread, std::sync::atomic::Ordering::SeqCst);
    DRIFT_WATCHER_ENABLED.store(true, std::sync::atomic::Ordering::SeqCst);
    sync_drift_hooks();
}

/// Disarms the watcher and unhooks everything; the final [`sync_drift_hooks`]
/// run also cancels the pending retry or sweep timer. Call on the
/// message-loop thread.
pub fn disable_drift_watcher() {
    DRIFT_WATCHER_ENABLED.store(false, std::sync::atomic::Ordering::SeqCst);
    sync_drift_hooks();
}

/// Reconciles the location-change hooks with the applied-window registry so a
/// game that moves, resizes or re-adds its border after GameFrame applied the
/// Zone (e.g. Last Epoch after an in-game settings change) is put back, while
/// the rest of the desktop's move events never reach this process. Each hook is
/// scoped to one game pid with `WINEVENT_SKIPOWNPROCESS`. Same
/// thread/message-loop requirements as [`register`]. If a registration fails or
/// a window's pid can't be read, a timer re-runs this after
/// [`DRIFT_SYNC_RETRY`] (an unreadable pid only [`DRIFT_UNRESOLVED_RETRY_LIMIT`]
/// times in a row); while any hook exists a [`DRIFT_SYNC_SWEEP`] timer re-runs
/// it to drop hooks of exited processes. OS-facing.
pub fn sync_drift_hooks() {
    let enabled = DRIFT_WATCHER_ENABLED.load(std::sync::atomic::Ordering::SeqCst);
    if enabled {
        // Safety: no arguments or preconditions.
        let here = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
        debug_assert_eq!(
            here,
            DRIFT_THREAD_ID.load(std::sync::atomic::Ordering::SeqCst),
            "sync_drift_hooks must run on the message-loop thread"
        );
    }
    let (applied, unresolved) = if enabled {
        crate::win32::refresh_applied_window_pids()
    } else {
        (std::collections::BTreeSet::new(), false)
    };
    let mut register_failed = false;
    let mut hooks = DRIFT_HOOKS.lock().unwrap_or_else(|p| p.into_inner());
    let hooked = hooks.keys().copied().collect();
    let plan = plan_drift_hooks(&applied, &hooked, std::process::id());
    for pid in plan.unregister {
        if let Some(hook) = hooks.remove(&pid) {
            // Safety: a handle this module obtained from `SetWinEventHook`.
            unsafe {
                UnhookWinEvent(hook as HWINEVENTHOOK);
            }
        }
    }
    for pid in plan.register {
        // Safety: `drift_callback` matches the `WINEVENTPROC` signature; the
        // hook is scoped to game `pid` (all its threads) and skips this process.
        let hook = unsafe {
            SetWinEventHook(
                EVENT_OBJECT_LOCATIONCHANGE,
                EVENT_OBJECT_LOCATIONCHANGE,
                std::ptr::null_mut(),
                Some(drift_callback),
                pid,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        if hook.is_null() {
            log!("drift watcher: SetWinEventHook failed for pid {pid}; retrying shortly");
            register_failed = true;
        } else {
            hooks.insert(pid, hook as isize);
        }
    }
    let hooks_exist = !hooks.is_empty();
    drop(hooks);
    use std::sync::atomic::Ordering;
    let next = plan_next_sync(
        register_failed,
        unresolved,
        DRIFT_UNRESOLVED_RUNS.load(Ordering::SeqCst),
        hooks_exist,
    );
    DRIFT_UNRESOLVED_RUNS.store(next.unresolved_runs, Ordering::SeqCst);
    schedule_drift_sync(next.after);
}

/// Replaces the one pending [`sync_drift_hooks`] timer (retry or sweep) with
/// one firing after `after`; with `None` only cancels it.
fn schedule_drift_sync(after: Option<std::time::Duration>) {
    use std::sync::atomic::Ordering;
    let previous = DRIFT_SYNC_TIMER.swap(0, Ordering::SeqCst);
    if previous != 0 {
        // Safety: a thread timer id this module created.
        unsafe {
            KillTimer(std::ptr::null_mut(), previous);
        }
    }
    let Some(after) = after else {
        return;
    };
    let delay_ms = u32::try_from(after.as_millis()).unwrap_or(u32::MAX);
    // Safety: a null window makes this a thread timer on the message-loop
    // thread, delivered to `drift_sync_timer` by `DispatchMessageW`.
    let timer = unsafe { SetTimer(std::ptr::null_mut(), 0, delay_ms, Some(drift_sync_timer)) };
    if timer == 0 {
        log!("drift watcher: SetTimer failed; hooks are retried on the next window change");
    } else {
        DRIFT_SYNC_TIMER.store(timer, Ordering::SeqCst);
    }
}

/// `TIMERPROC` for the [`sync_drift_hooks`] retry and sweep. OS-facing; not unit tested.
unsafe extern "system" fn drift_sync_timer(_hwnd: HWND, _msg: u32, timer: usize, _time: u32) {
    // Safety: `timer` is a thread timer id this module created.
    unsafe {
        KillTimer(std::ptr::null_mut(), timer);
    }
    DRIFT_SYNC_TIMER.store(0, std::sync::atomic::Ordering::SeqCst);
    let _crumb = crate::watchdog::scoped("drift timer: hook sync");
    sync_drift_hooks();
}

/// `EVENT_OBJECT_LOCATIONCHANGE` fires very often system-wide, so this
/// filters to `OBJID_WINDOW`/`CHILDID_SELF` and then to windows GameFrame
/// itself applied the Zone to (one mutex lock + scan) before doing any real
/// work. OS-facing; not unit tested.
unsafe extern "system" fn drift_callback(
    _hwineventhook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    idobject: i32,
    idchild: i32,
    _ideventthread: u32,
    _dwmseventtime: u32,
) {
    if hwnd.is_null() || idobject != OBJID_WINDOW || idchild != CHILDID_SELF as i32 {
        return;
    }
    if !crate::win32::is_applied_window(hwnd) {
        return;
    }
    let Some(config_dir) = WATCHED_CONFIG_DIR.get() else {
        return;
    };
    let _crumb = crate::watchdog::scoped("drift hook: re-check");
    check_applied_window_drift(config_dir, hwnd);
}

/// Runs the drift handler for `hwnd`; if it was only rate-limited or the
/// window is hung, schedules one re-check for when the limit expires or after
/// the hung re-check interval (at most one pending per window).
fn check_applied_window_drift(config_dir: &Path, hwnd: HWND) {
    if let Some(retry_in) = crate::placement::handle_applied_window_drift(config_dir, hwnd) {
        schedule_recheck(hwnd, retry_in, RecheckKind::Drift);
    }
}

/// Runs auto-apply for `hwnd`; a hung game is retried after the hung re-check
/// interval, reusing the drift re-check timer (until it responds or is
/// destroyed -- a hung game produces no further foreground events).
fn check_auto_apply(config_dir: &Path, hwnd: HWND) {
    if let Some(retry_in) = crate::placement::handle_auto_apply_launch(config_dir, hwnd) {
        schedule_recheck(hwnd, retry_in, RecheckKind::AutoApply);
    }
}

/// Arms the one pending re-check of `kind` for `hwnd`, if none is armed yet.
/// A drift re-check and an auto-apply retry of the same window are separate
/// timers, so neither swallows the other. The timers live in the per-window
/// registry, which kills them when the window is forgotten.
fn schedule_recheck(hwnd: HWND, retry_in: std::time::Duration, kind: RecheckKind) {
    if crate::win32::recheck_pending(hwnd, kind) {
        return;
    }
    let delay_ms = u32::try_from(retry_in.as_millis())
        .unwrap_or(u32::MAX)
        .saturating_add(1);
    // SAFETY: a null window makes this a thread timer on the message-loop
    // thread, delivered to `drift_recheck_timer` by `DispatchMessageW`. The
    // registry lock is not held here.
    let timer = unsafe { SetTimer(std::ptr::null_mut(), 0, delay_ms, Some(drift_recheck_timer)) };
    if timer == 0 {
        log!("drift re-check: SetTimer failed; the drift is corrected on its next event");
    } else if let Some(displaced) = crate::win32::store_recheck(hwnd, kind, timer) {
        // SAFETY: a thread timer id this module created.
        unsafe {
            KillTimer(std::ptr::null_mut(), displaced);
        }
    }
}

/// `TIMERPROC` for a deferred drift re-check: re-runs the drift decision for
/// the window the timer was scheduled for. OS-facing; not unit tested.
unsafe extern "system" fn drift_recheck_timer(_hwnd: HWND, _msg: u32, timer: usize, _time: u32) {
    // Safety: `timer` is a thread timer id this module created.
    unsafe {
        KillTimer(std::ptr::null_mut(), timer);
    }
    let Some((hwnd, kind)) = crate::win32::take_recheck(timer) else {
        return;
    };
    let Some(config_dir) = WATCHED_CONFIG_DIR.get() else {
        return;
    };
    match kind {
        RecheckKind::Drift => {
            let _crumb = crate::watchdog::scoped("drift timer: re-check");
            if crate::win32::is_applied_window(hwnd) {
                check_applied_window_drift(config_dir, hwnd);
            }
        }
        RecheckKind::AutoApply => {
            let _crumb = crate::watchdog::scoped("drift timer: auto-apply retry");
            check_auto_apply(config_dir, hwnd);
        }
    }
}

/// The real `WINEVENTPROC` Windows calls on every `EVENT_SYSTEM_FOREGROUND`
/// change, delivered on the calling thread's message loop (see module
/// docs). Filters defensively (a null `hwnd`) before delegating the actual
/// match-against-tracked-games decision and any resulting apply to
/// [`crate::placement::handle_auto_apply_launch`].
///
/// `idobject`/`idchild`/`ideventthread`/`dwmseventtime` are unused:
/// `EVENT_SYSTEM_FOREGROUND`'s own documented semantics are specifically
/// about the foreground window changing (unlike `EVENT_OBJECT_CREATE`,
/// which fires for many non-window UI objects), so no `idObject ==
/// OBJID_WINDOW` check is needed here -- this event is already scoped to
/// real top-level windows.
///
/// OS-facing, and the real hook-delivery path is verified by hand only --
/// not exercised by this crate's automated tests.
unsafe extern "system" fn win_event_callback(
    _hwineventhook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _idobject: i32,
    _idchild: i32,
    _ideventthread: u32,
    _dwmseventtime: u32,
) {
    if hwnd.is_null() {
        return;
    }

    let Some(config_dir) = WATCHED_CONFIG_DIR.get() else {
        return;
    };

    let _crumb = crate::watchdog::scoped("foreground hook: auto-apply");
    check_auto_apply(config_dir, hwnd);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn set(pids: &[u32]) -> BTreeSet<u32> {
        pids.iter().copied().collect()
    }

    #[test]
    fn next_sync_sweeps_slowly_while_a_hook_exists_and_stops_without_one() {
        assert_eq!(
            plan_next_sync(false, false, 0, true),
            NextSync {
                after: Some(DRIFT_SYNC_SWEEP),
                unresolved_runs: 0
            }
        );
        assert_eq!(plan_next_sync(false, false, 0, false).after, None);
    }

    #[test]
    fn next_sync_retries_a_failed_registration_quickly() {
        assert_eq!(
            plan_next_sync(true, false, 0, false).after,
            Some(DRIFT_SYNC_RETRY)
        );
    }

    #[test]
    fn next_sync_stops_retrying_an_unreadable_pid_after_the_limit() {
        let mut runs = 0;
        for _ in 0..DRIFT_UNRESOLVED_RETRY_LIMIT {
            let next = plan_next_sync(false, true, runs, true);
            assert_eq!(next.after, Some(DRIFT_SYNC_RETRY));
            runs = next.unresolved_runs;
        }
        let next = plan_next_sync(false, true, runs, true);
        assert_eq!(next.after, Some(DRIFT_SYNC_SWEEP));
        // No hook exists (the only window has an unreadable pid): still swept.
        let no_hook = plan_next_sync(false, true, runs, false);
        assert_eq!(no_hook.after, Some(DRIFT_SYNC_SWEEP));
        let resolved = plan_next_sync(false, false, next.unresolved_runs, true);
        assert_eq!(resolved.unresolved_runs, 0);
    }

    #[test]
    fn no_applied_window_means_no_hook() {
        assert_eq!(
            plan_drift_hooks(&set(&[]), &set(&[]), 1),
            DriftHookPlan::default()
        );
    }

    #[test]
    fn first_applied_process_gets_a_hook() {
        let plan = plan_drift_hooks(&set(&[42]), &set(&[]), 1);
        assert_eq!(plan.register, vec![42]);
        assert!(plan.unregister.is_empty());
    }

    #[test]
    fn unchanged_set_does_nothing() {
        assert_eq!(
            plan_drift_hooks(&set(&[42, 43]), &set(&[42, 43]), 1),
            DriftHookPlan::default()
        );
    }

    #[test]
    fn set_change_registers_new_and_drops_gone_processes() {
        let plan = plan_drift_hooks(&set(&[43, 44]), &set(&[42, 43]), 1);
        assert_eq!(plan.register, vec![44]);
        assert_eq!(plan.unregister, vec![42]);
    }

    #[test]
    fn last_window_gone_drops_every_hook() {
        let plan = plan_drift_hooks(&set(&[]), &set(&[42, 43]), 1);
        assert!(plan.register.is_empty());
        assert_eq!(plan.unregister, vec![42, 43]);
    }

    #[test]
    fn own_process_is_never_hooked() {
        let plan = plan_drift_hooks(&set(&[1, 42]), &set(&[]), 1);
        assert_eq!(plan.register, vec![42]);
    }
}
