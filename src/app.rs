//! Thin orchestrator: builds the tray icon/menu ([`crate::tray_ui`]) and
//! then runs the Win32 message loop that keeps it alive, dispatching a
//! click on "Exit" to a clean shutdown, a click on "Show Settings" to
//! lazily opening (and fully tearing down, on close) the egui Settings
//! window ([`crate::settings_ui`]), a press of the bound borderless hotkey
//! ([`crate::hotkeys`]) to stripping/restoring the foreground window's
//! border via [`crate::win32`], and a press of the bound taskbar-autohide
//! hotkey to toggling the taskbar hidden/visible (mode-aware -- see
//! [`crate::profile_store::TaskbarHideMode`]) via [`crate::win32`]. The
//! borderless and taskbar hotkey mechanics are fully independent: the
//! borderless toggle keeps its restore state per window in [`win32`], while
//! the taskbar toggle ([`taskbar::toggle_taskbar_hide`]) keeps none at all,
//! instead reading the taskbar's real OS-level state on every press --
//! either can be pressed without the other having been used first. A click
//! on one of the "Apply to Window" submenu's dynamically-listed windows
//! (recognized via [`tray_ui::decode_window_item_id`], distinct from the
//! five fixed top-level menu items) resolves that window's owning process
//! against the tracked-games list (adding it, with the new-tracked-game
//! defaults, if not already tracked -- see
//! [`crate::profile_store::upsert_tracked_game`]) and applies its assigned
//! Zone's borderless+reposition treatment to that specific window via
//! [`win32::apply_borderless_to_window`] -- a one-shot action with no
//! toggle/undo of its own (see [`handle_apply_to_window`]); the borderless
//! hotkey can still restore the window it placed.
//!
//! A third trigger requires no user action at all: [`crate::launch_watcher`]
//! registers a `SetWinEventHook` on the same message loop and, on every
//! foreground-window change, delegates to [`handle_auto_apply_launch`] --
//! which applies the Zone to a tracked game the moment it becomes the
//! foreground window, but only if that game's `auto_apply` flag is on (see
//! [`resolve_auto_apply`]).
//!
//! The taskbar is also restored automatically, with no hotkey press required:
//! every apply-path call site above that hides the taskbar for a specific
//! window arms [`taskbar::set_taskbar_hide_absolute_armed`]'s remembered
//! restore for that window (see `taskbar::ARMED_TASKBAR_RESTORE`'s doc
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
use crate::cursor_overlay::CursorOverlay;
use crate::hotkeys::{BindingOutcome, BindingReport, HotkeyAction, Hotkeys};
use crate::instance_channel;
use crate::launch_watcher;
use crate::message_box::show_message_box;
use crate::profile_store;
use crate::region_calc::{Region, resolve_region};
use crate::settings_ui::{self, TickContext, primary_monitor_geometry};
use crate::taskbar;
use crate::tray_ui::{self, Tray, TrayAction, TrayUiError};
use crate::update_ui::{
    Failure, UpdateEvent, UpdateEvents, handle_update_event, schedule_startup_update_check,
    start_install,
};
use crate::updater::{CheckOutcome, FlagGuard, SpawnError, UpdateInfo};
use crate::win32;
use crate::window_filter;
use crate::zone_overlay;
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use std::path::{Path, PathBuf};
use tray_icon::TrayIconEvent;
use tray_icon::menu::MenuEvent;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, IDYES, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_YESNO, MSG,
    TranslateMessage,
};

pub fn run() -> Result<(), TrayUiError> {
    let config_dir = config_dir();
    crate::logging::init(&config_dir);
    // `run` is only reached by the process that owns the single-instance mutex.
    crate::logging::claim_rotation();
    log!("GameFrame {} started", env!("CARGO_PKG_VERSION"));

    // The installer of an update cannot delete its own download.
    crate::update_install::clean_stale_downloads();

    // Undo a taskbar hide a crashed or killed run left behind,
    // before anything in this run can hide it, then guard against panics.
    crate::taskbar_recovery::init(&config_dir);
    crate::taskbar_recovery::recover_on_startup(&config_dir);
    crate::taskbar_recovery::install_panic_hook();

    let tray = Tray::new()?;

    // Lets a second launch ask this instance to open Settings.
    // Created on this thread so this thread's pumps dispatch its messages.
    let instance_channel = instance_channel::Receiver::create();
    // Needs that channel's window to ping; pings run on their own thread.
    sync_hang_watchdog(&config_dir, true);

    // First run (no Zone yet): offer to set one up once, now that the tray is up.
    prompt_for_zone_if_unset(&config_dir);

    let (hotkeys, hotkey_reports) = register_hotkeys(&config_dir);

    // Registered before entering `run_event_loop`'s `GetMessageW` pump --
    // see `crate::launch_watcher`'s module docs for why registration must
    // happen on this thread, before that loop starts.
    let launch_watcher_hook = launch_watcher::register(&config_dir);

    // A second, separate hook scoped to exactly
    // `EVENT_OBJECT_DESTROY`, rather than widening the foreground-change
    // hook's event range -- see `crate::launch_watcher`'s module docs for
    // why. Registered on this same thread for the same reason as
    // `launch_watcher_hook` above.
    let taskbar_restore_watcher_hook = launch_watcher::register_taskbar_restore_watcher();

    // Re-applies the Zone to a tracked game if the game itself resizes/restyles
    // its window afterwards (e.g. after an in-game settings change).
    let drift_watcher_hook = launch_watcher::register_drift_watcher();

    // The game cursor runs on its own thread so it keeps following the
    // mouse while this thread is blocked in Settings or the zone editor.
    let cursor_overlay = CursorOverlay::start(&config_dir);

    // Shortly after startup, off this thread; the result comes back through
    // `updates` (see `UpdateEvents`).
    let updates = UpdateEvents::new();
    schedule_startup_update_check(updates.notifier());

    run_event_loop(
        &tray,
        hotkeys,
        hotkey_reports,
        &config_dir,
        &cursor_overlay,
        &updates,
    );

    // Before the channel's window goes: the watchdog pings it.
    crate::watchdog::set_enabled(false);

    // Stops the game cursor thread (see `crate::cursor_overlay`).
    drop(cursor_overlay);
    drop(instance_channel);

    // If a Zone-apply's taskbar hide is still armed when the
    // user quits via the tray "Exit" item, restore it now -- before `tray`
    // is dropped below (which just removes the tray icon; it has no bearing
    // on the taskbar) and before the process actually exits, so the desktop
    // is never left with the taskbar hidden and no running GameFrame
    // instance left to bring it back.
    // Then anything still hidden (e.g. the manual hotkey toggle), the exit
    // safety net, and the marker.
    crate::taskbar::restore_all_for_exit();

    if let Some(hook) = launch_watcher_hook {
        launch_watcher::unregister(hook);
    }
    if let Some(hook) = taskbar_restore_watcher_hook {
        launch_watcher::unregister(hook);
    }
    if let Some(hook) = drift_watcher_hook {
        launch_watcher::unregister(hook);
    }

    // The log bridge's pending repeat counts are flushed by `main`, after this
    // returns, so every exit path (including errors) is covered.
    Ok(())
}

/// The user's answer to the first-run Zone prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstRunChoice {
    SetUpNow,
    Skip,
}

impl FirstRunChoice {
    /// Maps the prompt's `MessageBoxW` result: Yes sets up; anything else
    /// (No, closing the box) skips, so startup never gets stuck.
    fn from_message_box_result(result: i32) -> Self {
        if result == IDYES {
            Self::SetUpNow
        } else {
            Self::Skip
        }
    }
}

const FIRST_RUN_PROMPT: &str = "Welcome to GameFrame!\n\n\
A Zone is the area of your screen that games are placed into.\n\n\
Yes: set up the Zone now (draw it on screen).\n\
No: skip for now (set it up later in Settings; Settings stays limited until then).\n\n\
Set up the Zone now?";

/// When the loaded config has no Zone (see
/// [`profile_store::should_prompt_for_zone`]), explains what a Zone is and
/// asks whether to set it up now or skip. "Set up now" opens the zone editor
/// blank, persisting the Zone on Save exactly like Settings' "Draw zone" does.
/// Skip, Cancel, a close, or any failure leaves the Zone unset and startup
/// simply continues -- there is no re-prompt this session. No window is
/// placed and the taskbar is never touched here.
///
/// The prompt is a native message box on this (main) thread: its modal loop
/// pumps messages, so instance-channel commands are queued and handled by the
/// main loop afterwards, like during the editor.
///
/// OS-facing (opens real windows), so not unit-tested; the decision and the
/// answer mapping are.
fn prompt_for_zone_if_unset(config_dir: &Path) {
    let config = match profile_store::load(config_dir) {
        Ok(config) => config,
        Err(e) => {
            log!("failed to load config for the first-run zone prompt: {e}");
            return;
        }
    };
    if !profile_store::should_prompt_for_zone(&config) {
        return;
    }

    let answer = show_message_box(FIRST_RUN_PROMPT, MB_YESNO | MB_ICONINFORMATION);
    if FirstRunChoice::from_message_box_result(answer) == FirstRunChoice::Skip {
        return;
    }

    // No tray or hotkeys exist yet at first run, so there is nothing to tick.
    match zone_overlay::run_zone_editor_overlay(None, config.taskbar_hide_mode, &mut || {}) {
        Ok(Some(zone)) => {
            if let Err(e) = settings_ui::persist_zone(config_dir, Some(zone)) {
                log!("failed to save the zone: {e}");
            }
        }
        Ok(None) => {}
        Err(e) => log!("failed to open the zone editor overlay: {e}"),
    }
}

/// Loads the config and registers its bound hotkeys with the OS, each on its
/// own (see [`Hotkeys::register`]): one binding failing (e.g. another
/// application already holds that exact combination) leaves the others
/// working. Each failed binding is logged, and the per-binding report is
/// returned for Settings to show (also when the hotkey manager itself can't be
/// created: every binding is then reported failed). A malformed config file is
/// logged and yields no hotkeys at all -- GameFrame still starts, matching the
/// fresh-install "nothing bound yet" case.
fn register_hotkeys(config_dir: &Path) -> (Option<Hotkeys>, Vec<BindingReport>) {
    let config = match profile_store::load(config_dir) {
        Ok(config) => config,
        Err(e) => {
            log!("failed to load config for hotkey registration: {e}");
            return (None, Vec::new());
        }
    };

    let (hotkeys, reports) = Hotkeys::register(&config.hotkeys);
    for report in &reports {
        if let BindingOutcome::Failed(reason) = &report.outcome {
            log!(
                "failed to register hotkey {:?} for {:?}: {reason}",
                report.binding,
                report.action
            );
        }
    }
    (hotkeys, reports)
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
fn run_event_loop(
    tray: &Tray,
    mut hotkeys: Option<Hotkeys>,
    mut hotkey_reports: Vec<BindingReport>,
    config_dir: &Path,
    cursor_overlay: &CursorOverlay,
    updates: &UpdateEvents,
) {
    // The update the tray item "Update to vX.Y.Z" offers (ask mode).
    let mut offered_update: Option<UpdateInfo> = None;
    let ctx = Shared {
        tray,
        config_dir,
        cursor_overlay,
        updates,
    };

    // A Quit received during the first-run zone prompt (before this pump
    // existed) must not wait for the next message.
    if instance_channel::quit_pending() {
        return;
    }

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        // Waiting here is healthy; the label tells an idle pump from a stuck step.
        crate::watchdog::step("event loop: waiting for a message");
        crate::watchdog::idle();
        // 0 (WM_QUIT) or -1 (error) both mean: stop pumping.
        let ret = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if ret <= 0 {
            return;
        }
        crate::watchdog::step("event loop: dispatching a message");
        crate::watchdog::beat();
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        crate::watchdog::step("event loop: second-launch commands");
        // Commands from a second launch (see crate::instance_channel); the
        // channel's window is dispatched by this pump, so they are queued by now.
        // Keeps draining until the queue is empty, so a Quit that arrived
        // while Settings was open is handled as soon as it closes. Quit exits
        // through the same clean-shutdown path as the tray's Exit item below.
        if instance_channel::dispatch_commands(instance_channel::drain, || {
            show_settings(&ctx, &mut hotkeys, &mut hotkey_reports, &mut offered_update);
        }) {
            return;
        }

        // Tray, hotkey and update events. Settings drains the same events while
        // it is open (see `show_settings`), so nothing waits for it to close.
        loop {
            let drained = drain_events(&ctx, &hotkeys, &mut offered_update, None);
            if drained.exit {
                return;
            }
            if drained.show_settings {
                show_settings(&ctx, &mut hotkeys, &mut hotkey_reports, &mut offered_update);
                if instance_channel::quit_pending() {
                    return;
                }
                // Events queued behind the click are handled on the next pass.
                continue;
            }
            break;
        }
    }
}

/// The long-lived handles the event handlers share.
#[derive(Clone, Copy)]
struct Shared<'a> {
    tray: &'a Tray,
    config_dir: &'a Path,
    cursor_overlay: &'a CursorOverlay,
    updates: &'a UpdateEvents,
}

/// Whether a hotkey action may run while `context` (the window blocking the
/// main loop, `None` when none is) is running [`drain_events`].
///
/// The zone editor owns the taskbar (its session caches the state and its
/// guard restores on close), so the taskbar hotkey would desync it; the
/// borderless hotkey would only target the always-on-top overlay itself.
/// Everything else, the game-cursor hotkey included, runs as usual.
fn hotkey_allowed(context: Option<TickContext>, action: HotkeyAction) -> bool {
    match context {
        Some(TickContext::SettingsCapturing) => false,
        Some(TickContext::ZoneEditor) => matches!(action, HotkeyAction::ToggleGameCursor),
        Some(TickContext::Settings) | None => true,
    }
}

/// What one pass of [`drain_events`] asks its caller to do.
#[derive(Debug, Default, PartialEq, Eq)]
struct Drained {
    /// Exit GameFrame (the tray's Exit item, or an elevated relaunch).
    exit: bool,
    /// The tray's Settings item was clicked and Settings is not open.
    show_settings: bool,
    /// The game-cursor hotkey toggled `cursor.enabled`; the new value, for an
    /// open Settings window to show.
    cursor_enabled: Option<bool>,
}

/// Handles the pending update, tray-icon, tray-menu and hotkey events.
///
/// Shared by [`run_event_loop`] and, through [`show_settings`], the open
/// Settings window and the zone-editor overlay it launches (the `settings`
/// parameter says which), whose own loops block the main one: without it
/// nothing would react until they closed. Stops early on an exit request, or
/// on a Settings click while it is closed (events queued behind it stay in
/// their channels for the next call). With Settings open, that click focuses
/// the window instead. While the zone editor is open only the game-cursor
/// hotkey runs (see [`hotkey_allowed`]).
fn drain_events(
    ctx: &Shared,
    hotkeys: &Option<Hotkeys>,
    offered_update: &mut Option<UpdateInfo>,
    settings: Option<TickContext>,
) -> Drained {
    let Shared {
        tray,
        config_dir,
        cursor_overlay,
        updates,
    } = *ctx;
    let settings_open = settings.is_some();
    // Set by the hotkey loop. The two tray returns come before that loop, so
    // it is still `None` there; the borderless-elevated exit return is the one
    // that must report a toggle already persisted.
    let mut cursor_enabled = None;
    crate::watchdog::step("drain_events: update events");
    // Results of the startup update check and of installs started from
    // the tray (posted by worker threads, which wake this pump).
    while let Some(event) = updates.try_recv() {
        handle_update_event(event, tray, config_dir, offered_update);
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
    // Refreshing before every dispatched message would be wrong. The
    // primary, certain reason: `tray-icon` sets a 15ms `WM_TIMER` while the
    // mouse hovers over the icon, which would drive a full `EnumWindows` +
    // per-window `GetWindowTextW` + submenu rebuild (real Win32 syscalls)
    // up to ~66 times/second just from the cursor resting nearby --
    // needlessly expensive on this process's one and only message loop. A
    // secondary, less certain consideration: per the documented
    // `GetWindowTextW` contract, a same-process target window causes a
    // `WM_GETTEXT` to be sent via `SendMessage` and can block if that
    // window is stuck, whereas a cross-process target (the common case
    // when enumerating the whole desktop) is documented to read the cached
    // caption text directly instead, specifically so callers don't hang on
    // an unresponsive foreign process -- so the practical hang risk here
    // is narrower than "any hung window on the desktop" (more like "one of
    // GameFrame's own windows getting stuck"), but still worth avoiding
    // at no extra cost. No crate-native "menu about to show" event exists
    // to hook instead (see `Tray::refresh_apply_to_window_menu`'s doc
    // comment).
    crate::watchdog::step("drain_events: tray icon events");
    while let Ok(event) = TrayIconEvent::receiver().try_recv() {
        if matches!(
            event,
            TrayIconEvent::Enter { .. } | TrayIconEvent::Click { .. }
        ) {
            tray.refresh_apply_to_window_menu();
            tray.sync_start_with_windows_check();
        }
    }

    crate::watchdog::step("drain_events: tray menu events");
    while let Ok(event) = MenuEvent::receiver().try_recv() {
        if let Some(target_hwnd) = tray_ui::decode_window_item_id(event.id()) {
            handle_apply_to_window(config_dir, target_hwnd);
            // A newly tracked game gets the game cursor right away.
            cursor_overlay.reload(config_dir);
            continue;
        }

        match tray.ids().action_for(event.id()) {
            Some(TrayAction::Exit) => {
                // Returning to `run()` drops its owned `tray`, removing
                // the OS tray icon and destroying its hidden window --
                // the entire clean-shutdown path for the tray UI.
                return Drained {
                    exit: true,
                    cursor_enabled,
                    ..Drained::default()
                };
            }
            Some(TrayAction::ShowSettings) => {
                if settings_open {
                    instance_channel::focus_settings_window();
                } else {
                    return Drained {
                        show_settings: true,
                        cursor_enabled,
                        ..Drained::default()
                    };
                }
            }
            Some(TrayAction::ToggleStartWithWindows) => {
                // `muda` has already flipped the check mark, but the
                // registry -- not the menu -- is the source of truth:
                // toggle what the registry says, then re-sync the check
                // so a failed write doesn't leave it showing a lie.
                if let Err(e) = autostart::set_enabled(!autostart::is_enabled()) {
                    log!("failed to change Start with Windows: {e}");
                }
                tray.sync_start_with_windows_check();
            }
            Some(TrayAction::CheckForUpdates) => check_for_updates_from_tray(),
            Some(TrayAction::InstallUpdate) => {
                if let Some(info) = offered_update.clone() {
                    start_install(info, Failure::Report);
                }
            }
            None => {}
        }
    }

    crate::watchdog::step("drain_events: hotkey events");
    while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
        // Only act on the key-down edge; the crate also emits a
        // `Released` event on key-up (see `global_hotkey`'s Windows
        // implementation), which would otherwise double-toggle.
        if event.state() != HotKeyState::Pressed {
            continue;
        }
        // Settings is capturing a new binding: pressing a bound combo must
        // not also run its action.
        if settings == Some(TickContext::SettingsCapturing) {
            continue;
        }

        // With no hotkey bound (fresh install) `hotkeys` is `None`
        // (nothing was registered, so no event can arrive for it
        // anyway) or resolves to no action for `event.id()` --
        // either way, nothing happens here and nothing panics.
        let action = hotkeys.as_ref().and_then(|h| h.action_for(event.id()));
        if let Some(action) = action
            && !hotkey_allowed(settings, action)
        {
            log!("hotkey ignored while the zone editor is open; use the editor's own controls");
            continue;
        }
        match action {
            Some(HotkeyAction::ToggleBorderless) => {
                if handle_toggle_borderless(config_dir) {
                    // GameFrame relaunched itself elevated to complete a
                    // manipulation that failed for lack of permissions;
                    // this (unelevated) instance's job is done.
                    return Drained {
                        exit: true,
                        cursor_enabled,
                        ..Drained::default()
                    };
                }
            }
            Some(HotkeyAction::ToggleTaskbarAutohide) => {
                handle_toggle_taskbar_autohide(config_dir);
            }
            Some(HotkeyAction::ToggleGameCursor) => {
                if let Some(enabled) = handle_toggle_game_cursor(config_dir, cursor_overlay) {
                    cursor_enabled = Some(enabled);
                }
            }
            None => {}
        }
    }

    Drained {
        cursor_enabled,
        ..Drained::default()
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
    if config.zone.is_none() {
        "no Zone is set"
    } else {
        "the process is not a tracked game"
    }
}

/// Like [`resolve_tracked_apply`], but additionally requires the tracked
/// game's `auto_apply` flag to be on -- the extra gate the launch-detection
/// trigger (`crate::launch_watcher`) applies that the hotkey/tray "manual"
/// apply paths deliberately do not (`auto_apply` only governs whether a
/// launch is auto-applied without a hotkey press, not whether a manual
/// hotkey/tray press works).
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
/// *restore* (`is_restoring`, i.e. the foreground window has a remembered
/// [`win32::RestoreState`] -- see [`win32::plan_toggle`], which derives the
/// same strip-vs-restore decision from that same state) and whether the
/// foreground window's process currently resolves to a tracked game with a
/// Zone set (`tracked`, i.e. `resolve_tracked_apply(..).is_some()`).
///
/// Only the strip direction is gated on tracked-game membership: an
/// untracked foreground window blocks an initial strip, but never blocks a
/// pending restore purely because the *current* foreground window is
/// untracked. This is deliberate -- gating both directions uniformly would
/// permanently orphan a currently-stripped window in its borderless state
/// for the rest of the running session if its tracked game were removed via
/// Settings (or the Zone cleared) in between the strip press and the restore
/// press, since nothing else in the app can restore it once the remembered
/// `RestoreState` game is no longer resolvable. `plan_toggle`'s `Restore`
/// arm doesn't need a resolved Zone/region at all, so there is no
/// correctness reason to require one here.
///
/// Remembered restore state is per window, so a restore always applies to
/// the very window it was recorded for (the foreground window itself).
///
/// Pure -- no OS call -- so it's unit testable in complete isolation.
fn should_block_untracked_toggle(is_restoring: bool, tracked: bool) -> bool {
    !is_restoring && !tracked
}

/// Reacts to a press of the borderless-toggle hotkey: resolves the
/// foreground window's owning process against the tracked-games list (see
/// [`resolve_tracked_apply`]) and, if it's tracked (or a restore is already
/// pending for *this same window* -- see below), applies (or reverts) the
/// border-strip via [`win32::toggle_borderless_on_window`], then, on a
/// successful strip only, sets the taskbar absolutely from the global
/// `hide_taskbar_on_apply` and `taskbar_hide_mode`.
///
/// The tracked-game gate ([`should_block_untracked_toggle`]) only applies to
/// the *strip* direction: an untracked foreground window blocks an initial
/// strip (applying the Zone to a window whose process is not a tracked game
/// does nothing), but never blocks a pending *restore* purely because the
/// untracked window happens to be the one now in the foreground -- see that
/// function's doc comment for the full reasoning.
///
/// Restore state is remembered per window ([`win32::has_restore_state`]), so
/// the press acts on the foreground window alone: a window with state is
/// restored, any other is stripped (if tracked). Windows placed by the tray
/// picker, auto-apply or drift re-apply have state too, so the hotkey restores
/// them as well.
///
/// Taskbar auto-hide is set only on the strip direction, not on restore:
/// using the same fixed global boolean on both directions would leave the
/// taskbar auto-hidden after a full strip+restore cycle instead of it coming
/// back, whenever `hide_taskbar_on_apply` is on.
///
/// Returns `true` if GameFrame just relaunched itself elevated and this
/// process should exit (see [`win32::relaunch_elevated`]); `false` in every
/// other case (success, or a non-elevation failure that was just logged).
///
/// Reads the current config on every press (via [`profile_store::load`],
/// which serves its in-memory copy, refreshed by every save) so a Zone/game
/// added/edited via Settings since the last press is picked up immediately.
/// The hotkey binding itself is re-registered separately, right after the
/// Settings window closes -- see [`show_settings`] -- so a changed binding also
/// takes effect without a restart.
fn handle_toggle_borderless(config_dir: &Path) -> bool {
    let config = match profile_store::load(config_dir) {
        Ok(config) => config,
        Err(e) => {
            log!("borderless toggle: failed to load config: {e}");
            return false;
        }
    };

    let Some(fg_hwnd) = win32::foreground_window() else {
        log!("borderless toggle: no foreground window; ignoring");
        return false;
    };

    // Whether this press is about to restore (the window has a remembered
    // `RestoreState`) or strip (it has none) -- see
    // `should_block_untracked_toggle`'s doc comment for why only the strip
    // direction is gated on tracked-game membership below.
    let is_restoring = win32::has_restore_state(fg_hwnd);

    let tracked = window_filter::exe_path_for_window(fg_hwnd)
        .and_then(|exe_path| resolve_tracked_apply(&config, &exe_path));

    if should_block_untracked_toggle(is_restoring, tracked.is_some()) {
        log!(
            "borderless toggle: not applying ({}); ignoring",
            no_apply_reason(&config)
        );
        return false;
    }

    // Restoring stays allowed for any window; only stripping is refused.
    if !is_restoring && let Err(refusal) = window_filter::eligibility_for_hwnd(fg_hwnd) {
        log!("borderless toggle: refusing the window: {refusal}");
        return false;
    }

    let target_region = match &tracked {
        Some((tracked_game, zone)) => {
            let monitor = primary_monitor_geometry();
            match resolve_region(zone, &monitor) {
                Ok(region) => region,
                Err(e) => {
                    log!(
                        "borderless toggle: failed to resolve the Zone for {}: {e}",
                        tracked_game.display_name
                    );
                    return false;
                }
            }
        }
        // Only reachable while restoring -- the gate above already returned
        // for the strip direction when `tracked` is `None`. `plan_toggle`'s
        // `Restore` arm never reads `target_region`, so this placeholder
        // value is inert.
        None => Region {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
    };

    if let Err(e) = win32::toggle_borderless_on_window(fg_hwnd, target_region) {
        if e.is_access_denied() {
            log!("borderless toggle failed (access denied); requesting elevation");
            match win32::relaunch_elevated() {
                Ok(()) => return true,
                Err(relaunch_err) => log!("failed to relaunch elevated: {relaunch_err}"),
            }
        } else {
            log!("borderless toggle failed: {e}");
        }
        return false;
    }

    // Only set taskbar auto-hide on the strip direction -- setting it again
    // on restore (using the same fixed global boolean) would leave the
    // taskbar auto-hidden after the user exits borderless/game mode instead
    // of it coming back, whenever `hide_taskbar_on_apply` is on.
    // Coupling taskbar hide to the Zone apply is about the strip/apply
    // action, not the restore/undo action.
    //
    // Uses `set_taskbar_hide_absolute_armed` rather than the plain
    // `set_taskbar_hide_absolute` so that closing `fg_hwnd` later
    // automatically restores the taskbar with no hotkey press required.
    if !is_restoring
        && let Some((tracked_game, _)) = &tracked
        && let Err(e) = taskbar::set_taskbar_hide_absolute_armed(
            fg_hwnd,
            config.hide_taskbar_on_apply,
            config.taskbar_hide_mode,
        )
    {
        log!(
            "borderless toggle: failed to set the taskbar for {}: {e}",
            tracked_game.display_name
        );
    }

    false
}

/// Reacts to a click on one of the "Apply to Window" submenu's
/// dynamically-listed windows ([`tray_ui::decode_window_item_id`]): resolves
/// `target_hwnd`'s owning process's executable path, looks it up (or adds it,
/// with auto-apply on) in the tracked-games list via
/// [`profile_store::upsert_tracked_game`], persists that list update
/// immediately (even if a later step below fails), then applies the
/// Zone's borderless+reposition treatment to `target_hwnd` --
/// which may not be the foreground window at all, that's the whole point of
/// this tray-menu path -- via [`win32::apply_borderless_to_window`], and,
/// only once that border-apply succeeds, sets the taskbar absolutely from the
/// global `hide_taskbar_on_apply` and `taskbar_hide_mode`. A
/// border-apply failure (e.g. the target window is minimized) returns early
/// without touching the taskbar at all -- mirroring
/// [`handle_toggle_borderless`]'s equivalent gating -- since forcing the
/// taskbar away for a window that was never actually stripped
/// would contradict the whole point of coupling the two.
///
/// A one-shot action, not a toggle: selecting the same window again simply
/// re-applies the same treatment rather than undoing it. The window's
/// pre-strip state is remembered (see [`win32::apply_borderless_to_window`]),
/// so the borderless hotkey can restore it.
///
/// Reads the current config on every click, for the same reason
/// [`handle_toggle_borderless`] does.
///
/// Unlike [`handle_toggle_borderless`], a failure here (including an
/// access-denied failure) is only logged, not retried via
/// [`win32::relaunch_elevated`] -- reactive elevation is scoped to the
/// hotkey path.
fn handle_apply_to_window(config_dir: &Path, target_hwnd: HWND) {
    let mut config = match profile_store::load(config_dir) {
        Ok(config) => config,
        Err(e) => {
            log!("apply to window: failed to load config: {e}");
            return;
        }
    };

    let exe_path = match window_filter::eligibility_for_hwnd(target_hwnd) {
        Ok(exe_path) => exe_path,
        Err(refusal) => {
            log!("apply to window: refusing the window: {refusal}");
            return;
        }
    };

    let display_name = win32::window_text(target_hwnd);
    let index =
        profile_store::upsert_tracked_game(&mut config.tracked_games, exe_path, display_name);
    let tracked_game = config.tracked_games[index].clone();

    // Persist the (possibly newly-added) tracked game immediately, before
    // attempting the region/border/taskbar steps below -- so a downstream
    // failure (e.g. no Zone set, or the window manipulation
    // itself failing) never costs the user the "this window is now tracked"
    // outcome that "Apply to Window" is documented to guarantee.
    if let Err(e) = profile_store::save(config_dir, &config) {
        log!(
            "apply to window: failed to save tracked game {}: {e}",
            tracked_game.display_name
        );
    }

    let Some(zone) = config.zone else {
        log!(
            "apply to window: no Zone is set; {} is tracked but nothing was applied",
            tracked_game.display_name
        );
        return;
    };

    let monitor = primary_monitor_geometry();
    let target_region = match resolve_region(&zone, &monitor) {
        Ok(region) => region,
        Err(e) => {
            log!(
                "apply to window: failed to resolve the Zone for {}: {e}",
                tracked_game.display_name
            );
            return;
        }
    };

    if let Err(e) = win32::apply_borderless_to_window(target_hwnd, target_region) {
        log!("apply to window failed: {e}");
        return;
    }
    win32::remember_applied_window(target_hwnd);
    // Picking the window from the tray is an explicit request, which
    // overrides an earlier hotkey restore of it.
    win32::allow_auto_apply(target_hwnd);

    // `set_taskbar_hide_absolute_armed` rather than the plain
    // `set_taskbar_hide_absolute`: arms `target_hwnd` so closing it later
    // automatically restores the taskbar with no hotkey press required.
    if let Err(e) = taskbar::set_taskbar_hide_absolute_armed(
        target_hwnd,
        config.hide_taskbar_on_apply,
        config.taskbar_hide_mode,
    ) {
        log!(
            "apply to window: failed to set the taskbar for {}: {e}",
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
/// (including its border-apply-must-succeed-first gate on the taskbar step)
/// and [`win32::apply_borderless_to_window`]
/// itself, rather than the hotkey path's toggle/restore mechanism -- a
/// launch-detected apply has no "undo" concept, since there's nothing to
/// restore from (the game just started).
///
/// Unlike [`handle_apply_to_window`], this never adds a new tracked game or
/// persists the config: a game must already be tracked, with auto-apply on,
/// to reach an actual apply here at all (see [`resolve_auto_apply`]'s doc
/// comment), so there is no upsert/save step.
///
/// Reads the current config on every foreground-change event, matching
/// [`handle_toggle_borderless`]/[`handle_apply_to_window`].
///
/// OS-facing (resolves a real `HWND`'s owning process and applies real
/// window/taskbar mutations) -- not exercised by this crate's automated
/// tests, per this crate's convention for OS-facing code.
/// [`resolve_auto_apply`] above is the pure decision this function
/// delegates to, and is what's actually unit tested.
pub(crate) fn handle_auto_apply_launch(config_dir: &Path, hwnd: HWND) {
    // The user restored this window with the hotkey: leave it alone.
    if win32::auto_apply_suppressed(hwnd) {
        return;
    }

    let config = match profile_store::load(config_dir) {
        Ok(config) => config,
        Err(e) => {
            log!("launch detection: failed to load config: {e}");
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
            && profile_store::find_tracked_game(&config.tracked_games, &exe_path)
                .is_some_and(|g| g.auto_apply)
        {
            log!(
                "launch detection: not applying to a tracked game ({})",
                no_apply_reason(&config)
            );
        }
        return;
    };

    if let Err(refusal) = window_filter::eligibility_for_hwnd(hwnd) {
        log!(
            "launch detection: refusing the window of {}: {refusal}",
            tracked_game.display_name
        );
        return;
    }

    let monitor = primary_monitor_geometry();
    let target_region = match resolve_region(&zone, &monitor) {
        Ok(region) => region,
        Err(e) => {
            log!(
                "launch detection: failed to resolve the Zone for {}: {e}",
                tracked_game.display_name
            );
            return;
        }
    };

    if let Err(e) = win32::apply_borderless_to_window(hwnd, target_region) {
        log!(
            "launch detection: failed to apply borderless treatment to {}: {e}",
            tracked_game.display_name
        );
        return;
    }
    // Lets the location-change watcher re-apply if the game later resizes
    // or restyles itself (e.g. after an in-game settings change).
    win32::remember_applied_window(hwnd);

    // `set_taskbar_hide_absolute_armed` rather than the plain
    // `set_taskbar_hide_absolute`: arms `hwnd` so closing the game later
    // automatically restores the taskbar with no hotkey press required.
    if let Err(e) = taskbar::set_taskbar_hide_absolute_armed(
        hwnd,
        config.hide_taskbar_on_apply,
        config.taskbar_hide_mode,
    ) {
        log!(
            "launch detection: failed to set the taskbar for {}: {e}",
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
/// Minimized windows are skipped and re-applies are rate-limited per window.
/// Returns how long to wait before re-checking when the event was skipped only
/// because of the rate limit, so a drift that lands inside the limit is still
/// corrected.
///
/// OS-facing; the drift decisions are [`win32::window_drifted`] and the
/// re-apply decision behind `win32::decide_drift_for_window`, unit tested.
pub(crate) fn handle_applied_window_drift(
    config_dir: &Path,
    hwnd: HWND,
) -> Option<std::time::Duration> {
    // A minimized window's rect is its off-screen minimized position, not drift.
    if win32::auto_apply_suppressed(hwnd) || win32::is_window_minimized(hwnd) {
        return None;
    }
    let exe_path = window_filter::exe_path_for_window(hwnd)?;
    let config = profile_store::load(config_dir).ok()?;
    let (tracked_game, zone) = resolve_auto_apply(&config, &exe_path)?;
    let target_region = resolve_region(&zone, &primary_monitor_geometry()).ok()?;
    let drifted = matches!(win32::window_has_drifted(hwnd, target_region), Ok(true));
    match win32::decide_drift_for_window(hwnd, drifted) {
        win32::DriftDecision::Reapply => {}
        win32::DriftDecision::SkipRateLimited { retry_in } => return Some(retry_in),
        win32::DriftDecision::SkipNotDrifted => return None,
    }
    // Recorded before the eligibility check so a refused window is rate
    // limited (and logged) like any other attempt.
    win32::record_drift_attempt(hwnd);
    if let Err(refusal) = window_filter::eligibility_for_hwnd(hwnd) {
        log!(
            "drift re-apply: refusing the window of {}: {refusal}",
            tracked_game.display_name
        );
        return None;
    }
    if let Err(e) = win32::apply_borderless_to_window(hwnd, target_region) {
        log!(
            "drift re-apply: failed for {}: {e}",
            tracked_game.display_name
        );
    }
    None
}

/// Whether one of the tray's update message boxes is open.
static UPDATE_RESULT_BOX_OPEN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Runs an update check on a worker thread (the event loop never waits on the
/// network) and shows the result in a message box from that thread; an
/// available update is offered for installation. Ignored while a message box
/// of an earlier check is still open; while a Settings-started check is still
/// fetching, a box says so.
fn check_for_updates_from_tray() {
    let Some(box_open) = FlagGuard::acquire(&UPDATE_RESULT_BOX_OPEN) else {
        log!("update check: the previous result is still shown");
        return;
    };
    let started = crate::updater::spawn_check(move |result| {
        let _box_open = box_open;
        match result {
            Ok(CheckOutcome::UpdateAvailable(info)) => offer_install(info),
            result => {
                if matches!(result, Ok(CheckOutcome::UpToDate)) {
                    crate::update_ui::notify(UpdateEvent::UpToDate);
                }
                let icon = if result.is_ok() {
                    MB_ICONINFORMATION
                } else {
                    MB_ICONWARNING
                };
                show_message_box(&crate::updater::result_message(&result), MB_OK | icon);
            }
        }
    });
    let why_not = match started {
        Ok(()) => return,
        Err(SpawnError::Busy) => "An update check is already running. Try again in a moment.",
        Err(SpawnError::ThreadFailed) => "Couldn't start the update check.",
    };
    // The flag went down with the dropped closure; take it again for the box
    // that tells the user why nothing was checked.
    if let Some(box_open) = FlagGuard::acquire(&UPDATE_RESULT_BOX_OPEN) {
        std::thread::spawn(move || {
            let _box_open = box_open;
            show_message_box(why_not, MB_OK | MB_ICONINFORMATION);
        });
    }
}

/// Asks whether to install the available update `info` and, on yes, installs
/// it on a worker thread: GameFrame then quits and the installer starts it
/// again. Blocks on the question, so call it from a worker thread.
fn offer_install(info: crate::updater::UpdateInfo) {
    let question = format!(
        "{}\n\nInstall it now? GameFrame will close and start again.",
        CheckOutcome::UpdateAvailable(info.clone()).message()
    );
    if show_message_box(&question, MB_YESNO | MB_ICONINFORMATION) != IDYES {
        return;
    }
    start_install(info, Failure::Report);
}

/// Reacts to a press of the taskbar-autohide-toggle hotkey: flips the
/// taskbar's *actual* hidden/visible state via [`taskbar::toggle_taskbar_hide`],
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
/// Reads the current (cached) config on every press, as
/// [`handle_toggle_borderless`] does, so a mode change made via Settings
/// since the last press is picked up immediately.
fn handle_toggle_taskbar_autohide(config_dir: &Path) {
    let mode = match profile_store::load(config_dir) {
        Ok(config) => config.taskbar_hide_mode,
        Err(e) => {
            log!("taskbar auto-hide toggle: failed to load config: {e}");
            return;
        }
    };

    if let Err(e) = taskbar::toggle_taskbar_hide(mode) {
        log!("taskbar auto-hide toggle failed: {e}");
    }
}

/// Reacts to a press of the game-cursor hotkey: flips the persisted
/// `cursor.enabled` flag (so Settings and the next start agree with what's on
/// screen), hands the change to the overlay thread and, when turning it on,
/// plays the find-my-cursor pulse so the user sees where it is.
///
/// Returns the new `cursor.enabled`, or `None` when the toggle failed.
fn handle_toggle_game_cursor(config_dir: &Path, cursor_overlay: &CursorOverlay) -> Option<bool> {
    let mut config = match profile_store::load(config_dir) {
        Ok(config) => config,
        Err(e) => {
            log!("game cursor toggle: failed to load config: {e}");
            return None;
        }
    };
    config.cursor.enabled = !config.cursor.enabled;
    if let Err(e) = profile_store::save(config_dir, &config) {
        log!("game cursor toggle: failed to save config: {e}");
        return None;
    }
    cursor_overlay.reload(config_dir);
    if config.cursor.enabled {
        cursor_overlay.pulse();
    }
    Some(config.cursor.enabled)
}

/// Opens Settings (what the tray item and a second launch both do) and, once
/// it closes, applies what it may have changed.
fn show_settings(
    ctx: &Shared,
    hotkeys: &mut Option<Hotkeys>,
    hotkey_reports: &mut Vec<BindingReport>,
    offered_update: &mut Option<UpdateInfo>,
) {
    let Shared {
        tray,
        config_dir,
        cursor_overlay,
        ..
    } = *ctx;
    // Synchronous/modal-style: opening Settings blocks this thread until the
    // Settings window closes, at which point the egui/wgpu context it used is
    // fully dropped and we resume pumping tray messages right where we left
    // off. Meanwhile Settings' frame loop calls back into `drain_events`, so
    // tray, hotkey and update events are handled immediately instead of
    // queueing until it closes.
    //
    // The outcome of the last registration is handed to Settings instead of
    // re-checking: while it is open this instance's own registrations hold
    // the combos, so a fresh probe would report GameFrame's own bindings as
    // "in use". Settings hides a failure once its binding has been changed.
    let mut on_tick = {
        let hotkeys = &*hotkeys;
        move |context: TickContext| -> Option<bool> {
            let drained = drain_events(ctx, hotkeys, offered_update, Some(context));
            if drained.exit {
                // Settings closes on a queued Quit and the main loop exits.
                instance_channel::queue_quit();
            }
            // Back in Settings' frame loop (rendering, window messages): a stall
            // from here until the next tick happened in Settings itself (#128).
            crate::watchdog::step("settings: frame loop");
            drained.cursor_enabled
        }
    };
    crate::watchdog::step("settings: opening window");
    // Window and GPU setup can take a while and are not a hang: idle until
    // Settings' first `ui()` beats; busy again once it has closed.
    crate::watchdog::idle();
    if let Err(e) = open_settings(config_dir, hotkey_reports, &mut on_tick) {
        log!("failed to open Settings: {e}");
    }
    crate::watchdog::beat();
    crate::watchdog::step("settings: closed, re-registering hotkeys");

    // Settings may have changed the hotkey bindings on disk (Save persists via
    // `profile_store`) -- re-register from the freshly saved config so a newly
    // bound/changed/removed hotkey takes effect immediately, without requiring
    // a full app restart. The old `Hotkeys` must be dropped *before*
    // registering again (its `Drop` destroys the manager's hidden window,
    // which releases its hotkeys): a plain `hotkeys = register_hotkeys(..)`
    // evaluates the right side first, so every unchanged key failed with
    // "already registered" and all hotkeys went dead after any Settings visit.
    drop(hotkeys.take());
    (*hotkeys, *hotkey_reports) = register_hotkeys(config_dir);
    sync_hang_watchdog(config_dir, false);
    cursor_overlay.reload(config_dir);
    tray.sync_start_with_windows_check();
}

/// Starts or stops the hang watchdog to match the saved `log_ui_hangs`
/// setting. If the config can't be read the watchdog keeps its current state,
/// except at startup, where it starts (a diagnostic fails on).
fn sync_hang_watchdog(config_dir: &Path, startup: bool) {
    match profile_store::load(config_dir) {
        Ok(config) => crate::watchdog::set_enabled(config.log_ui_hangs),
        Err(e) => {
            log!("hang watchdog: could not read the config: {e}");
            if startup {
                crate::watchdog::set_enabled(true);
            }
        }
    }
}

/// Opens the Settings window at `config_dir`. See
/// [`settings_ui::run_settings_window`] for what "opens" entails (blocks
/// until closed, persists on Save).
fn open_settings(
    config_dir: &Path,
    hotkey_reports: &[BindingReport],
    on_tick: &mut dyn FnMut(TickContext) -> Option<bool>,
) -> Result<(), settings_ui::SettingsUiError> {
    // While open, a second launch's OpenSettings focuses this window instead.
    let _modal = instance_channel::enter_settings();
    settings_ui::run_settings_window(config_dir, hotkey_reports, on_tick)
}

/// The real `%APPDATA%\GameFrame` directory GameFrame persists its config
/// under. Falls back to a `GameFrame` directory relative to the current
/// working directory in the (in-practice-never-on-real-Windows) case where
/// the `APPDATA` environment variable isn't set, rather than panicking.
pub(crate) fn config_dir() -> PathBuf {
    match std::env::var("APPDATA") {
        Ok(appdata) => PathBuf::from(appdata).join("GameFrame"),
        Err(_) => PathBuf::from("GameFrame"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkeys::register_each;
    use crate::profile_store::{Config, TrackedGame, Zone};
    use global_hotkey::hotkey::HotKey;
    use std::path::PathBuf;
    use windows_sys::Win32::UI::WindowsAndMessaging::IDNO;

    #[test]
    fn zone_editor_tick_allows_only_the_cursor_hotkey() {
        use HotkeyAction::*;
        assert!(hotkey_allowed(
            Some(TickContext::ZoneEditor),
            ToggleGameCursor
        ));
        assert!(!hotkey_allowed(
            Some(TickContext::ZoneEditor),
            ToggleTaskbarAutohide
        ));
        assert!(!hotkey_allowed(
            Some(TickContext::ZoneEditor),
            ToggleBorderless
        ));
    }

    #[test]
    fn settings_ticks_allow_every_hotkey_except_while_capturing() {
        use HotkeyAction::*;
        for action in [ToggleBorderless, ToggleTaskbarAutohide, ToggleGameCursor] {
            assert!(hotkey_allowed(None, action));
            assert!(hotkey_allowed(Some(TickContext::Settings), action));
            assert!(!hotkey_allowed(
                Some(TickContext::SettingsCapturing),
                action
            ));
        }
    }

    #[test]
    fn first_run_choice_sets_up_only_on_yes() {
        assert_eq!(
            FirstRunChoice::from_message_box_result(IDYES),
            FirstRunChoice::SetUpNow
        );
        assert_eq!(
            FirstRunChoice::from_message_box_result(IDNO),
            FirstRunChoice::Skip
        );
        assert_eq!(
            FirstRunChoice::from_message_box_result(0),
            FirstRunChoice::Skip
        );
    }

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
        // state for the rest of the run.
        assert!(!should_block_untracked_toggle(true, false));
    }

    #[test]
    fn should_block_untracked_toggle_allows_a_tracked_restore() {
        assert!(!should_block_untracked_toggle(true, true));
    }

    fn config_with_zone_and_game(zone: Option<Zone>, auto_apply: bool) -> (Config, PathBuf) {
        let mut config = Config {
            zone,
            ..Config::default()
        };
        let exe_path = PathBuf::from("C:\\Games\\thing.exe");
        let mut game = TrackedGame::new_default(exe_path.clone(), "Thing".to_string());
        game.auto_apply = auto_apply;
        config.tracked_games.push(game);
        (config, exe_path)
    }

    const A_ZONE: Zone = Zone {
        x: 10,
        y: 20,
        width: 300,
        height: 200,
    };

    #[test]
    fn resolve_tracked_apply_returns_none_for_an_untracked_executable_path() {
        let config = Config {
            zone: Some(A_ZONE),
            ..Config::default()
        };

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

        let (game, zone) =
            resolve_tracked_apply(&config, &exe_path).expect("expected a tracked game match");

        assert_eq!(game.display_name, "Thing");
        assert_eq!(zone, A_ZONE);
    }

    #[test]
    fn resolve_auto_apply_returns_the_game_and_the_zone_when_auto_apply_is_on() {
        let (config, exe_path) = config_with_zone_and_game(Some(A_ZONE), true);

        let (game, zone) =
            resolve_auto_apply(&config, &exe_path).expect("expected a resolved auto-apply match");

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
        let config = Config {
            zone: Some(A_ZONE),
            ..Config::default()
        };

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
        let with_zone = Config {
            zone: Some(A_ZONE),
            ..Config::default()
        };

        assert_eq!(no_apply_reason(&no_zone), "no Zone is set");
        assert_eq!(
            no_apply_reason(&with_zone),
            "the process is not a tracked game"
        );
    }

    /// Regression coverage for the bug where a hotkey bound via Settings
    /// didn't take effect until a full app restart: `run_event_loop`'s
    /// `ShowSettings` branch reacts to Settings closing by calling
    /// [`register_hotkeys`] again, which loads the saved config and
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
        assert!(register_each(&initial.hotkeys, |_| Ok(())).0.is_empty());

        // Simulates Settings persisting a newly bound hotkey on Save.
        let mut updated = initial.clone();
        updated.hotkeys.toggle_borderless = Some("F4".to_string());
        profile_store::save(&config_dir, &updated).unwrap();

        // Simulates the fix: reloading after Settings closes, instead of
        // continuing to act on the config snapshot loaded at startup.
        let reloaded = profile_store::load(&config_dir).unwrap();
        let (reloaded_resolved, _) = register_each(&reloaded.hotkeys, |_| Ok(()));

        assert_eq!(reloaded_resolved.len(), 1);
        assert_eq!(reloaded_resolved[0].action, HotkeyAction::ToggleBorderless);
        assert_eq!(reloaded_resolved[0].hotkey, "F4".parse::<HotKey>().unwrap());
    }
}
