//! The one "apply the Zone to a window" pipeline.
//!
//! Five triggers place a window into the Zone: the borderless hotkey (strip
//! direction), the tray's "Apply to Window", launch detection (auto-apply on a
//! foreground change), the drift re-apply, and Settings' live re-apply after a
//! Zone change. They all go through [`apply_zone`], which does, in order:
//! the restore opt-out check (auto-apply and drift only), the tracked-game
//! decision, window eligibility, region resolution, the drift gate (drift
//! only), strip+place (the hung and already-in-place decisions live in
//! [`win32::apply_borderless_to_window`]), remembering the window for drift
//! correction, and the taskbar hide per config. Config and region errors are
//! logged on every path (on the drift path once per window, since it runs on
//! every location-change event).
//!
//! The per-trigger differences are all in [`Trigger`]: whether the game must
//! have `auto_apply` on, whether the placement is an explicit user request
//! (lifts a hotkey-restore opt-out and the drift re-apply cap), and whether
//! the taskbar is touched (not for a drift re-apply, which only corrects
//! the position).
//!
//! The OS calls sit behind [`PlacementOps`] so the orchestration is unit tested
//! against a fake; [`SystemOps`] is the real implementation.

use crate::profile_store::{self, Config, TaskbarHideMode, TrackedGame, Zone};
use crate::region_calc::{Region, resolve_region};
use crate::taskbar;
use crate::win32::{self, DriftDecision, Win32Error};
use crate::window_filter;
use std::path::{Path, PathBuf};
use std::time::Duration;
use windows_sys::Win32::Foundation::HWND;

/// What asked for the placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trigger {
    /// The borderless hotkey, strip direction.
    Hotkey,
    /// The tray's "Apply to Window": also tracks the window's game.
    Tray,
    /// A foreground-window change (launch detection).
    AutoApply,
    /// The game moved or restyled itself after it was placed.
    Drift,
    /// Settings saved a new Zone while the game was running.
    LiveReapply,
}

impl Trigger {
    /// Prefix of this trigger's log lines.
    fn label(self) -> &'static str {
        match self {
            Trigger::Hotkey => "borderless toggle",
            Trigger::Tray => "apply to window",
            Trigger::AutoApply => "launch detection",
            Trigger::Drift => "drift re-apply",
            Trigger::LiveReapply => "live re-apply",
        }
    }

    /// Fires without the user asking, at high frequency: stays quiet about an
    /// untracked window or a missing Zone.
    fn is_automatic(self) -> bool {
        matches!(self, Trigger::AutoApply | Trigger::Drift)
    }

    /// Whether the game's `auto_apply` flag gates the placement. It only
    /// governs launch/focus auto-apply: drift corrects every window GameFrame
    /// placed that is still a tracked game with a Zone.
    fn requires_auto_apply(self) -> bool {
        self == Trigger::AutoApply
    }

    /// An explicit user request: it overrides an earlier hotkey restore and
    /// lifts the drift re-apply cap.
    fn is_explicit(self) -> bool {
        matches!(self, Trigger::Hotkey | Trigger::Tray | Trigger::LiveReapply)
    }

    /// Whether this trigger remembers the window for drift correction (a drift
    /// re-apply works on a window that is already remembered).
    fn remembers_window(self) -> bool {
        self != Trigger::Drift
    }

    /// Whether this trigger sets the taskbar.
    fn sets_taskbar(self) -> bool {
        self != Trigger::Drift
    }
}

/// How an [`apply_zone`] call ended.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// The window was placed (or already in place).
    Applied,
    /// Nothing was attempted by an automatic trigger: not tracked, no Zone,
    /// restored by the user, not drifted, or the config could not be read.
    /// Already logged where it is worth logging. (A user-triggered apply that
    /// attempts nothing reports why, as [`Outcome::Declined`].)
    Skipped,
    /// A user-triggered apply attempted nothing, for this reason.
    Declined(Declined),
    /// The eligibility filter refused the window (logged).
    Refused,
    /// The Zone does not resolve against the monitor.
    RegionFailed,
    /// Placing the window failed (a hung window included).
    Failed(Win32Error),
    /// A drift event arrived inside the re-apply rate limit: check again after
    /// this long, so a drift that lands inside the limit is still corrected.
    RetryIn(Duration),
}

/// Why a user-triggered [`apply_zone`] attempted nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Declined {
    /// The config could not be read.
    ConfigUnreadable,
    /// The window's process could not be read.
    ProcessUnreadable,
    /// The window's program is not a tracked game.
    NotTracked,
    /// No Zone is set.
    NoZone,
}

/// The [`Outcome`] for an apply that attempted nothing: automatic triggers stay
/// quiet ([`Outcome::Skipped`]), the others say why.
fn declined(trigger: Trigger, reason: Declined) -> Outcome {
    if trigger.is_automatic() {
        Outcome::Skipped
    } else {
        Outcome::Declined(reason)
    }
}

/// The plain-language notice for a failed `outcome` of a `trigger` the user
/// pressed or clicked ([`Trigger::Tray`], [`Trigger::Hotkey`]), or `None` when
/// there is nothing to tell: success, an automatic skip, or an access-denied
/// hotkey press (answered by the elevation request instead).
pub(crate) fn failure_notice(trigger: Trigger, outcome: &Outcome) -> Option<String> {
    let text = match outcome {
        Outcome::Applied | Outcome::Skipped | Outcome::RetryIn(_) => return None,
        Outcome::Declined(Declined::NoZone) => {
            "No Zone is set, so nothing was applied. Draw one in Settings first."
        }
        Outcome::Declined(Declined::NotTracked) if trigger == Trigger::Hotkey => {
            "This window's program isn't a tracked game. Add it in Settings, or pick the window under Apply to Window in the tray menu."
        }
        Outcome::Declined(Declined::NotTracked) => {
            "This window's program isn't a tracked game. Add it in Settings."
        }
        Outcome::Declined(Declined::ProcessUnreadable) => {
            "GameFrame couldn't tell which program owns this window."
        }
        Outcome::Declined(Declined::ConfigUnreadable) => {
            "GameFrame couldn't read its settings file, so nothing was applied. See the log."
        }
        Outcome::Refused => {
            "GameFrame won't move this window (it isn't an ordinary application window). See the log."
        }
        Outcome::RegionFailed => {
            "The Zone doesn't fit the monitor this window is on. Draw it again in Settings."
        }
        Outcome::Failed(e) if e.is_window_hung() => {
            "This window isn't responding, so it couldn't be moved. Try again once it responds."
        }
        Outcome::Failed(e) if trigger == Trigger::Hotkey && e.is_access_denied() => return None,
        Outcome::Failed(e) if e.is_access_denied() => {
            "Windows denied access to this window. Run GameFrame as administrator to move windows of elevated programs."
        }
        Outcome::Failed(e) => return Some(format!("Couldn't move this window: {e}")),
    };
    Some(text.to_string())
}

/// The OS and config calls [`apply_zone`] makes.
pub(crate) trait PlacementOps {
    fn load_config(&self) -> Result<Config, String>;
    /// Persists the Tray pick: adds the game to the tracked list (a no-op for
    /// one already there) as one read-modify-write of the stored config, and
    /// returns the stored entry.
    fn track_game(&self, exe_path: &Path, display_name: &str) -> Result<TrackedGame, String>;
    /// The window's process exe path, or `None` when it cannot be read.
    fn exe_path(&self, hwnd: HWND) -> Option<PathBuf>;
    /// The eligibility filter; `Ok` carries the exe path, `Err` the refusal.
    fn eligibility(&self, hwnd: HWND) -> Result<PathBuf, String>;
    fn window_text(&self, hwnd: HWND) -> String;
    /// The user restored this window with the hotkey: automatic triggers leave
    /// it alone.
    fn auto_apply_suppressed(&self, hwnd: HWND) -> bool;
    /// A minimized window's rect is its off-screen minimized position, which
    /// is not drift.
    fn is_minimized(&self, hwnd: HWND) -> bool;
    fn resolve_region(&self, zone: &Zone) -> Result<Region, String>;
    /// Whether a drift event on `hwnd` should re-apply `target`: compares the
    /// window with the target and applies the rate limit, the cap and the
    /// fullscreen skip.
    fn drift_decision(&self, hwnd: HWND, target: Region) -> DriftDecision;
    fn record_drift_attempt(&self, hwnd: HWND);
    /// True the first time a capped window's event is skipped.
    fn note_drift_capped(&self, hwnd: HWND) -> bool;
    /// True the first time a config/Zone failure is reported for `hwnd`'s
    /// drift path.
    fn note_drift_failure(&self, hwnd: HWND) -> bool;
    fn clear_drift_failure(&self, hwnd: HWND);
    /// Strip the border and place the window; owns the hung and
    /// already-in-place decisions.
    fn place(&self, hwnd: HWND, region: Region) -> Result<(), Win32Error>;
    fn remember_applied(&self, hwnd: HWND);
    fn reset_drift_cap(&self, hwnd: HWND);
    fn allow_auto_apply(&self, hwnd: HWND);
    fn set_taskbar(&self, hwnd: HWND, hide: bool, mode: TaskbarHideMode) -> Result<(), Win32Error>;
}

/// The tracked game for `exe_path` and the configured Zone -- `None` for an
/// untracked process or when no Zone is set.
pub(crate) fn resolve_tracked_apply(
    config: &Config,
    exe_path: &Path,
) -> Option<(TrackedGame, Zone)> {
    let game = profile_store::find_tracked_game(&config.tracked_games, exe_path)?.clone();
    let zone = config.zone?;
    Some((game, zone))
}

/// Why [`resolve_tracked_apply`] returned `None`, for logging: with no Zone
/// that is reported even for an untracked process.
fn no_apply_reason(config: &Config) -> &'static str {
    if config.zone.is_none() {
        "no Zone is set"
    } else {
        "the process is not a tracked game"
    }
}

/// [`resolve_tracked_apply`] plus the game's `auto_apply` gate.
pub(crate) fn resolve_auto_apply(config: &Config, exe_path: &Path) -> Option<(TrackedGame, Zone)> {
    let (game, zone) = resolve_tracked_apply(config, exe_path)?;
    game.auto_apply.then_some((game, zone))
}

/// Whether a config/Zone failure of `trigger` is worth a log line now: the
/// drift path runs on every location-change event, so it reports once per
/// window until the condition clears.
fn should_log_failure(ops: &impl PlacementOps, hwnd: HWND, trigger: Trigger) -> bool {
    trigger != Trigger::Drift || ops.note_drift_failure(hwnd)
}

/// Places `hwnd` into the configured Zone on behalf of `trigger` against the
/// real OS. See the module docs.
pub(crate) fn apply_zone(config_dir: &Path, hwnd: HWND, trigger: Trigger) -> Outcome {
    apply_zone_with(&SystemOps { config_dir }, hwnd, trigger)
}

/// [`apply_zone`] over any [`PlacementOps`].
pub(crate) fn apply_zone_with(ops: &impl PlacementOps, hwnd: HWND, trigger: Trigger) -> Outcome {
    let label = trigger.label();
    let drift = trigger == Trigger::Drift;
    if trigger.is_automatic() && ops.auto_apply_suppressed(hwnd) {
        return Outcome::Skipped;
    }
    if drift && ops.is_minimized(hwnd) {
        return Outcome::Skipped;
    }
    let mut config = match ops.load_config() {
        Ok(config) => config,
        Err(e) => {
            if should_log_failure(ops, hwnd, trigger) {
                log!("{label}: failed to load config: {e}");
            }
            return declined(trigger, Declined::ConfigUnreadable);
        }
    };

    // The game to place and its Zone, and whether eligibility already ran.
    let (game, zone, eligibility_checked) = if trigger == Trigger::Tray {
        // Tracks any eligible window's game before anything else can fail, so
        // the tracking survives a later failure.
        let exe_path = match ops.eligibility(hwnd) {
            Ok(exe_path) => exe_path,
            Err(refusal) => {
                log!("{label}: refusing the window: {refusal}");
                return Outcome::Refused;
            }
        };
        let display_name = ops.window_text(hwnd);
        let game = match ops.track_game(&exe_path, &display_name) {
            Ok(game) => game,
            Err(e) => {
                // Placement still goes ahead with an in-memory entry.
                log!("{label}: failed to save tracked game {display_name}: {e}");
                let index = profile_store::upsert_tracked_game(
                    &mut config.tracked_games,
                    exe_path,
                    display_name,
                );
                config.tracked_games[index].clone()
            }
        };
        let Some(zone) = config.zone else {
            log!(
                "{label}: no Zone is set; {} is tracked but nothing was applied",
                game.display_name
            );
            return Outcome::Declined(Declined::NoZone);
        };
        (game, zone, true)
    } else {
        let Some(exe_path) = ops.exe_path(hwnd) else {
            if !trigger.is_automatic() {
                log!("{label}: not applying (the window's process cannot be read)");
            }
            return declined(trigger, Declined::ProcessUnreadable);
        };
        let resolved = if trigger.requires_auto_apply() {
            resolve_auto_apply(&config, &exe_path)
        } else {
            resolve_tracked_apply(&config, &exe_path)
        };
        let Some((game, zone)) = resolved else {
            if !trigger.is_automatic() {
                log!("{label}: not applying ({})", no_apply_reason(&config));
            } else if trigger == Trigger::AutoApply && config.zone.is_none() {
                // A tracked auto-apply game while no Zone is set; any other
                // miss is the common case of an untracked foreground window.
                let tracked = profile_store::find_tracked_game(&config.tracked_games, &exe_path);
                if tracked.is_some_and(|g| g.auto_apply) {
                    log!(
                        "{label}: not applying to a tracked game ({})",
                        no_apply_reason(&config)
                    );
                }
            }
            let reason = if config.zone.is_none() {
                Declined::NoZone
            } else {
                Declined::NotTracked
            };
            return declined(trigger, reason);
        };
        (game, zone, false)
    };

    // A drift re-apply checks eligibility after its gate, so a refused window
    // is rate limited like any other attempt.
    if !drift
        && !eligibility_checked
        && let Err(refusal) = ops.eligibility(hwnd)
    {
        log!(
            "{label}: refusing the window of {}: {refusal}",
            game.display_name
        );
        return Outcome::Refused;
    }

    let region = match ops.resolve_region(&zone) {
        Ok(region) => region,
        Err(e) => {
            if should_log_failure(ops, hwnd, trigger) {
                log!(
                    "{label}: failed to resolve the Zone for {}: {e}",
                    game.display_name
                );
            }
            return Outcome::RegionFailed;
        }
    };

    if drift {
        // Config and Zone resolve again: a later failure is a new condition.
        ops.clear_drift_failure(hwnd);
        match ops.drift_decision(hwnd, region) {
            DriftDecision::Reapply => ops.record_drift_attempt(hwnd),
            DriftDecision::SkipRateLimited { retry_in } => return Outcome::RetryIn(retry_in),
            DriftDecision::SkipCapped => {
                // Logged when the cap first stops a re-apply, not before.
                if ops.note_drift_capped(hwnd) {
                    log!(
                        "{label}: {} keeps resizing itself; leaving it alone",
                        game.display_name
                    );
                }
                return Outcome::Skipped;
            }
            // No re-check timer for either: re-checking is what fights a game.
            DriftDecision::SkipNotDrifted | DriftDecision::SkipFullscreen => {
                return Outcome::Skipped;
            }
        }
        if let Err(refusal) = ops.eligibility(hwnd) {
            log!(
                "{label}: refusing the window of {}: {refusal}",
                game.display_name
            );
            return Outcome::Refused;
        }
    }

    if let Err(e) = ops.place(hwnd, region) {
        // A hung window was already logged once where it was found; an access
        // denied hotkey press is reported by the caller's elevation request.
        let reported_elsewhere =
            e.is_window_hung() || (trigger == Trigger::Hotkey && e.is_access_denied());
        if !reported_elsewhere {
            log!("{label}: failed to apply to {}: {e}", game.display_name);
        }
        return Outcome::Failed(e);
    }

    if trigger.remembers_window() {
        ops.remember_applied(hwnd);
    }
    if trigger.is_explicit() {
        ops.reset_drift_cap(hwnd);
        ops.allow_auto_apply(hwnd);
    }
    if trigger.sets_taskbar()
        && let Err(e) =
            ops.set_taskbar(hwnd, config.hide_taskbar_on_apply, config.taskbar_hide_mode)
        && !e.is_window_hung()
    {
        log!(
            "{label}: failed to set the taskbar for {}: {e}",
            game.display_name
        );
    }
    Outcome::Applied
}

/// A launch-detected foreground change: places a tracked auto-apply game.
///
/// Returns how long to wait before trying again when the game was hung: auto
/// apply only runs on a foreground change, so without a retry a game that is
/// hung as it takes the foreground would never be placed (see [`retry_after`]).
pub(crate) fn handle_auto_apply_launch(config_dir: &Path, hwnd: HWND) -> Option<Duration> {
    retry_after(&apply_zone(config_dir, hwnd, Trigger::AutoApply))
}

/// A location-change event on a window GameFrame placed: re-applies the Zone
/// if the game moved, resized, or re-added its border itself, through
/// [`apply_zone`] with [`Trigger::Drift`]. A no-op when the window still
/// matches the target, which also stops GameFrame's own `SetWindowPos` from
/// re-triggering itself.
///
/// Returns how long to wait before re-checking when the event was skipped only
/// because of the rate limit or because the window is hung (see
/// [`drift_retry`]).
pub(crate) fn handle_applied_window_drift(config_dir: &Path, hwnd: HWND) -> Option<Duration> {
    retry_after(&apply_zone(config_dir, hwnd, Trigger::Drift))
}

/// When to try a window again after an automatic [`Outcome`]: after the rate
/// limit, or -- since a hung window produces no location-change events and
/// auto-apply only fires on a foreground change -- polling until it responds
/// again.
fn retry_after(outcome: &Outcome) -> Option<Duration> {
    match outcome {
        Outcome::RetryIn(retry_in) => Some(*retry_in),
        Outcome::Failed(e) if e.is_window_hung() => Some(win32::HUNG_RECHECK_INTERVAL),
        _ => None,
    }
}

/// The real OS and config.
struct SystemOps<'a> {
    config_dir: &'a Path,
}

impl PlacementOps for SystemOps<'_> {
    fn load_config(&self) -> Result<Config, String> {
        profile_store::load(self.config_dir).map_err(|e| e.to_string())
    }

    fn track_game(&self, exe_path: &Path, display_name: &str) -> Result<TrackedGame, String> {
        profile_store::update(self.config_dir, |config| {
            let index = profile_store::upsert_tracked_game(
                &mut config.tracked_games,
                exe_path.to_path_buf(),
                display_name.to_string(),
            );
            config.tracked_games[index].clone()
        })
        .map_err(|e| e.to_string())
    }

    fn exe_path(&self, hwnd: HWND) -> Option<PathBuf> {
        window_filter::exe_path_for_window(hwnd)
    }

    fn eligibility(&self, hwnd: HWND) -> Result<PathBuf, String> {
        window_filter::eligibility_for_hwnd(hwnd).map_err(|refusal| refusal.to_string())
    }

    fn window_text(&self, hwnd: HWND) -> String {
        win32::window_text(hwnd)
    }

    fn auto_apply_suppressed(&self, hwnd: HWND) -> bool {
        win32::auto_apply_suppressed(hwnd)
    }

    fn is_minimized(&self, hwnd: HWND) -> bool {
        win32::is_window_minimized(hwnd)
    }

    fn resolve_region(&self, zone: &Zone) -> Result<Region, String> {
        resolve_region(zone, &win32::primary_monitor_geometry()).map_err(|e| e.to_string())
    }

    fn drift_decision(&self, hwnd: HWND, target: Region) -> DriftDecision {
        let drifted = matches!(win32::window_has_drifted(hwnd, target), Ok(true));
        win32::decide_drift_for_window(hwnd, drifted)
    }

    fn record_drift_attempt(&self, hwnd: HWND) {
        win32::record_drift_attempt(hwnd);
    }

    fn note_drift_capped(&self, hwnd: HWND) -> bool {
        win32::note_drift_capped(hwnd)
    }

    fn note_drift_failure(&self, hwnd: HWND) -> bool {
        win32::note_drift_failure(hwnd)
    }

    fn clear_drift_failure(&self, hwnd: HWND) {
        win32::clear_drift_failure(hwnd);
    }

    fn place(&self, hwnd: HWND, region: Region) -> Result<(), Win32Error> {
        win32::apply_borderless_to_window(hwnd, region)
    }

    fn remember_applied(&self, hwnd: HWND) {
        win32::remember_applied_window(hwnd);
    }

    fn reset_drift_cap(&self, hwnd: HWND) {
        win32::reset_drift_cap(hwnd);
    }

    fn allow_auto_apply(&self, hwnd: HWND) {
        win32::allow_auto_apply(hwnd);
    }

    fn set_taskbar(&self, hwnd: HWND, hide: bool, mode: TaskbarHideMode) -> Result<(), Win32Error> {
        // Armed, so closing `hwnd` later restores the taskbar with no hotkey.
        taskbar::set_taskbar_hide_absolute_armed(hwnd, hide, mode)
    }
}

#[cfg(test)]
mod tests;
