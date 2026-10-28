//! The one "apply the Zone to a window" pipeline.
//!
//! Five triggers place a window into the Zone: the borderless hotkey (strip
//! direction), the tray's "Apply to Window", launch detection (auto-apply on a
//! foreground change), the drift re-apply, and Settings' live re-apply after a
//! Zone change. They all go through [`apply_zone`], which does, in order:
//! the restore opt-out check (auto-apply and drift only), the tracked-game
//! decision, window eligibility, region resolution, strip+place (the hung and
//! already-in-place decisions live in [`win32::apply_borderless_to_window`]),
//! remembering the window for drift correction, and the taskbar hide per
//! config. Config and region errors are logged on every path.
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
use crate::win32::{self, Win32Error};
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
    /// A foreground-window change ([`crate::launch_watcher`]).
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

    /// Whether the game's `auto_apply` flag gates the placement (and a
    /// hotkey-restored window is left alone).
    fn requires_auto_apply(self) -> bool {
        matches!(self, Trigger::AutoApply | Trigger::Drift)
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

    /// Whether an untracked window / missing Zone is worth a log line: the
    /// automatic triggers fire for every foreground change, so they stay quiet.
    fn logs_not_applying(self) -> bool {
        !self.requires_auto_apply()
    }
}

/// How an [`apply_zone`] call ended.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// The window was placed (or already in place).
    Applied,
    /// Nothing was attempted: not tracked, no Zone, restored by the user,
    /// refused by the eligibility filter, or the config could not be read.
    /// Already logged where it is worth logging.
    Skipped,
    /// The Zone does not resolve against the monitor.
    RegionFailed,
    /// Placing the window failed (a hung window included).
    Failed(Win32Error),
}

/// The OS and config calls [`apply_zone`] makes.
pub(crate) trait PlacementOps {
    fn load_config(&self) -> Result<Config, String>;
    fn save_config(&self, config: &Config) -> Result<(), String>;
    /// The window's process exe path, or `None` when it cannot be read.
    fn exe_path(&self, hwnd: HWND) -> Option<PathBuf>;
    /// The eligibility filter; `Ok` carries the exe path, `Err` the refusal.
    fn eligibility(&self, hwnd: HWND) -> Result<PathBuf, String>;
    fn window_text(&self, hwnd: HWND) -> String;
    /// The user restored this window with the hotkey: automatic triggers leave
    /// it alone.
    fn auto_apply_suppressed(&self, hwnd: HWND) -> bool;
    fn resolve_region(&self, zone: &Zone) -> Result<Region, String>;
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

/// Places `hwnd` into the configured Zone on behalf of `trigger` against the
/// real OS. See the module docs.
pub(crate) fn apply_zone(config_dir: &Path, hwnd: HWND, trigger: Trigger) -> Outcome {
    apply_zone_with(&SystemOps { config_dir }, hwnd, trigger)
}

/// [`apply_zone`] over any [`PlacementOps`].
pub(crate) fn apply_zone_with(ops: &impl PlacementOps, hwnd: HWND, trigger: Trigger) -> Outcome {
    let label = trigger.label();
    if trigger.requires_auto_apply() && ops.auto_apply_suppressed(hwnd) {
        return Outcome::Skipped;
    }
    let mut config = match ops.load_config() {
        Ok(config) => config,
        Err(e) => {
            log!("{label}: failed to load config: {e}");
            return Outcome::Skipped;
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
                return Outcome::Skipped;
            }
        };
        let display_name = ops.window_text(hwnd);
        let index =
            profile_store::upsert_tracked_game(&mut config.tracked_games, exe_path, display_name);
        let game = config.tracked_games[index].clone();
        if let Err(e) = ops.save_config(&config) {
            log!(
                "{label}: failed to save tracked game {}: {e}",
                game.display_name
            );
        }
        let Some(zone) = config.zone else {
            log!(
                "{label}: no Zone is set; {} is tracked but nothing was applied",
                game.display_name
            );
            return Outcome::Skipped;
        };
        (game, zone, true)
    } else {
        let Some(exe_path) = ops.exe_path(hwnd) else {
            return Outcome::Skipped;
        };
        let resolved = if trigger.requires_auto_apply() {
            resolve_auto_apply(&config, &exe_path)
        } else {
            resolve_tracked_apply(&config, &exe_path)
        };
        let Some((game, zone)) = resolved else {
            let tracked = profile_store::find_tracked_game(&config.tracked_games, &exe_path);
            if trigger.logs_not_applying() {
                log!("{label}: not applying ({})", no_apply_reason(&config));
            } else if config.zone.is_none() && tracked.is_some_and(|g| g.auto_apply) {
                // A tracked auto-apply game while no Zone is set; any other
                // miss is the common case of an untracked foreground window.
                log!(
                    "{label}: not applying to a tracked game ({})",
                    no_apply_reason(&config)
                );
            }
            return Outcome::Skipped;
        };
        (game, zone, false)
    };

    if !eligibility_checked && let Err(refusal) = ops.eligibility(hwnd) {
        log!(
            "{label}: refusing the window of {}: {refusal}",
            game.display_name
        );
        return Outcome::Skipped;
    }

    let region = match ops.resolve_region(&zone) {
        Ok(region) => region,
        Err(e) => {
            log!(
                "{label}: failed to resolve the Zone for {}: {e}",
                game.display_name
            );
            return Outcome::RegionFailed;
        }
    };

    if let Err(e) = ops.place(hwnd, region) {
        // A hung window was already logged once where it was found.
        if !e.is_window_hung() {
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
pub(crate) fn handle_auto_apply_launch(config_dir: &Path, hwnd: HWND) {
    apply_zone(config_dir, hwnd, Trigger::AutoApply);
}

/// A location-change event on a window GameFrame placed: re-applies the Zone
/// if the game moved, resized, or re-added its border itself. A no-op when the
/// window still matches the target, which also stops GameFrame's own
/// `SetWindowPos` from re-triggering itself.
///
/// Minimized windows are skipped and re-applies are rate-limited per window.
/// Returns how long to wait before re-checking when the event was skipped only
/// because of the rate limit (so a drift that lands inside the limit is still
/// corrected) or because the window is hung (so it is retried once it responds
/// again).
///
/// OS-facing; the decisions are [`win32::window_drifted`] and
/// `win32::decide_drift_for_window`, unit tested.
pub(crate) fn handle_applied_window_drift(config_dir: &Path, hwnd: HWND) -> Option<Duration> {
    // A minimized window's rect is its off-screen minimized position, not drift.
    if win32::auto_apply_suppressed(hwnd) || win32::is_window_minimized(hwnd) {
        return None;
    }
    let exe_path = window_filter::exe_path_for_window(hwnd)?;
    let config = match profile_store::load(config_dir) {
        Ok(config) => config,
        Err(e) => {
            log!("drift re-apply: failed to load config: {e}");
            return None;
        }
    };
    let (game, zone) = resolve_auto_apply(&config, &exe_path)?;
    let target_region = match resolve_region(&zone, &win32::primary_monitor_geometry()) {
        Ok(region) => region,
        Err(e) => {
            log!(
                "drift re-apply: failed to resolve the Zone for {}: {e}",
                game.display_name
            );
            return None;
        }
    };
    let drifted = matches!(win32::window_has_drifted(hwnd, target_region), Ok(true));
    match win32::decide_drift_for_window(hwnd, drifted) {
        win32::DriftDecision::Reapply => {}
        win32::DriftDecision::SkipRateLimited { retry_in } => return Some(retry_in),
        win32::DriftDecision::SkipCapped => {
            // Logged when the cap first stops a re-apply, not before.
            if win32::note_drift_capped(hwnd) {
                log!(
                    "drift re-apply: {} keeps resizing itself; leaving it alone",
                    game.display_name
                );
            }
            return None;
        }
        // No re-check timer for either: re-checking is what fights a game.
        win32::DriftDecision::SkipNotDrifted | win32::DriftDecision::SkipFullscreen => {
            return None;
        }
    }
    // Recorded before the eligibility check so a refused window is rate
    // limited (and logged) like any other attempt.
    win32::record_drift_attempt(hwnd);
    match apply_zone(config_dir, hwnd, Trigger::Drift) {
        // A hung window produces no location-change events, so poll it until
        // it responds.
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

    fn save_config(&self, config: &Config) -> Result<(), String> {
        profile_store::save(self.config_dir, config).map_err(|e| e.to_string())
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

    fn resolve_region(&self, zone: &Zone) -> Result<Region, String> {
        resolve_region(zone, &win32::primary_monitor_geometry()).map_err(|e| e.to_string())
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
mod tests {
    use super::*;
    use std::cell::RefCell;

    const A_ZONE: Zone = Zone {
        x: 10,
        y: 20,
        width: 300,
        height: 200,
    };
    const THE_REGION: Region = Region {
        x: 10,
        y: 20,
        width: 300,
        height: 200,
    };

    fn the_exe() -> PathBuf {
        PathBuf::from("C:\\Games\\thing.exe")
    }

    fn hwnd() -> HWND {
        0x1234usize as HWND
    }

    fn config_with_zone_and_game(zone: Option<Zone>, auto_apply: bool) -> Config {
        let mut config = Config {
            zone,
            ..Config::default()
        };
        let mut game = TrackedGame::new_default(the_exe(), "Thing".to_string());
        game.auto_apply = auto_apply;
        config.tracked_games.push(game);
        config
    }

    /// A fake OS: records every call in order and answers from its fields.
    struct FakeOps {
        config: RefCell<Config>,
        suppressed: bool,
        eligible: bool,
        region_ok: bool,
        place_result: fn() -> Result<(), Win32Error>,
        taskbar_result: fn() -> Result<(), Win32Error>,
        calls: RefCell<Vec<String>>,
    }

    impl FakeOps {
        fn new(config: Config) -> Self {
            Self {
                config: RefCell::new(config),
                suppressed: false,
                eligible: true,
                region_ok: true,
                place_result: || Ok(()),
                taskbar_result: || Ok(()),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        fn note(&self, call: &str) {
            self.calls.borrow_mut().push(call.to_string());
        }
    }

    impl PlacementOps for FakeOps {
        fn load_config(&self) -> Result<Config, String> {
            Ok(self.config.borrow().clone())
        }
        fn save_config(&self, config: &Config) -> Result<(), String> {
            self.note("save");
            *self.config.borrow_mut() = config.clone();
            Ok(())
        }
        fn exe_path(&self, _: HWND) -> Option<PathBuf> {
            Some(the_exe())
        }
        fn eligibility(&self, _: HWND) -> Result<PathBuf, String> {
            self.note("eligibility");
            if self.eligible {
                Ok(the_exe())
            } else {
                Err("not a normal window".to_string())
            }
        }
        fn window_text(&self, _: HWND) -> String {
            "Thing Window".to_string()
        }
        fn auto_apply_suppressed(&self, _: HWND) -> bool {
            self.suppressed
        }
        fn resolve_region(&self, zone: &Zone) -> Result<Region, String> {
            assert_eq!(*zone, A_ZONE);
            if self.region_ok {
                Ok(THE_REGION)
            } else {
                Err("zone off screen".to_string())
            }
        }
        fn place(&self, _: HWND, region: Region) -> Result<(), Win32Error> {
            assert_eq!(region, THE_REGION);
            self.note("place");
            (self.place_result)()
        }
        fn remember_applied(&self, _: HWND) {
            self.note("remember");
        }
        fn reset_drift_cap(&self, _: HWND) {
            self.note("reset_cap");
        }
        fn allow_auto_apply(&self, _: HWND) {
            self.note("allow_auto_apply");
        }
        fn set_taskbar(&self, _: HWND, hide: bool, _: TaskbarHideMode) -> Result<(), Win32Error> {
            self.note(if hide { "taskbar_hide" } else { "taskbar_show" });
            (self.taskbar_result)()
        }
    }

    fn tracked_ops(auto_apply: bool) -> FakeOps {
        FakeOps::new(config_with_zone_and_game(Some(A_ZONE), auto_apply))
    }

    #[test]
    fn success_places_remembers_and_sets_the_taskbar() {
        let ops = tracked_ops(true);
        ops.config.borrow_mut().hide_taskbar_on_apply = true;

        let outcome = apply_zone_with(&ops, hwnd(), Trigger::AutoApply);

        assert!(matches!(outcome, Outcome::Applied));
        assert_eq!(
            ops.calls(),
            ["eligibility", "place", "remember", "taskbar_hide"]
        );
    }

    #[test]
    fn taskbar_is_set_from_the_config_flag() {
        let ops = tracked_ops(true);

        apply_zone_with(&ops, hwnd(), Trigger::AutoApply);

        assert_eq!(ops.calls().last().unwrap(), "taskbar_show");
    }

    #[test]
    fn a_hung_window_is_not_remembered_and_leaves_the_taskbar_alone() {
        let mut ops = tracked_ops(true);
        ops.place_result = || Err(Win32Error::WindowHung);

        let outcome = apply_zone_with(&ops, hwnd(), Trigger::AutoApply);

        assert!(matches!(outcome, Outcome::Failed(ref e) if e.is_window_hung()));
        assert_eq!(ops.calls(), ["eligibility", "place"]);
    }

    #[test]
    fn a_window_already_in_place_still_re_arms_the_taskbar() {
        // `place` reports an in-place window as success: every foreground
        // switch back to the game re-arms the taskbar hide.
        let ops = tracked_ops(true);

        let outcome = apply_zone_with(&ops, hwnd(), Trigger::AutoApply);

        assert!(matches!(outcome, Outcome::Applied));
        assert!(ops.calls().contains(&"taskbar_show".to_string()));
    }

    #[test]
    fn an_ineligible_window_is_skipped_before_anything_is_placed() {
        let mut ops = tracked_ops(true);
        ops.eligible = false;

        let outcome = apply_zone_with(&ops, hwnd(), Trigger::Hotkey);

        assert!(matches!(outcome, Outcome::Skipped));
        assert_eq!(ops.calls(), ["eligibility"]);
    }

    #[test]
    fn a_failed_taskbar_set_does_not_fail_the_placement() {
        let mut ops = tracked_ops(true);
        ops.taskbar_result = || Err(Win32Error::TaskbarNotFound);

        let outcome = apply_zone_with(&ops, hwnd(), Trigger::Hotkey);

        assert!(matches!(outcome, Outcome::Applied));
    }

    #[test]
    fn a_region_error_is_reported_and_nothing_is_placed() {
        let mut ops = tracked_ops(true);
        ops.region_ok = false;

        let outcome = apply_zone_with(&ops, hwnd(), Trigger::LiveReapply);

        assert!(matches!(outcome, Outcome::RegionFailed));
        assert!(!ops.calls().contains(&"place".to_string()));
    }

    #[test]
    fn hotkey_and_live_reapply_are_remembered_for_drift_correction() {
        for trigger in [Trigger::Hotkey, Trigger::LiveReapply, Trigger::Tray] {
            let ops = tracked_ops(false);

            apply_zone_with(&ops, hwnd(), trigger);

            assert!(
                ops.calls().contains(&"remember".to_string()),
                "{trigger:?} was not remembered"
            );
        }
    }

    #[test]
    fn explicit_triggers_lift_the_restore_opt_out_and_the_drift_cap() {
        for trigger in [Trigger::Hotkey, Trigger::Tray, Trigger::LiveReapply] {
            let ops = tracked_ops(false);

            apply_zone_with(&ops, hwnd(), trigger);

            let calls = ops.calls();
            assert!(calls.contains(&"reset_cap".to_string()), "{trigger:?}");
            assert!(
                calls.contains(&"allow_auto_apply".to_string()),
                "{trigger:?}"
            );
        }
    }

    #[test]
    fn automatic_triggers_do_not_lift_the_opt_out_or_the_cap() {
        let ops = tracked_ops(true);

        apply_zone_with(&ops, hwnd(), Trigger::AutoApply);

        let calls = ops.calls();
        assert!(!calls.contains(&"reset_cap".to_string()));
        assert!(!calls.contains(&"allow_auto_apply".to_string()));
    }

    #[test]
    fn drift_re_apply_only_places_the_window() {
        let ops = tracked_ops(true);

        let outcome = apply_zone_with(&ops, hwnd(), Trigger::Drift);

        assert!(matches!(outcome, Outcome::Applied));
        assert_eq!(ops.calls(), ["eligibility", "place"]);
    }

    #[test]
    fn automatic_triggers_skip_a_game_without_auto_apply() {
        for trigger in [Trigger::AutoApply, Trigger::Drift] {
            let ops = tracked_ops(false);

            assert!(matches!(
                apply_zone_with(&ops, hwnd(), trigger),
                Outcome::Skipped
            ));
            assert!(ops.calls().is_empty());
        }
    }

    #[test]
    fn manual_triggers_apply_to_a_game_without_auto_apply() {
        for trigger in [Trigger::Hotkey, Trigger::LiveReapply] {
            let ops = tracked_ops(false);

            assert!(matches!(
                apply_zone_with(&ops, hwnd(), trigger),
                Outcome::Applied
            ));
        }
    }

    #[test]
    fn automatic_triggers_leave_a_hotkey_restored_window_alone() {
        for trigger in [Trigger::AutoApply, Trigger::Drift] {
            let mut ops = tracked_ops(true);
            ops.suppressed = true;

            assert!(matches!(
                apply_zone_with(&ops, hwnd(), trigger),
                Outcome::Skipped
            ));
            assert!(ops.calls().is_empty());
        }
    }

    #[test]
    fn an_untracked_window_is_skipped_by_every_non_tray_trigger() {
        for trigger in [
            Trigger::Hotkey,
            Trigger::AutoApply,
            Trigger::Drift,
            Trigger::LiveReapply,
        ] {
            let ops = FakeOps::new(Config {
                zone: Some(A_ZONE),
                ..Config::default()
            });

            assert!(matches!(
                apply_zone_with(&ops, hwnd(), trigger),
                Outcome::Skipped
            ));
            assert!(ops.calls().is_empty());
        }
    }

    #[test]
    fn no_zone_skips_the_placement() {
        let ops = FakeOps::new(config_with_zone_and_game(None, true));

        assert!(matches!(
            apply_zone_with(&ops, hwnd(), Trigger::Hotkey),
            Outcome::Skipped
        ));
        assert!(ops.calls().is_empty());
    }

    #[test]
    fn tray_apply_tracks_a_new_game_with_auto_apply_and_saves_before_placing() {
        let ops = FakeOps::new(Config {
            zone: Some(A_ZONE),
            ..Config::default()
        });

        let outcome = apply_zone_with(&ops, hwnd(), Trigger::Tray);

        assert!(matches!(outcome, Outcome::Applied));
        let calls = ops.calls();
        let save = calls.iter().position(|c| c == "save").unwrap();
        let place = calls.iter().position(|c| c == "place").unwrap();
        assert!(save < place);
        let saved = ops.config.borrow();
        assert_eq!(saved.tracked_games.len(), 1);
        assert_eq!(saved.tracked_games[0].display_name, "Thing Window");
        assert!(saved.tracked_games[0].auto_apply);
    }

    #[test]
    fn tray_apply_keeps_the_tracking_when_no_zone_is_set() {
        let ops = FakeOps::new(Config::default());

        let outcome = apply_zone_with(&ops, hwnd(), Trigger::Tray);

        assert!(matches!(outcome, Outcome::Skipped));
        assert_eq!(ops.calls(), ["eligibility", "save"]);
        assert_eq!(ops.config.borrow().tracked_games.len(), 1);
    }

    #[test]
    fn tray_apply_does_not_track_an_ineligible_window() {
        let mut ops = FakeOps::new(Config {
            zone: Some(A_ZONE),
            ..Config::default()
        });
        ops.eligible = false;

        let outcome = apply_zone_with(&ops, hwnd(), Trigger::Tray);

        assert!(matches!(outcome, Outcome::Skipped));
        assert!(ops.config.borrow().tracked_games.is_empty());
    }

    #[test]
    fn tray_apply_ignores_the_auto_apply_flag_and_the_restore_opt_out() {
        let mut ops = tracked_ops(false);
        ops.suppressed = true;

        assert!(matches!(
            apply_zone_with(&ops, hwnd(), Trigger::Tray),
            Outcome::Applied
        ));
    }

    // The pure resolvers (moved here from `app` with the apply logic).

    #[test]
    fn resolve_tracked_apply_returns_none_for_an_untracked_executable_path() {
        let config = Config {
            zone: Some(A_ZONE),
            ..Config::default()
        };

        assert!(resolve_tracked_apply(&config, &the_exe()).is_none());
    }

    #[test]
    fn resolve_tracked_apply_returns_none_when_no_zone_is_set() {
        let config = config_with_zone_and_game(None, true);

        assert!(resolve_tracked_apply(&config, &the_exe()).is_none());
    }

    #[test]
    fn resolve_tracked_apply_returns_the_game_and_the_zone() {
        let config = config_with_zone_and_game(Some(A_ZONE), true);

        let (game, zone) = resolve_tracked_apply(&config, &the_exe()).unwrap();

        assert_eq!(game.display_name, "Thing");
        assert_eq!(zone, A_ZONE);
    }

    #[test]
    fn resolve_auto_apply_requires_the_auto_apply_flag() {
        let on = config_with_zone_and_game(Some(A_ZONE), true);
        let off = config_with_zone_and_game(Some(A_ZONE), false);

        assert!(resolve_auto_apply(&on, &the_exe()).is_some());
        assert!(resolve_auto_apply(&off, &the_exe()).is_none());
    }

    #[test]
    fn resolve_auto_apply_returns_none_for_untracked_or_without_a_zone() {
        let untracked = Config {
            zone: Some(A_ZONE),
            ..Config::default()
        };
        let no_zone = config_with_zone_and_game(None, true);

        assert!(resolve_auto_apply(&untracked, &the_exe()).is_none());
        assert!(resolve_auto_apply(&no_zone, &the_exe()).is_none());
    }

    #[test]
    fn no_apply_reason_distinguishes_no_zone_from_untracked() {
        let with_zone = Config {
            zone: Some(A_ZONE),
            ..Config::default()
        };

        assert_eq!(no_apply_reason(&Config::default()), "no Zone is set");
        assert_eq!(
            no_apply_reason(&with_zone),
            "the process is not a tracked game"
        );
    }
}
