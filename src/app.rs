//! Thin orchestrator: builds the tray icon/menu ([`crate::tray_ui`]) and
//! then runs the Win32 message loop that keeps it alive, dispatching a
//! click on "Exit" to a clean shutdown, a click on "Show Settings" to
//! starting a short-lived child process that shows the egui Settings
//! window ([`crate::settings_ui`], see [`crate::settings_link`]) and applying
//! what it changed once it exits, a press of the bound borderless hotkey
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
//! [`placement::apply_zone`] -- a one-shot action with no toggle/undo of its
//! own; the borderless hotkey can still restore the window it placed.
//!
//! A third trigger requires no user action at all: [`crate::launch_watcher`]
//! registers a `SetWinEventHook` on the same message loop and, on every
//! foreground-window change, delegates to
//! [`placement::handle_auto_apply_launch`] -- which applies the Zone to a
//! tracked game the moment it becomes the foreground window, but only if that
//! game's `auto_apply` flag is on (see [`placement::resolve_auto_apply`]). All
//! the apply triggers share the one [`placement::apply_zone`] pipeline.
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
use crate::placement::{self, Outcome, Trigger};
use crate::profile_store;
use crate::settings_host::{self, SettingsProcess};
use crate::settings_link::{ChildKind, EXIT_ZONE_SAVED, InstallProgress, ParentMsg};
use crate::settings_ui::TickContext;
use crate::taskbar;
use crate::tray_ui::{self, Tray, TrayAction, TrayUiError};
use crate::update_ui::{
    Failure, UpdateEvent, UpdateEvents, handle_update_event, schedule_startup_update_check,
    start_install,
};
use crate::updater::{CheckOutcome, FlagGuard, SpawnError, UpdateInfo};
use crate::win32;
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use std::path::{Path, PathBuf};
use tray_icon::TrayIconEvent;
use tray_icon::menu::MenuEvent;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, IDYES, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_YESNO, MSG,
    PM_REMOVE, PeekMessageW, PostQuitMessage, TranslateMessage, WM_QUIT,
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
    // No hook exists until a window is applied; then one per game process.
    launch_watcher::enable_drift_watcher();

    // The game cursor runs on its own thread so it keeps following the
    // mouse while this thread is busy (a hotkey, a live re-apply).
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
    launch_watcher::disable_drift_watcher();

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

/// Shown after the first-run Zone is saved: what to do next and where the
/// tray icon is (Windows 11 may hide a new icon in the overflow area).
const ZONE_SAVED_NEXT_STEPS: &str = "Zone saved.\n\n\
Next: start your game in windowed or borderless-windowed mode. Then either track it in \
Settings > Games, or right-click the GameFrame tray icon and choose Apply to Window > your game.\n\n\
The tray icon may be hidden under the ^ arrow in the taskbar. Launching GameFrame again \
also opens Settings.";

/// When the loaded config has no Zone (see
/// [`profile_store::should_prompt_for_zone`]), explains what a Zone is and
/// asks whether to set it up now or skip. "Set up now" opens the zone editor
/// blank, persisting the Zone on Save exactly like Settings' "Draw Zone" does.
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

    // The editor runs in a child process (it saves the Zone itself and reports
    // that through its exit code), so the GPU driver never loads here.
    match run_zone_editor_process() {
        Ok(exit_code) => {
            // The child wrote the config file behind this process's cache.
            profile_store::forget_cached(config_dir);
            if exit_code == Some(EXIT_ZONE_SAVED) {
                show_message_box(ZONE_SAVED_NEXT_STEPS, MB_OK | MB_ICONINFORMATION);
            }
        }
        Err(e) => log!("failed to open the zone editor: {e}"),
    }
}

/// Runs the first-run zone editor child to completion and returns its exit
/// code. Keeps pumping this thread's messages while it waits, so the tray and a
/// second launch's command stay responsive; a queued `Quit` closes the editor.
fn run_zone_editor_process() -> std::io::Result<Option<i32>> {
    let mut process = SettingsProcess::spawn(ChildKind::ZoneEditor, Vec::new(), None)?;
    let mut closing = false;
    Ok(crate::watchdog::modal(|| {
        loop {
            if !pump_pending_messages() {
                // Leave the WM_QUIT for the main loop; stop waiting.
                process.close();
            }
            let polled = process.poll();
            if polled.exited {
                return polled.exit_code;
            }
            if !closing && instance_channel::quit_pending() {
                closing = true;
                process.send(&ParentMsg::Close);
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }))
}

/// Dispatches the messages waiting for this thread. `false` after a `WM_QUIT`,
/// which is posted again so the next pump sees it too.
fn pump_pending_messages() -> bool {
    // SAFETY: all-zero bytes are a valid value of this plain C struct.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // SAFETY: `msg` is a valid out-pointer; the calls take the message just read.
    unsafe {
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            if msg.message == WM_QUIT {
                PostQuitMessage(msg.wParam as i32);
                return false;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    true
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
/// `hotkeys` is owned (not borrowed) specifically so [`settings_closed`] can
/// reassign it with a freshly re-registered [`Hotkeys`] after the Settings
/// window closes -- see that function's comment.
///
/// The Settings window runs in a child process (#186), so this loop never
/// blocks in it: tray, hotkeys, hooks and a second launch's commands keep being
/// handled as usual, and [`settings_host::service_settings`] picks up the child's messages.
fn run_event_loop(
    tray: &Tray,
    hotkeys: Option<Hotkeys>,
    hotkey_reports: Vec<BindingReport>,
    config_dir: &Path,
    cursor_overlay: &CursorOverlay,
    updates: &UpdateEvents,
) {
    let mut state = LoopState {
        hotkeys,
        hotkey_reports,
        // The update the tray item "Update to vX.Y.Z" offers (ask mode).
        offered_update: None,
        settings: None,
    };
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

    // Prewarm the "Apply to Window" list so the first menu open has content.
    tray.refresh_apply_to_window_menu(config_dir);

    // SAFETY: all-zero bytes are a valid value of this plain C struct.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        // Waiting here is healthy; the label tells an idle pump from a stuck step.
        crate::watchdog::step("event loop: waiting for a message");
        crate::watchdog::idle();
        // 0 (WM_QUIT) or -1 (error) both mean: stop pumping.
        // SAFETY: `msg` is a valid out-pointer for the call.
        let ret = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if ret <= 0 {
            return;
        }
        crate::watchdog::step("event loop: dispatching a message");
        crate::watchdog::beat();
        // SAFETY: both calls take the `msg` just filled in by `GetMessageW`.
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        crate::watchdog::step("event loop: settings process");
        // The Settings child's messages, and its exit (which re-registers the
        // hotkeys it may have changed). First, so the window it reports is
        // current for the hotkey events below.
        let child_exited = settings_host::service_settings(
            ctx.tray,
            ctx.config_dir,
            &mut state.settings,
            &mut state.offered_update,
        );
        if child_exited {
            settings_closed(&ctx, &mut state);
        }

        crate::watchdog::step("event loop: second-launch commands");
        // Commands from a second launch (see crate::instance_channel); the
        // channel's window is dispatched by this pump, so they are queued by now.
        // Quit exits through the same clean-shutdown path as the tray's Exit
        // item below (which also closes the Settings child).
        if instance_channel::dispatch_commands(instance_channel::drain, || {
            show_settings(&ctx, &mut state);
        }) {
            return;
        }

        // Tray, hotkey and update events.
        loop {
            let settings_context = state.settings.as_ref().map(|s| s.context);
            let mut install_progress = Vec::new();
            let drained = drain_events(
                &ctx,
                &state.hotkeys,
                &mut state.offered_update,
                settings_context,
                &mut install_progress,
            );
            settings_host::forward_install_progress(&mut state.settings, install_progress);
            if drained.exit {
                return;
            }
            // A game-cursor toggle by hotkey: the open window shows the new value.
            if let (Some(enabled), Some(settings)) =
                (drained.cursor_enabled, state.settings.as_mut())
            {
                settings.send(&ParentMsg::CursorEnabled(enabled));
            }
            if drained.show_settings {
                show_settings(&ctx, &mut state);
                // Events queued behind the click are handled on the next pass.
                continue;
            }
            break;
        }
    }
}

/// What the event loop owns and the Settings child's lifecycle touches.
struct LoopState {
    hotkeys: Option<Hotkeys>,
    hotkey_reports: Vec<BindingReport>,
    offered_update: Option<UpdateInfo>,
    /// The running Settings child, if any. Dropping it closes it.
    settings: Option<SettingsProcess>,
}

/// The long-lived handles the event handlers share.
#[derive(Clone, Copy)]
struct Shared<'a> {
    tray: &'a Tray,
    config_dir: &'a Path,
    cursor_overlay: &'a CursorOverlay,
    updates: &'a UpdateEvents,
}

/// Whether a hotkey action may run while `context` (the window the Settings
/// child reports as ticking, `None` when no child is open) is current.
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
/// Called by [`run_event_loop`] after every pumped message. `settings` is the
/// window the Settings child last reported (`None` when no child is open).
/// Stops early on an exit request, or on a Settings click while it is closed
/// (events queued behind it stay in their channels for the next call). With
/// Settings open, that click focuses the window instead. While the zone editor
/// is open only the game-cursor hotkey runs (see [`hotkey_allowed`]). The
/// progress of installs Settings asked for is appended to `install_progress` as
/// (request id, progress), for [`settings_host::forward_install_progress`].
fn drain_events(
    ctx: &Shared,
    hotkeys: &Option<Hotkeys>,
    offered_update: &mut Option<UpdateInfo>,
    settings: Option<TickContext>,
    install_progress: &mut Vec<(Option<u64>, InstallProgress)>,
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
        install_progress.extend(InstallProgress::from_event(&event));
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
    // The window list is built on a worker thread (see
    // `Tray::refresh_apply_to_window_menu`); a refresh started by a hover
    // below that has finished since the last pass swaps its result into the
    // submenu here.
    tray.apply_finished_window_refresh();

    crate::watchdog::step("drain_events: tray icon events");
    while let Ok(event) = TrayIconEvent::receiver().try_recv() {
        if matches!(
            event,
            TrayIconEvent::Enter { .. } | TrayIconEvent::Click { .. }
        ) {
            tray.refresh_apply_to_window_menu(config_dir);
            tray.sync_start_with_windows_check();
        }
    }

    crate::watchdog::step("drain_events: tray menu events");
    while let Ok(event) = MenuEvent::receiver().try_recv() {
        if let Some(target_hwnd) = tray_ui::decode_window_item_id(event.id()) {
            let outcome = placement::apply_zone(config_dir, target_hwnd, Trigger::Tray);
            if let Some(notice) = placement::failure_notice(Trigger::Tray, &outcome) {
                tray.notify(&notice);
            }
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
                if handle_toggle_borderless(config_dir, tray) {
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

/// Reacts to a press of the borderless-toggle hotkey on the foreground window.
///
/// Restore state is remembered per window ([`win32::has_restore_state`]), so
/// the press acts on that window alone: a window with state is restored (any
/// window, so removing a game from Settings never orphans a stripped one), any
/// other is stripped through [`placement::apply_zone`], which refuses an
/// untracked or ineligible window, sets the taskbar per the config, and
/// remembers the window for drift correction. Windows placed by the tray
/// picker, auto-apply or drift re-apply have state too, so the hotkey restores
/// them as well.
///
/// The taskbar is touched on the strip direction only, never on restore:
/// otherwise a strip+restore cycle would leave it hidden.
///
/// Returns `true` if GameFrame just relaunched itself elevated and this
/// process should exit (see [`win32::relaunch_elevated`]); `false` in every
/// other case (success, or a failure that was just logged and shown to the
/// user through `tray`, at most once per press).
fn handle_toggle_borderless(config_dir: &Path, tray: &Tray) -> bool {
    let Some(fg_hwnd) = win32::foreground_window() else {
        log!("borderless toggle: no foreground window; ignoring");
        return false;
    };

    let error = if win32::has_restore_state(fg_hwnd) {
        match win32::restore_window(fg_hwnd) {
            Ok(()) => return false,
            Err(e) => {
                if !e.is_access_denied() && !e.is_window_hung() {
                    log!("borderless toggle failed: {e}");
                }
                if let Some(notice) = restore_failure_notice(&e) {
                    tray.notify(&notice);
                }
                e
            }
        }
    } else {
        let outcome = placement::apply_zone(config_dir, fg_hwnd, Trigger::Hotkey);
        if let Some(notice) = placement::failure_notice(Trigger::Hotkey, &outcome) {
            tray.notify(&notice);
        }
        match outcome {
            Outcome::Failed(e) => e,
            // Placed, or declined/skipped and already reported.
            _ => return false,
        }
    };

    if error.is_access_denied() {
        log!("borderless toggle failed (access denied); requesting elevation");
        match win32::relaunch_elevated() {
            Ok(()) => return true,
            Err(relaunch_err) => {
                log!("failed to relaunch elevated: {relaunch_err}");
                tray.notify(ELEVATION_FAILED_NOTICE);
            }
        }
    }
    false
}

/// Shown when the elevated relaunch after an access-denied hotkey press fails
/// or its UAC prompt is declined. That press stayed quiet until now, so this is
/// the one notice for it.
const ELEVATION_FAILED_NOTICE: &str = "GameFrame needs administrator rights to change this window, and the elevation was cancelled or failed.";

/// The notice for a failed restore of the foreground window by the borderless
/// hotkey. An access-denied press is answered by the elevation request instead.
fn restore_failure_notice(error: &win32::Win32Error) -> Option<String> {
    if error.is_access_denied() {
        None
    } else if error.is_window_hung() {
        Some("This window isn't responding, so its border couldn't be restored. Try again once it responds.".to_string())
    } else {
        Some(format!("Couldn't restore this window: {error}"))
    }
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
/// again. An elevated instance never installs, so it only tells the user how
/// to update and asks nothing. Blocks on the question or the box, so call it
/// from a worker thread.
fn offer_install(info: crate::updater::UpdateInfo) {
    // An elevated instance never installs, so don't ask a question whose "yes"
    // can only fail.
    if crate::win32::is_process_elevated() {
        show_message_box(
            &format!(
                "{}\n\n{}",
                CheckOutcome::UpdateAvailable(info).message(),
                crate::updater::ELEVATED_UPDATE_ADVICE
            ),
            MB_OK | MB_ICONINFORMATION,
        );
        return;
    }
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

    if let Err(e) = taskbar::toggle_taskbar_hide(mode)
        && !e.is_window_hung()
    {
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
    let enabled = match profile_store::update(config_dir, |config| {
        config.cursor.enabled = !config.cursor.enabled;
        config.cursor.enabled
    }) {
        Ok(enabled) => enabled,
        Err(e) => {
            log!("game cursor toggle: failed to update config: {e}");
            return None;
        }
    };
    cursor_overlay.reload(config_dir);
    if enabled {
        cursor_overlay.pulse();
    }
    Some(enabled)
}

/// Opens Settings (what the tray item and a second launch both do): starts the
/// Settings child process, or brings the open one to the front. Returns at
/// once; [`settings_host::service_settings`] follows the child from here.
fn show_settings(ctx: &Shared, state: &mut LoopState) {
    if state.settings.is_some() {
        instance_channel::focus_settings_window();
        return;
    }
    // The outcome of the last registration is handed to Settings instead of
    // re-checking: while the window is open this instance's own registrations
    // hold the combos, so a fresh probe would report GameFrame's own bindings
    // as "in use". Settings hides a failure once its binding has been changed.
    match SettingsProcess::spawn(
        ChildKind::Settings,
        state.hotkey_reports.clone(),
        crate::cursor_overlay::last_game_cursor(),
    ) {
        Ok(process) => state.settings = Some(process),
        Err(e) => {
            log!("failed to open Settings: {e}");
            ctx.tray
                .notify("GameFrame couldn't open Settings. See the log for details.");
        }
    }
}

/// Applies what Settings may have changed, once its window has closed.
fn settings_closed(ctx: &Shared, state: &mut LoopState) {
    let Shared {
        tray,
        config_dir,
        cursor_overlay,
        ..
    } = *ctx;
    crate::watchdog::step("settings: closed, re-registering hotkeys");
    // The child wrote the config file (Save) behind this process's cache.
    profile_store::forget_cached(config_dir);

    // Settings may have changed the hotkey bindings on disk (Save persists via
    // `profile_store`) -- re-register from the freshly saved config so a newly
    // bound/changed/removed hotkey takes effect immediately, without requiring
    // a full app restart. The old `Hotkeys` must be dropped *before*
    // registering again (its `Drop` destroys the manager's hidden window,
    // which releases its hotkeys): a plain `hotkeys = register_hotkeys(..)`
    // evaluates the right side first, so every unchanged key failed with
    // "already registered" and all hotkeys went dead after any Settings visit.
    drop(state.hotkeys.take());
    let previous_reports = std::mem::take(&mut state.hotkey_reports);
    (state.hotkeys, state.hotkey_reports) = register_hotkeys(config_dir);
    // Right after Save, not only on reopening Settings: one notice for the
    // bindings that just started failing (not for ones already reported).
    if let Some(notice) =
        crate::hotkeys::new_failure_notice(&previous_reports, &state.hotkey_reports)
    {
        tray.notify(&notice);
    }
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
    use global_hotkey::hotkey::HotKey;
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
    fn a_failed_restore_is_explained_unless_elevation_answers_it() {
        use crate::win32::Win32Error;
        let denied = Win32Error::Os(std::io::Error::from_raw_os_error(5));

        assert_eq!(restore_failure_notice(&denied), None);
        assert!(
            restore_failure_notice(&Win32Error::WindowHung)
                .unwrap()
                .contains("isn't responding")
        );
        assert!(
            restore_failure_notice(&Win32Error::WindowMinimized)
                .unwrap()
                .starts_with("Couldn't restore")
        );
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
