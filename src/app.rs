//! Thin orchestrator: builds the tray icon/menu ([`crate::tray_ui`]) and
//! then runs the Win32 message loop that keeps it alive, dispatching a
//! click on "Exit" to a clean shutdown, a click on "Show Settings" to
//! lazily opening (and fully tearing down, on close) the egui Settings
//! window ([`crate::settings_ui`]), a press of the bound borderless hotkey
//! ([`crate::hotkeys`]) to stripping/restoring the foreground window's
//! border via [`crate::win32`], and a press of the bound taskbar-autohide
//! hotkey to toggling the taskbar hidden/visible (mode-aware as of issue
//! #42 -- see [`crate::profile_store::TaskbarHideMode`]) via
//! [`crate::win32`]. The two hotkey mechanics are fully independent: the
//! borderless toggle keeps its own `toggle_state` below, while the taskbar
//! toggle ([`win32::toggle_taskbar_hide`]) keeps none at all, instead
//! reading the taskbar's real OS-level state on every press -- either can
//! be pressed without the other having been used first. A click on one of the
//! "Apply to Window" submenu's dynamically-listed windows (recognized via
//! [`tray_ui::decode_window_item_id`], distinct from the three fixed
//! top-level menu items) resolves that window's owning process against the
//! tracked-games list (adding it, with this issue's new-tracked-game
//! defaults, if not already tracked -- see
//! [`crate::profile_store::upsert_tracked_game`]) and applies its assigned
//! Zone's borderless+reposition treatment to that specific window via
//! [`win32::apply_borderless_to_window`] -- a one-shot action with no
//! remembered restore state, unlike the hotkey path's `toggle_state` (see
//! [`handle_apply_to_window`]).
//!
//! A third trigger requires no user action at all: [`crate::launch_watcher`]
//! registers a `SetWinEventHook` on the same message loop and, on every
//! foreground-window change, delegates to [`handle_auto_apply_launch`] --
//! which applies the Zone to a tracked game the moment it becomes the
//! foreground window, but only if that game's `auto_apply` flag is on (see
//! [`resolve_auto_apply`]).
//!
//! Issue #43 adds automatic taskbar restore, with no hotkey press required:
//! every apply-path call site above that hides the taskbar for a specific
//! window now arms [`win32::set_taskbar_hide_absolute_armed`]'s remembered
//! restore for that window (see `win32::ARMED_TASKBAR_RESTORE`'s doc
//! comment). [`crate::launch_watcher`] registers a second `SetWinEventHook`
//! scoped to `EVENT_OBJECT_DESTROY` and restores the taskbar automatically
//! when the armed window closes; [`run`] additionally checks the same armed
//! state and restores it before returning, covering the case where the
//! user quits GameFrame via the tray "Exit" item while a hide is still in
//! effect.
//!
//! The message loop itself ([`run`]) is OS-facing (pumps real Win32
//! messages) and is not exercised by this crate's automated tests, for
//! the same reason [`crate::tray_ui::Tray::new`] isn't: it needs a live
//! thread message queue that a headless/CI sandbox can't provide.

use crate::autostart;
use crate::hotkeys::{Hotkeys, HotkeyAction};
use crate::launch_watcher;
use crate::profile_store;
use crate::region_calc::{resolve_region, Region};
use crate::settings_ui::{self, primary_monitor_geometry};
use crate::tray_ui::{self, Tray, TrayAction, TrayUiError};
use crate::win32::{self, RestoreState};
use crate::window_filter;
use crate::zone_overlay;
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use std::path::{Path, PathBuf};
use tray_icon::menu::MenuEvent;
use tray_icon::TrayIconEvent;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, TranslateMessage, MSG};

pub fn run() -> Result<(), TrayUiError> {
    let tray = Tray::new()?;
    let config_dir = config_dir();

    // First run (no Zone yet): open the zone editor once, now that the tray is up.
    prompt_for_zone_if_unset(&config_dir);

    let hotkeys = register_hotkeys(&config_dir);

    // Registered before entering `run_event_loop`'s `GetMessageW` pump --
    // see `crate::launch_watcher`'s module docs for why registration must
    // happen on this thread, before that loop starts.
    let launch_watcher_hook = launch_watcher::register(&config_dir);

    // Issue #43: a second, separate hook scoped to exactly
    // `EVENT_OBJECT_DESTROY`, rather than widening the foreground-change
    // hook's event range -- see `crate::launch_watcher`'s module docs for
    // why. Registered on this same thread for the same reason as
    // `launch_watcher_hook` above.
    let taskbar_restore_watcher_hook = launch_watcher::register_taskbar_restore_watcher();

    // Re-applies the Zone to a tracked game if the game itself resizes/restyles
    // its window afterwards (e.g. after an in-game settings change).
    let drift_watcher_hook = launch_watcher::register_drift_watcher();

    run_event_loop(&tray, hotkeys, &config_dir);

    // Issue #43: if a Zone-apply's taskbar hide is still armed when the
    // user quits via the tray "Exit" item, restore it now -- before `tray`
    // is dropped below (which just removes the tray icon; it has no bearing
    // on the taskbar) and before the process actually exits, so the desktop
    // is never left with the taskbar hidden and no running GameFrame
    // instance left to bring it back.
    if let Some(restore) = win32::take_any_armed_taskbar_restore() {
        if let Err(e) = win32::restore_taskbar(restore) {
            eprintln!("failed to restore taskbar on exit: {e}");
        }
    }

    if let Some(hook) = launch_watcher_hook {
        launch_watcher::unregister(hook);
    }
    if let Some(hook) = taskbar_restore_watcher_hook {
        launch_watcher::unregister(hook);
    }
    if let Some(hook) = drift_watcher_hook {
        launch_watcher::unregister(hook);
    }

    Ok(())
}

/// Opens the zone editor blank when the loaded config has no Zone (see
/// [`profile_store::should_prompt_for_zone`]), persisting the Zone on Save
/// exactly like Settings' "Draw zone" does. Cancel, a close, or any failure
/// leaves the Zone unset and startup simply continues -- there is no
/// re-prompt this session. No window is placed here either way.
///
/// OS-facing (opens a real window), so not unit-tested; the decision itself is.
fn prompt_for_zone_if_unset(config_dir: &Path) {
    let config = match profile_store::load(config_dir) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("failed to load config for the first-run zone prompt: {e}");
            return;
        }
    };
    if !profile_store::should_prompt_for_zone(&config) {
        return;
    }

    match zone_overlay::run_zone_editor_overlay(None, config.taskbar_hide_mode) {
        Ok(Some(zone)) => {
            if let Err(e) = settings_ui::persist_zone(config_dir, Some(zone)) {
                eprintln!("failed to save the zone: {e}");
            }
        }
        Ok(None) => {}
        Err(e) => eprintln!("failed to open the zone editor overlay: {e}"),
    }
}

/// Loads the config and registers its bound hotkeys with the OS. A failure
/// at either step (a malformed config file, an OS registration failure --
/// e.g. another application already holds the exact key combination) is
/// reported to stderr rather than treated as fatal: GameFrame still starts
/// with no hotkey wired up, matching the fresh-install "nothing bound yet"
/// case where pressing keys is simply inert.
fn register_hotkeys(config_dir: &Path) -> Option<Hotkeys> {
    let config = match profile_store::load(config_dir) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("failed to load config for hotkey registration: {e}");
            return None;
        }
    };

    match Hotkeys::register(&config.hotkeys) {
        Ok(hotkeys) => Some(hotkeys),
        Err(e) => {
            eprintln!("failed to register global hotkeys: {e}");
            None
        }
    }
}

/// Pumps the Win32 message loop on this thread. Each dispatched message
/// may cause the tray/menu to enqueue an event on
/// [`MenuEvent::receiver`]; draining it after every dispatched message
/// lets us react to a click without a separate polling thread. The same
/// pattern applies to global hotkey presses, delivered on
/// [`GlobalHotKeyEvent::receiver`] by the `global-hotkey` crate, and to tray
/// icon mouse interactions, delivered on [`TrayIconEvent::receiver`] by
/// `tray-icon` -- the latter is what gates the "Apply to Window" submenu's
/// live refresh (see the loop body below).
///
/// `hotkeys` is owned (not borrowed) specifically so the `ShowSettings`
/// branch below can reassign it with a freshly re-registered [`Hotkeys`]
/// after the Settings window closes -- see that branch's comment.
fn run_event_loop(tray: &Tray, mut hotkeys: Option<Hotkeys>, config_dir: &Path) {
    // Remembers the foreground window's pre-toggle style/region across
    // presses of the borderless hotkey -- `None` means the window (if any)
    // is currently in its normal bordered state; `Some` means the last
    // press stripped it and this is what a second press should restore.
    let mut toggle_state: Option<RestoreState> = None;

    // Remembers which window `toggle_state` was stripped from, in lockstep
    // with `toggle_state` itself -- lets `handle_toggle_borderless` refuse a
    // restore press if a *different* window now has focus (see
    // `should_allow_restore`; HIGH review finding, issue #31 iteration 3).
    let mut stripped_hwnd: Option<HWND> = None;

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        // 0 (WM_QUIT) or -1 (error) both mean: stop pumping.
        let ret = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if ret <= 0 {
            return;
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Refreshes the "Apply to Window" submenu's item list only in
        // response to an actual tray-icon mouse interaction (the mouse
        // entering the icon, or a click on it), rather than unconditionally
        // on every pumped message.
        //
        // `Enter` reliably arrives on its own dispatched `WM_MOUSEMOVE`
        // message, strictly before the later `WM_RBUTTONUP` message that
        // opens the context menu (a user must hover onto the tray icon
        // before they can click it) -- so refreshing here, once per
        // `Enter`, completes before the menu is ever shown. `Click` is
        // included too as a defensive fallback (e.g. covers the case where
        // this process only just started pumping messages while the cursor
        // was already resting on the icon, so no `Enter` transition ever
        // fires) even though, per `tray-icon`'s Windows backend, the menu is
        // shown synchronously in the same `WM_RBUTTONUP` handler right after
        // the `Click` event is enqueued -- refreshing on `Click` won't beat
        // that particular menu display, but it does mean the very next
        // open is guaranteed current.
        //
        // This replaces an earlier unconditional "refresh before every
        // dispatched message" approach. The primary, certain reason: `tray-icon`
        // sets a 15ms `WM_TIMER` while the mouse hovers over the icon, which
        // would otherwise have driven a full `EnumWindows` + per-window
        // `GetWindowTextW` + submenu rebuild (real Win32 syscalls) up to ~66
        // times/second just from the cursor resting nearby -- needlessly
        // expensive on this process's one and only message loop. A secondary,
        // less certain consideration: per the documented `GetWindowTextW`
        // contract, a same-process target window causes a `WM_GETTEXT` to be
        // sent via `SendMessage` and can block if that window is stuck,
        // whereas a cross-process target (the common case when enumerating
        // the whole desktop) is documented to read the cached caption text
        // directly instead, specifically so callers don't hang on an
        // unresponsive foreign process -- so the practical hang risk here is
        // narrower than "any hung window on the desktop" (more like "one of
        // GameFrame's own windows getting stuck"), but still worth avoiding
        // at no extra cost. No crate-native "menu about to show" event exists
        // to hook instead (see `Tray::refresh_apply_to_window_menu`'s doc
        // comment).
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if matches!(event, TrayIconEvent::Enter { .. } | TrayIconEvent::Click { .. }) {
                tray.refresh_apply_to_window_menu();
                tray.sync_start_with_windows_check();
            }
        }

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if let Some(target_hwnd) = tray_ui::decode_window_item_id(event.id()) {
                handle_apply_to_window(config_dir, target_hwnd);
                continue;
            }

            match tray.ids().action_for(event.id()) {
                Some(TrayAction::Exit) => {
                    // Returning here unwinds back to `run()`, which then
                    // drops its owned `tray`, removing the OS tray icon
                    // and destroying its hidden window -- the entire
                    // clean-shutdown path for the tray UI.
                    return;
                }
                Some(TrayAction::ShowSettings) => {
                    // Synchronous/modal-style for this MVP: opening
                    // Settings blocks this thread until the Settings
                    // window closes, at which point the egui/wgpu
                    // context it used is fully dropped and we resume
                    // pumping tray messages right where we left off.
                    if let Err(e) = open_settings(config_dir) {
                        eprintln!("failed to open Settings: {e}");
                    }

                    // Settings may have changed the hotkey bindings on
                    // disk (Save persists via `profile_store`) --
                    // re-register from the freshly saved config so a
                    // newly bound/changed/removed hotkey takes effect
                    // immediately, without requiring a full app
                    // restart. Assigning over `hotkeys` drops the old
                    // `Hotkeys` first (its `Drop` impl destroys the old
                    // `GlobalHotKeyManager`'s hidden window, which the OS
                    // uses to automatically unregister every hotkey that
                    // was bound to it) before the new one registers,
                    // so there's no window where both the old and new
                    // bindings are simultaneously held.
                    hotkeys = register_hotkeys(config_dir);
                    tray.sync_start_with_windows_check();
                }
                Some(TrayAction::ToggleStartWithWindows) => {
                    // `muda` has already flipped the check mark, but the
                    // registry -- not the menu -- is the source of truth:
                    // toggle what the registry says, then re-sync the check
                    // so a failed write doesn't leave it showing a lie.
                    if let Err(e) = autostart::set_enabled(!autostart::is_enabled()) {
                        eprintln!("failed to change Start with Windows: {e}");
                    }
                    tray.sync_start_with_windows_check();
                }
                None => {}
            }
        }

        while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            // Only act on the key-down edge; the crate also emits a
            // `Released` event on key-up (see `global_hotkey`'s Windows
            // implementation), which would otherwise double-toggle.
            if event.state() != HotKeyState::Pressed {
                continue;
            }

            // With no hotkey bound (fresh install) `hotkeys` is `None`
            // (nothing was registered, so no event can arrive for it
            // anyway) or resolves to no action for `event.id()` --
            // either way, nothing happens here and nothing panics.
            let action = hotkeys.as_ref().and_then(|h| h.action_for(event.id()));
            match action {
                Some(HotkeyAction::ToggleBorderless) => {
                    if handle_toggle_borderless(config_dir, &mut toggle_state, &mut stripped_hwnd) {
                        // GameFrame relaunched itself elevated to complete a
                        // manipulation that failed for lack of permissions;
                        // this (unelevated) instance's job is done.
                        return;
                    }
                }
                Some(HotkeyAction::ToggleTaskbarAutohide) => {
                    handle_toggle_taskbar_autohide(config_dir);
                }
                None => {}
            }
        }
    }
}

/// Resolves the tracked game (if any) whose executable path is `exe_path`,
/// and the configured Zone -- the shared decision logic behind
/// [`handle_toggle_borderless`]'s "resolve against tracked games" rule.
///
/// Returns `None` in either of two cases, both treated identically by
/// callers (do nothing, no fallback; see [`no_apply_reason`] for the
/// distinct log text):
/// - no tracked game matches `exe_path` (an untracked process); or
/// - no Zone is set in the config.
///
/// Pure -- no OS call -- so it's unit testable in complete isolation from
/// the live foreground-window/tray-hwnd resolution that supplies `exe_path`.
fn resolve_tracked_apply(
    config: &profile_store::Config,
    exe_path: &std::path::Path,
) -> Option<(profile_store::TrackedGame, profile_store::Zone)> {
    let game = profile_store::find_tracked_game(&config.tracked_games, exe_path)?.clone();
    let zone = config.zone?;
    Some((game, zone))
}

/// Why [`resolve_tracked_apply`] returned `None`, for logging: "no Zone set"
/// is reported distinctly from "not a tracked game". Decided from
/// `config.zone` alone, so with no Zone it reports that even for an untracked
/// process.
fn no_apply_reason(config: &profile_store::Config) -> &'static str {
    if config.zone.is_none() { "no Zone is set" } else { "the process is not a tracked game" }
}

/// Like [`resolve_tracked_apply`], but additionally requires the tracked
/// game's `auto_apply` flag to be on -- the extra gate this issue's
/// launch-detection trigger (`crate::launch_watcher`) applies that the
/// hotkey/tray "manual" apply paths deliberately do not (per issue #31's
/// design: `auto_apply` only governs whether a launch is auto-applied
/// without a hotkey press, not whether a manual hotkey/tray press works).
///
/// Returns `None` for exactly the same two cases [`resolve_tracked_apply`]
/// does (no match; no Zone set), plus a third: matched, Zone set, but
/// `auto_apply` is off.
///
/// Pure -- no OS call -- so it's unit testable in complete isolation from
/// the real WinEvent hook registration/callback (`crate::launch_watcher`)
/// that supplies `exe_path`.
pub(crate) fn resolve_auto_apply(
    config: &profile_store::Config,
    exe_path: &std::path::Path,
) -> Option<(profile_store::TrackedGame, profile_store::Zone)> {
    let (game, zone) = resolve_tracked_apply(config, exe_path)?;
    if !game.auto_apply {
        return None;
    }
    Some((game, zone))
}

/// Decides whether [`handle_toggle_borderless`]'s tracked-game gate should
/// block this hotkey press outright, given whether the press is a pending
/// *restore* (`is_restoring`, i.e. `toggle_state.is_some()` -- see
/// [`win32::plan_toggle`], which derives the same strip-vs-restore decision
/// from that same remembered state) and whether the foreground window's
/// process currently resolves to a tracked game with a Zone set
/// (`tracked`, i.e. `resolve_tracked_apply(..).is_some()`).
///
/// Only the strip direction is gated on tracked-game membership: an
/// untracked foreground window blocks an initial strip, but never blocks a
/// pending restore purely because the *current* foreground window is
/// untracked. This is a deliberate judgment call (see the MEDIUM review
/// finding on issue #31) -- gating both directions uniformly would
/// permanently orphan a currently-stripped window in its borderless state
/// for the rest of the running session if its tracked game were removed via
/// Settings (or the Zone cleared) in between
/// the strip press and the restore press, since nothing else in the app can
/// restore it once the remembered `RestoreState` game is no longer
/// resolvable. `plan_toggle`'s `Restore` arm doesn't need a resolved
/// Zone/region at all, so there is no correctness reason to require one
/// here.
///
/// This function alone is *not* sufficient to greenlight a restore, though:
/// per the HIGH review finding on issue #31 iteration 3, a restore must
/// additionally only proceed against the *same* window that was originally
/// stripped -- see the separate, HWND-identity-based [`should_allow_restore`],
/// which [`handle_toggle_borderless`] also consults before calling
/// [`win32::toggle_borderless_on_foreground_window`] on the restore path.
///
/// Pure -- no OS call -- so it's unit testable in complete isolation.
fn should_block_untracked_toggle(is_restoring: bool, tracked: bool) -> bool {
    !is_restoring && !tracked
}

/// Decides whether a pending restore (`stripped`, the `HWND` that was
/// stripped by the last successful strip press, remembered alongside
/// `toggle_state`) may be applied to the window that currently has focus
/// (`current`, from [`win32::foreground_window`]).
///
/// Restoring blindly applies the previously-stripped window's saved
/// style/region to *whatever* `GetForegroundWindow()` currently returns --
/// if the user alt-tabbed away to a different window (Chrome, Explorer,
/// another tracked game) in between the strip and restore presses, applying
/// the restore there would corrupt that unrelated window's position/style
/// while permanently orphaning the original window in its stripped state
/// (fixes the HIGH review finding on issue #31 iteration 3, introduced by
/// [`should_block_untracked_toggle`]'s iteration-2 fix unconditionally
/// allowing every pending restore through regardless of which window is now
/// focused).
///
/// Only returns `true` when both `current` and `stripped` are `Some` and
/// equal -- a `None` foreground window, no remembered stripped window, or a
/// mismatch between the two all refuse the restore.
///
/// Pure -- no OS call, just an `HWND` comparison -- so it's unit testable in
/// complete isolation.
fn should_allow_restore(current: Option<HWND>, stripped: Option<HWND>) -> bool {
    current.is_some() && current == stripped
}

/// Reacts to a press of the borderless-toggle hotkey: resolves the
/// foreground window's owning process against the tracked-games list (see
/// [`resolve_tracked_apply`]) and, if it's tracked (or a restore is already
/// pending for *this same window* -- see below), applies (or reverts) the
/// border-strip via [`win32::toggle_borderless_on_foreground_window`], then,
/// on a successful strip only, sets the tracked game's taskbar-auto-hide
/// value absolutely (see
/// `docs/adr/0002-taskbar-autohide-coupled-to-preset-apply.md`).
///
/// The tracked-game gate ([`should_block_untracked_toggle`]) only applies to
/// the *strip* direction: an untracked foreground window blocks an initial
/// strip (per this issue's acceptance criteria and `CONTEXT.md`'s "Applying
/// the Zone to a window whose process is not a tracked game does nothing"),
/// but never blocks a pending *restore* purely because the untracked window
/// happens to be the one now in the foreground -- see that function's doc
/// comment for the full reasoning.
///
/// A pending restore is additionally gated on window identity via
/// [`should_allow_restore`]: `stripped_hwnd` remembers which window
/// `toggle_state` was stripped from, and a restore press only proceeds if
/// the current foreground window (`fg_hwnd`) is that same window. If the
/// user alt-tabbed to a different window in between the strip and restore
/// presses, this press is refused outright -- logged, `toggle_state`/
/// `stripped_hwnd` left untouched -- rather than applying the original
/// game's saved style/region to whatever window now happens to have focus
/// (HIGH review finding, issue #31 iteration 3). The user can still complete
/// the correct restore later by refocusing the originally-stripped window.
///
/// Taskbar auto-hide is set only on the strip direction, not on restore:
/// using the same fixed global boolean on both directions would leave the
/// taskbar auto-hidden after a full strip+restore cycle instead of it coming
/// back, for the common `hide_taskbar_on_apply: true` default (MEDIUM review
/// finding, issue #31 iteration 3).
///
/// Returns `true` if GameFrame just relaunched itself elevated and this
/// process should exit (see [`win32::relaunch_elevated`]); `false` in every
/// other case (success, or a non-elevation failure that was just logged).
///
/// Reloads the config from disk on every press (rather than caching it
/// across the whole run) so a Zone/game added/edited via Settings
/// since the last press is picked up immediately -- the trade-off is a
/// small, infrequent disk read on a user-initiated action, which is cheap.
/// The hotkey binding itself is re-registered separately, right after the
/// Settings window closes -- see the `ShowSettings` branch in
/// [`run_event_loop`] -- so a changed binding also takes effect without a
/// restart.
fn handle_toggle_borderless(config_dir: &Path, toggle_state: &mut Option<RestoreState>, stripped_hwnd: &mut Option<HWND>) -> bool {
    let config = match profile_store::load(config_dir) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("borderless toggle: failed to load config: {e}");
            return false;
        }
    };

    // Whether this press is about to restore (a `RestoreState` is already
    // remembered from a prior strip) or strip (nothing remembered yet) --
    // see `should_block_untracked_toggle`'s doc comment for why only the
    // strip direction is gated on tracked-game membership below.
    let is_restoring = toggle_state.is_some();

    // Resolved once and reused both for the tracked-game lookup below and
    // for the restore-direction identity check (`should_allow_restore`) --
    // see `win32::foreground_window`'s doc comment for why this replaces a
    // second, separate `GetForegroundWindow()` call.
    let fg_hwnd = win32::foreground_window();

    if is_restoring {
        if !should_allow_restore(fg_hwnd, *stripped_hwnd) {
            eprintln!(
                "borderless toggle: a pending restore exists for a different window; ignoring press until that window is focused again"
            );
            // Deliberately do NOT clear `toggle_state`/`stripped_hwnd` here --
            // the user can still complete the correct restore later by
            // refocusing the originally-stripped window (HIGH review finding,
            // issue #31 iteration 3).
            return false;
        }
    }

    let tracked = fg_hwnd
        .and_then(window_filter::exe_path_for_window)
        .and_then(|exe_path| resolve_tracked_apply(&config, &exe_path));

    if should_block_untracked_toggle(is_restoring, tracked.is_some()) {
        eprintln!("borderless toggle: not applying ({}); ignoring", no_apply_reason(&config));
        return false;
    }

    let target_region = match &tracked {
        Some((tracked_game, zone)) => {
            let monitor = primary_monitor_geometry();
            match resolve_region(zone, &monitor) {
                Ok(region) => region,
                Err(e) => {
                    eprintln!("borderless toggle: failed to resolve the Zone for {}: {e}", tracked_game.display_name);
                    return false;
                }
            }
        }
        // Only reachable while restoring -- the gate above already returned
        // for the strip direction when `tracked` is `None`. `plan_toggle`'s
        // `Restore` arm never reads `target_region`, so this placeholder
        // value is inert.
        None => Region { x: 0, y: 0, width: 0, height: 0 },
    };

    // Commit `stripped_hwnd` for the strip direction *before* attempting the
    // OS-mutating call below, mirroring `win32.rs`'s own "commit early"
    // pattern for `toggle_state` in its `MakeBorderless` arm (see that arm's
    // doc comment: "Commit the new remembered state *before* attempting the
    // OS-mutating calls below"). Without this, a partial failure inside
    // `toggle_borderless_on_foreground_window` would leave `toggle_state`
    // committed (as designed, by that function itself) but `stripped_hwnd`
    // unchanged -- a desync where `is_restoring` reads `true` on the next
    // press but `should_allow_restore` refuses it forever, since
    // `stripped_hwnd` never got recorded (HIGH review finding, issue #31
    // iteration 4). The restore direction is not committed early: a failed
    // restore should still leave `stripped_hwnd` in place so a retry can
    // succeed, mirroring `win32.rs`'s own `Restore` arm, which only clears
    // `toggle_state` after all three restore calls succeed.
    if !is_restoring {
        *stripped_hwnd = fg_hwnd;
    }

    if let Err(e) = win32::toggle_borderless_on_foreground_window(toggle_state, target_region) {
        if e.is_access_denied() {
            eprintln!("borderless toggle failed (access denied); requesting elevation");
            match win32::relaunch_elevated() {
                Ok(()) => return true,
                Err(relaunch_err) => eprintln!("failed to relaunch elevated: {relaunch_err}"),
            }
        } else {
            eprintln!("borderless toggle failed: {e}");
        }
        return false;
    }

    // Mirror `toggle_state`'s own transition for `stripped_hwnd` on success:
    // the strip direction was already committed above (before the call); a
    // successful restore (only reachable here when `should_allow_restore`
    // already confirmed `fg_hwnd == *stripped_hwnd`) clears it now.
    if is_restoring {
        *stripped_hwnd = None;
    }

    // Only set taskbar auto-hide on the strip direction -- setting it again
    // on restore (using the same fixed global boolean) would leave the
    // taskbar auto-hidden after the user exits borderless/game mode instead
    // of it coming back, for the common `hide_taskbar_on_apply: true` default
    // (MEDIUM review finding, issue #31 iteration 3). ADR 0002's coupling is
    // about the strip/apply action, not the restore/undo action.
    //
    // Uses `set_taskbar_hide_absolute_armed` rather than the plain
    // `set_taskbar_hide_absolute` (issue #43) so that closing `fg_hwnd`
    // later automatically restores the taskbar with no hotkey press
    // required. `fg_hwnd` is guaranteed `Some` here: `tracked` was only
    // resolved (in the strip direction) from `fg_hwnd` itself above.
    if !is_restoring {
        if let (Some((tracked_game, _)), Some(hwnd)) = (&tracked, fg_hwnd) {
            if let Err(e) = win32::set_taskbar_hide_absolute_armed(
                hwnd,
                config.hide_taskbar_on_apply,
                config.taskbar_hide_mode,
            ) {
                eprintln!(
                    "borderless toggle: failed to set taskbar auto-hide for {}: {e}",
                    tracked_game.display_name
                );
            }
        }
    }

    false
}

/// Reacts to a click on one of the "Apply to Window" submenu's
/// dynamically-listed windows ([`tray_ui::decode_window_item_id`]): resolves
/// `target_hwnd`'s owning process's executable path, looks it up (or adds it,
/// with auto-apply on, taskbar-auto-hide on) in the tracked-games list via
/// [`profile_store::upsert_tracked_game`], persists that list update
/// immediately (even if a later step below fails), then applies the
/// Zone's borderless+reposition treatment to `target_hwnd` --
/// which may not be the foreground window at all, that's the whole point of
/// this tray-menu path -- via [`win32::apply_borderless_to_window`], and,
/// only once that border-apply succeeds, sets the tracked game's
/// taskbar-auto-hide value absolutely (see
/// `docs/adr/0002-taskbar-autohide-coupled-to-preset-apply.md`). A
/// border-apply failure (e.g. the target window is minimized) returns early
/// without touching the taskbar at all -- mirroring
/// [`handle_toggle_borderless`]'s equivalent gating -- since forcing the
/// taskbar into auto-hide for a window that was never actually stripped
/// would contradict ADR 0002's whole rationale for coupling the two.
///
/// A one-shot action, not a toggle: there is no remembered restore state for
/// this path (see this module's doc comment for the scope decision), so
/// selecting the same window again simply re-applies the same treatment
/// rather than undoing it.
///
/// Reloads the config from disk on every click, for the same reason
/// [`handle_toggle_borderless`] does.
///
/// Unlike [`handle_toggle_borderless`], a failure here (including an
/// access-denied failure) is only logged, not retried via
/// [`win32::relaunch_elevated`] -- reactive elevation for this path is out
/// of scope for this slice.
fn handle_apply_to_window(config_dir: &Path, target_hwnd: HWND) {
    let mut config = match profile_store::load(config_dir) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("apply to window: failed to load config: {e}");
            return;
        }
    };

    let Some(exe_path) = window_filter::exe_path_for_window(target_hwnd) else {
        eprintln!("apply to window: could not resolve the target window's executable path");
        return;
    };

    let display_name = win32::window_text(target_hwnd);
    let index = profile_store::upsert_tracked_game(&mut config.tracked_games, exe_path, display_name);
    let tracked_game = config.tracked_games[index].clone();

    // Persist the (possibly newly-added) tracked game immediately, before
    // attempting the region/border/taskbar steps below -- so a downstream
    // failure (e.g. no Zone set, or the window manipulation
    // itself failing) never costs the user the "this window is now tracked"
    // outcome that "Apply to Window" is documented to guarantee.
    if let Err(e) = profile_store::save(config_dir, &config) {
        eprintln!("apply to window: failed to save tracked game {}: {e}", tracked_game.display_name);
    }

    let Some(zone) = config.zone else {
        eprintln!("apply to window: no Zone is set; {} is tracked but nothing was applied", tracked_game.display_name);
        return;
    };

    let monitor = primary_monitor_geometry();
    let target_region = match resolve_region(&zone, &monitor) {
        Ok(region) => region,
        Err(e) => {
            eprintln!("apply to window: failed to resolve the Zone for {}: {e}", tracked_game.display_name);
            return;
        }
    };

    if let Err(e) = win32::apply_borderless_to_window(target_hwnd, target_region) {
        eprintln!("apply to window failed: {e}");
        return;
    }
    win32::remember_applied_window(target_hwnd);

    // `set_taskbar_hide_absolute_armed` (issue #43) rather than the plain
    // `set_taskbar_hide_absolute`: arms `target_hwnd` so closing it later
    // automatically restores the taskbar with no hotkey press required.
    if let Err(e) = win32::set_taskbar_hide_absolute_armed(
        target_hwnd,
        config.hide_taskbar_on_apply,
        config.taskbar_hide_mode,
    ) {
        eprintln!(
            "apply to window: failed to set taskbar auto-hide for {}: {e}",
            tracked_game.display_name
        );
    }
}

/// Reacts to a [`crate::launch_watcher`]-detected foreground-window change:
/// resolves `hwnd`'s owning process's executable path and, only if it
/// matches a tracked game with auto-apply on (see [`resolve_auto_apply`]),
/// applies the Zone's borderless+reposition treatment to
/// `hwnd` via [`win32::apply_borderless_to_window`], then, only once that
/// succeeds, sets the global hide-taskbar-on-apply value absolutely --
/// exactly mirroring [`handle_apply_to_window`]'s one-shot apply shape
/// (including its border-apply-must-succeed-first gate on the taskbar step,
/// per issue #31's HIGH review fix) and [`win32::apply_borderless_to_window`]
/// itself, rather than the hotkey path's toggle/restore mechanism -- a
/// launch-detected apply has no "undo" concept, since there's nothing to
/// restore from (the game just started).
///
/// Unlike [`handle_apply_to_window`], this never adds a new tracked game or
/// persists the config: a game must already be tracked, with auto-apply on,
/// to reach an actual apply here at all (see [`resolve_auto_apply`]'s doc
/// comment), so there is no upsert/save step.
///
/// Reloads the config from disk on every foreground-change event, matching
/// [`handle_toggle_borderless`]/[`handle_apply_to_window`]'s own "reload
/// from disk on every trigger" convention.
///
/// OS-facing (resolves a real `HWND`'s owning process and applies real
/// window/taskbar mutations) -- not exercised by this crate's automated
/// tests, per this crate's established convention (manual usertest only).
/// [`resolve_auto_apply`] above is the pure decision this function
/// delegates to, and is what's actually unit tested.
pub(crate) fn handle_auto_apply_launch(config_dir: &Path, hwnd: HWND) {
    let config = match profile_store::load(config_dir) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("launch detection: failed to load config: {e}");
            return;
        }
    };

    let Some(exe_path) = window_filter::exe_path_for_window(hwnd) else {
        return;
    };

    let Some((tracked_game, zone)) = resolve_auto_apply(&config, &exe_path) else {
        // Distinct from "untracked" (silent, the common case): a tracked
        // auto-apply game that launched while no Zone is set.
        if config.zone.is_none()
            && profile_store::find_tracked_game(&config.tracked_games, &exe_path).is_some_and(|g| g.auto_apply)
        {
            eprintln!("launch detection: not applying to a tracked game ({})", no_apply_reason(&config));
        }
        return;
    };

    let monitor = primary_monitor_geometry();
    let target_region = match resolve_region(&zone, &monitor) {
        Ok(region) => region,
        Err(e) => {
            eprintln!("launch detection: failed to resolve the Zone for {}: {e}", tracked_game.display_name);
            return;
        }
    };

    if let Err(e) = win32::apply_borderless_to_window(hwnd, target_region) {
        eprintln!("launch detection: failed to apply borderless treatment to {}: {e}", tracked_game.display_name);
        return;
    }
    // Lets the location-change watcher re-apply if the game later resizes
    // or restyles itself (e.g. after an in-game settings change).
    win32::remember_applied_window(hwnd);

    // `set_taskbar_hide_absolute_armed` (issue #43) rather than the plain
    // `set_taskbar_hide_absolute`: arms `hwnd` so closing the game later
    // automatically restores the taskbar with no hotkey press required.
    if let Err(e) = win32::set_taskbar_hide_absolute_armed(
        hwnd,
        config.hide_taskbar_on_apply,
        config.taskbar_hide_mode,
    ) {
        eprintln!(
            "launch detection: failed to set taskbar auto-hide for {}: {e}",
            tracked_game.display_name
        );
    }
}

/// Reacts to a location-change event on a window GameFrame previously
/// applied the Zone to (see [`win32::is_applied_window`]): if the game moved,
/// resized, or re-added its border itself (e.g. after an in-game graphics
/// setting change), re-applies the Zone. A no-op when the
/// window still matches the target, which also stops GameFrame's own
/// `SetWindowPos` from re-triggering itself. Does not touch the taskbar.
///
/// OS-facing; the drift decision is [`win32::window_drifted`], unit tested.
pub(crate) fn handle_applied_window_drift(config_dir: &Path, hwnd: HWND) {
    let Some(exe_path) = window_filter::exe_path_for_window(hwnd) else {
        return;
    };
    let Ok(config) = profile_store::load(config_dir) else {
        return;
    };
    let Some((tracked_game, zone)) = resolve_auto_apply(&config, &exe_path) else {
        return;
    };
    let Ok(target_region) = resolve_region(&zone, &primary_monitor_geometry()) else {
        return;
    };
    if !matches!(win32::window_has_drifted(hwnd, target_region), Ok(true)) {
        return;
    }
    if let Err(e) = win32::apply_borderless_to_window(hwnd, target_region) {
        eprintln!("drift re-apply: failed for {}: {e}", tracked_game.display_name);
    }
}

/// Reacts to a press of the taskbar-autohide-toggle hotkey: flips the
/// taskbar's *actual* hidden/visible state via [`win32::toggle_taskbar_hide`],
/// which grounds its decision in the taskbar's real OS-level state rather
/// than a remembered flag -- see that function's doc comment for why (a
/// remembered flag drifts out of sync with reality the moment a tracked
/// game's launch-detection auto-hide changes the taskbar's real state
/// without this hotkey knowing). A failure (e.g. the taskbar window
/// couldn't be located) is logged rather than crashing, matching
/// [`handle_toggle_borderless`]'s error-handling pattern -- unlike that
/// mechanic, this one never needs elevation (`SHAppBarMessage`/`ShowWindow`
/// on the taskbar doesn't require it), so there's no relaunch-elevated path
/// here.
///
/// Reloads the config from disk on every press, matching
/// [`handle_toggle_borderless`]'s own "reload from disk on every trigger"
/// convention -- so a mode change made via Settings since the last press is
/// picked up immediately.
fn handle_toggle_taskbar_autohide(config_dir: &Path) {
    let mode = match profile_store::load(config_dir) {
        Ok(config) => config.taskbar_hide_mode,
        Err(e) => {
            eprintln!("taskbar auto-hide toggle: failed to load config: {e}");
            return;
        }
    };

    if let Err(e) = win32::toggle_taskbar_hide(mode) {
        eprintln!("taskbar auto-hide toggle failed: {e}");
    }
}

/// Opens the Settings window at `config_dir`. See
/// [`settings_ui::run_settings_window`] for what "opens" entails (blocks
/// until closed, persists on Save).
fn open_settings(config_dir: &Path) -> Result<(), settings_ui::SettingsUiError> {
    settings_ui::run_settings_window(config_dir)
}

/// The real `%APPDATA%\GameFrame` directory GameFrame persists its config
/// under. Falls back to a `GameFrame` directory relative to the current
/// working directory in the (in-practice-never-on-real-Windows) case where
/// the `APPDATA` environment variable isn't set, rather than panicking.
fn config_dir() -> PathBuf {
    match std::env::var("APPDATA") {
        Ok(appdata) => PathBuf::from(appdata).join("GameFrame"),
        Err(_) => PathBuf::from("GameFrame"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkeys::resolve_bindings;
    use crate::profile_store::{Config, TrackedGame, Zone};
    use global_hotkey::hotkey::HotKey;
    use std::path::PathBuf;

    #[test]
    fn should_block_untracked_toggle_blocks_an_untracked_strip() {
        assert!(should_block_untracked_toggle(false, false));
    }

    #[test]
    fn should_block_untracked_toggle_allows_a_tracked_strip() {
        assert!(!should_block_untracked_toggle(false, true));
    }

    #[test]
    fn should_block_untracked_toggle_allows_an_untracked_restore() {
        // A pending restore (a `RestoreState` already remembered from a
        // prior strip) must always be allowed to complete, even if the game
        // is no longer tracked (e.g. removed via Settings mid-session) --
        // otherwise the window is permanently orphaned in its stripped
        // state for the rest of the run (MEDIUM review finding, issue #31).
        assert!(!should_block_untracked_toggle(true, false));
    }

    #[test]
    fn should_block_untracked_toggle_allows_a_tracked_restore() {
        assert!(!should_block_untracked_toggle(true, true));
    }

    #[test]
    fn should_allow_restore_when_the_foreground_window_matches_the_stripped_window() {
        let hwnd = 1usize as HWND;
        assert!(should_allow_restore(Some(hwnd), Some(hwnd)));
    }

    #[test]
    fn should_allow_restore_refuses_when_a_different_window_is_now_focused() {
        // Alt-tab from a stripped game to any other window (issue #31,
        // iteration-3 HIGH fix) must never let the pending restore apply to
        // the newly-focused window.
        let stripped = 1usize as HWND;
        let other = 2usize as HWND;
        assert!(!should_allow_restore(Some(other), Some(stripped)));
    }

    #[test]
    fn should_allow_restore_refuses_when_there_is_no_foreground_window() {
        let stripped = 1usize as HWND;
        assert!(!should_allow_restore(None, Some(stripped)));
    }

    #[test]
    fn should_allow_restore_refuses_when_nothing_was_ever_stripped() {
        let hwnd = 1usize as HWND;
        assert!(!should_allow_restore(Some(hwnd), None));
    }

    fn config_with_zone_and_game(zone: Option<Zone>, auto_apply: bool) -> (Config, PathBuf) {
        let mut config = Config { zone, ..Config::default() };
        let exe_path = PathBuf::from("C:\\Games\\thing.exe");
        let mut game = TrackedGame::new_default(exe_path.clone(), "Thing".to_string());
        game.auto_apply = auto_apply;
        config.tracked_games.push(game);
        (config, exe_path)
    }

    const A_ZONE: Zone = Zone { x: 10, y: 20, width: 300, height: 200 };

    #[test]
    fn resolve_tracked_apply_returns_none_for_an_untracked_executable_path() {
        let config = Config { zone: Some(A_ZONE), ..Config::default() };

        let result = resolve_tracked_apply(&config, &PathBuf::from("C:\\Games\\thing.exe"));

        assert!(result.is_none());
    }

    #[test]
    fn resolve_tracked_apply_returns_none_when_no_zone_is_set() {
        let (config, exe_path) = config_with_zone_and_game(None, true);

        assert!(resolve_tracked_apply(&config, &exe_path).is_none());
    }

    #[test]
    fn resolve_tracked_apply_returns_the_game_and_the_zone() {
        let (config, exe_path) = config_with_zone_and_game(Some(A_ZONE), true);

        let (game, zone) = resolve_tracked_apply(&config, &exe_path).expect("expected a tracked game match");

        assert_eq!(game.display_name, "Thing");
        assert_eq!(zone, A_ZONE);
    }

    #[test]
    fn resolve_auto_apply_returns_the_game_and_the_zone_when_auto_apply_is_on() {
        let (config, exe_path) = config_with_zone_and_game(Some(A_ZONE), true);

        let (game, zone) = resolve_auto_apply(&config, &exe_path).expect("expected a resolved auto-apply match");

        assert_eq!(game.display_name, "Thing");
        assert_eq!(zone, A_ZONE);
    }

    #[test]
    fn resolve_auto_apply_returns_none_when_auto_apply_is_off() {
        let (config, exe_path) = config_with_zone_and_game(Some(A_ZONE), false);

        assert!(resolve_auto_apply(&config, &exe_path).is_none());
    }

    #[test]
    fn resolve_auto_apply_returns_none_for_an_untracked_executable_path() {
        let config = Config { zone: Some(A_ZONE), ..Config::default() };

        let result = resolve_auto_apply(&config, &PathBuf::from("C:\\Games\\thing.exe"));

        assert!(result.is_none());
    }

    #[test]
    fn resolve_auto_apply_returns_none_when_no_zone_is_set() {
        let (config, exe_path) = config_with_zone_and_game(None, true);

        assert!(resolve_auto_apply(&config, &exe_path).is_none());
    }

    #[test]
    fn no_apply_reason_distinguishes_no_zone_from_untracked() {
        let no_zone = Config::default();
        let with_zone = Config { zone: Some(A_ZONE), ..Config::default() };

        assert_eq!(no_apply_reason(&no_zone), "no Zone is set");
        assert_eq!(no_apply_reason(&with_zone), "the process is not a tracked game");
    }

    /// Regression coverage for the bug where a hotkey bound via Settings
    /// didn't take effect until a full app restart: `run_event_loop`'s
    /// `ShowSettings` branch reacts to Settings closing by calling
    /// [`register_hotkeys`] again, which reloads the config from disk and
    /// resolves it into fresh bindings. This test exercises exactly that
    /// reload-and-resolve chain -- load, simulate a Settings save that
    /// changes a binding, reload -- and asserts the new binding is picked
    /// up, i.e. a second read after a save is never stale.
    ///
    /// It deliberately stops short of calling [`register_hotkeys`] (or
    /// [`Hotkeys::register`]) itself: those also drive a real
    /// `GlobalHotKeyManager` OS registration, which -- per `crate::hotkeys`'s
    /// module doc comment -- needs a live Win32 event loop and is not
    /// exercised by this crate's automated tests, the same boundary that
    /// already excludes `run_event_loop` and `Tray::new` from testing here.
    /// Because of that boundary, this test cannot regression-guard the exact
    /// call site of the fix (the `hotkeys = register_hotkeys(config_dir);`
    /// line added to the `ShowSettings` branch) -- only a live message loop
    /// could do that. What it does guard is the reload mechanism that call
    /// site depends on: that reading the config a second time, after a
    /// Settings-style save, surfaces the change rather than returning
    /// stale/cached data.
    #[test]
    fn reloading_config_after_a_settings_style_save_resolves_the_newly_bound_hotkey() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");

        // Fresh install: nothing bound yet.
        let initial = profile_store::load(&config_dir).unwrap();
        assert_eq!(resolve_bindings(&initial.hotkeys).unwrap(), vec![]);

        // Simulates Settings persisting a newly bound hotkey on Save.
        let mut updated = initial.clone();
        updated.hotkeys.toggle_borderless = Some("F4".to_string());
        profile_store::save(&config_dir, &updated).unwrap();

        // Simulates the fix: reloading after Settings closes, instead of
        // continuing to act on the config snapshot loaded at startup.
        let reloaded = profile_store::load(&config_dir).unwrap();
        let reloaded_resolved = resolve_bindings(&reloaded.hotkeys).unwrap();

        assert_eq!(reloaded_resolved.len(), 1);
        assert_eq!(reloaded_resolved[0].action, HotkeyAction::ToggleBorderless);
        assert_eq!(reloaded_resolved[0].hotkey, "F4".parse::<HotKey>().unwrap());
    }
}
