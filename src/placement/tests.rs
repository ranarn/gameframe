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
    let mut game = TrackedGame::new_for_cursor_setting(the_exe(), "Thing".to_string(), true);
    game.auto_apply = auto_apply;
    config.tracked_games.push(game);
    config
}

/// A fake OS: records every call in order and answers from its fields.
struct FakeOps {
    config: RefCell<Config>,
    suppressed: bool,
    eligible: bool,
    config_ok: bool,
    /// Saving a newly tracked game succeeds.
    track_ok: bool,
    /// The window's exe path can be read.
    exe_known: bool,
    minimized: bool,
    region_ok: bool,
    drift: DriftDecision,
    /// Relabels the placement call as `in_place`. The fake still returns the
    /// success result: the real in-place decision lives in
    /// `win32::apply_borderless_to_window` (`decide_apply`), not here.
    in_place: bool,
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
            config_ok: true,
            track_ok: true,
            exe_known: true,
            minimized: false,
            region_ok: true,
            drift: DriftDecision::Reapply,
            in_place: false,
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
        if self.config_ok {
            Ok(self.config.borrow().clone())
        } else {
            Err("config is corrupt".to_string())
        }
    }
    fn track_game(&self, exe_path: &Path, display_name: &str) -> Result<TrackedGame, String> {
        self.note("track_game");
        if !self.track_ok {
            return Err("config is locked".to_string());
        }
        let mut config = self.config.borrow_mut();
        let config = &mut *config;
        let index = profile_store::upsert_tracked_game(
            &mut config.tracked_games,
            exe_path.to_path_buf(),
            display_name.to_string(),
            config.cursor.enabled,
        );
        Ok(config.tracked_games[index].clone())
    }
    fn exe_path(&self, _: HWND) -> Option<PathBuf> {
        self.exe_known.then(the_exe)
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
    fn is_minimized(&self, _: HWND) -> bool {
        self.minimized
    }
    fn resolve_region(&self, zone: &Zone) -> Result<Region, String> {
        assert_eq!(*zone, A_ZONE);
        if self.region_ok {
            Ok(THE_REGION)
        } else {
            Err("zone off screen".to_string())
        }
    }
    fn drift_decision(&self, _: HWND, target: Region) -> DriftDecision {
        assert_eq!(target, THE_REGION);
        self.note("drift_decision");
        self.drift
    }
    fn record_drift_attempt(&self, _: HWND) {
        self.note("record_attempt");
    }
    fn note_drift_capped(&self, _: HWND) -> bool {
        self.note("note_capped");
        true
    }
    fn note_drift_failure(&self, _: HWND) -> bool {
        self.note("note_failure");
        true
    }
    fn clear_drift_failure(&self, _: HWND) {
        self.note("clear_failure");
    }
    fn place(&self, _: HWND, region: Region) -> Result<(), Win32Error> {
        assert_eq!(region, THE_REGION);
        self.note(if self.in_place { "in_place" } else { "place" });
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
fn a_window_whose_exe_path_cannot_be_read_is_skipped_without_placing() {
    let mut ops = tracked_ops(true);
    ops.exe_known = false;

    let outcome = apply_zone_with(&ops, hwnd(), Trigger::Hotkey);

    assert!(matches!(
        outcome,
        Outcome::Declined(Declined::ProcessUnreadable)
    ));
    assert!(ops.calls().is_empty());
}

#[test]
fn a_window_already_in_place_still_re_arms_the_taskbar() {
    // `place` reports an in-place window as success: every foreground
    // switch back to the game re-arms the taskbar hide.
    let mut ops = tracked_ops(true);
    ops.in_place = true;

    let outcome = apply_zone_with(&ops, hwnd(), Trigger::AutoApply);

    assert!(matches!(outcome, Outcome::Applied));
    assert_eq!(
        ops.calls(),
        ["eligibility", "in_place", "remember", "taskbar_show"]
    );
}

#[test]
fn an_ineligible_window_is_skipped_before_anything_is_placed() {
    let mut ops = tracked_ops(true);
    ops.eligible = false;

    let outcome = apply_zone_with(&ops, hwnd(), Trigger::Hotkey);

    assert!(matches!(outcome, Outcome::Refused));
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
fn hotkey_live_reapply_and_tray_placements_are_remembered_for_drift_correction() {
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
    assert_eq!(
        ops.calls(),
        [
            "clear_failure",
            "drift_decision",
            "record_attempt",
            "eligibility",
            "place"
        ]
    );
}

#[test]
fn drift_corrects_a_hotkey_placed_game_whose_auto_apply_is_off() {
    let ops = tracked_ops(false);

    assert!(matches!(
        apply_zone_with(&ops, hwnd(), Trigger::Drift),
        Outcome::Applied
    ));
    assert!(ops.calls().contains(&"place".to_string()));
}

#[test]
fn drift_leaves_a_game_that_is_no_longer_tracked_alone() {
    let ops = FakeOps::new(Config {
        zone: Some(A_ZONE),
        ..Config::default()
    });

    assert!(matches!(
        apply_zone_with(&ops, hwnd(), Trigger::Drift),
        Outcome::Skipped
    ));
    assert!(ops.calls().is_empty());
}

#[test]
fn drift_skips_a_minimized_window_before_reading_anything() {
    let mut ops = tracked_ops(true);
    ops.minimized = true;

    assert!(matches!(
        apply_zone_with(&ops, hwnd(), Trigger::Drift),
        Outcome::Skipped
    ));
    assert!(ops.calls().is_empty());
}

#[test]
fn drift_does_nothing_when_the_gate_says_not_to_re_apply() {
    for decision in [DriftDecision::SkipNotDrifted, DriftDecision::SkipFullscreen] {
        let mut ops = tracked_ops(true);
        ops.drift = decision;

        assert!(matches!(
            apply_zone_with(&ops, hwnd(), Trigger::Drift),
            Outcome::Skipped
        ));
        assert!(!ops.calls().contains(&"place".to_string()), "{decision:?}");
        assert!(!ops.calls().contains(&"record_attempt".to_string()));
    }
}

#[test]
fn drift_inside_the_rate_limit_asks_for_a_re_check() {
    let mut ops = tracked_ops(true);
    let retry_in = Duration::from_millis(400);
    ops.drift = DriftDecision::SkipRateLimited { retry_in };

    let outcome = apply_zone_with(&ops, hwnd(), Trigger::Drift);

    assert!(matches!(outcome, Outcome::RetryIn(d) if d == retry_in));
    assert!(!ops.calls().contains(&"place".to_string()));
}

#[test]
fn a_capped_drift_is_reported_once_and_not_re_applied() {
    let mut ops = tracked_ops(true);
    ops.drift = DriftDecision::SkipCapped;

    let outcome = apply_zone_with(&ops, hwnd(), Trigger::Drift);

    assert!(matches!(outcome, Outcome::Skipped));
    assert!(ops.calls().contains(&"note_capped".to_string()));
    assert!(!ops.calls().contains(&"place".to_string()));
}

#[test]
fn a_refused_drift_window_still_counts_as_an_attempt() {
    let mut ops = tracked_ops(true);
    ops.eligible = false;

    let outcome = apply_zone_with(&ops, hwnd(), Trigger::Drift);

    assert!(matches!(outcome, Outcome::Refused));
    assert!(ops.calls().contains(&"record_attempt".to_string()));
    assert!(!ops.calls().contains(&"place".to_string()));
}

#[test]
fn drift_config_and_zone_failures_go_through_the_once_per_window_latch() {
    let mut broken_region = tracked_ops(true);
    broken_region.region_ok = false;
    let mut broken_config = tracked_ops(true);
    broken_config.config_ok = false;

    assert!(matches!(
        apply_zone_with(&broken_region, hwnd(), Trigger::Drift),
        Outcome::RegionFailed
    ));
    assert!(matches!(
        apply_zone_with(&broken_config, hwnd(), Trigger::Drift),
        Outcome::Skipped
    ));

    assert_eq!(broken_region.calls(), ["note_failure"]);
    assert_eq!(broken_config.calls(), ["note_failure"]);
}

#[test]
fn other_triggers_do_not_use_the_drift_failure_latch() {
    let mut ops = tracked_ops(true);
    ops.region_ok = false;

    apply_zone_with(&ops, hwnd(), Trigger::Hotkey);

    assert!(!ops.calls().contains(&"note_failure".to_string()));
}

#[test]
fn only_the_drift_trigger_goes_through_the_drift_gate() {
    let mut ops = tracked_ops(true);
    ops.drift = DriftDecision::SkipNotDrifted;

    let outcome = apply_zone_with(&ops, hwnd(), Trigger::Hotkey);

    assert!(matches!(outcome, Outcome::Applied));
    assert!(!ops.calls().contains(&"drift_decision".to_string()));
}

#[test]
fn drift_re_checks_a_hung_window_and_a_rate_limited_event_later() {
    let hung = Outcome::Failed(Win32Error::WindowHung);
    let retry_in = Duration::from_millis(300);

    assert_eq!(retry_after(&hung), Some(win32::HUNG_RECHECK_INTERVAL));
    assert_eq!(retry_after(&Outcome::RetryIn(retry_in)), Some(retry_in));
    assert_eq!(retry_after(&Outcome::Applied), None);
    assert_eq!(retry_after(&Outcome::Skipped), None);
    assert_eq!(
        retry_after(&Outcome::Failed(Win32Error::TaskbarNotFound)),
        None
    );
}

#[test]
fn a_hung_auto_apply_game_is_retried_later() {
    let mut ops = tracked_ops(true);
    ops.place_result = || Err(Win32Error::WindowHung);

    let outcome = apply_zone_with(&ops, hwnd(), Trigger::AutoApply);

    assert_eq!(retry_after(&outcome), Some(win32::HUNG_RECHECK_INTERVAL));
}

#[test]
fn automatic_triggers_skip_a_game_without_auto_apply() {
    let ops = tracked_ops(false);

    assert!(matches!(
        apply_zone_with(&ops, hwnd(), Trigger::AutoApply),
        Outcome::Skipped
    ));
    assert!(ops.calls().is_empty());
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

        let outcome = apply_zone_with(&ops, hwnd(), trigger);

        // Automatic triggers stay quiet; the user's own say why.
        if trigger.is_automatic() {
            assert!(matches!(outcome, Outcome::Skipped));
        } else {
            assert!(matches!(outcome, Outcome::Declined(Declined::NotTracked)));
        }
        assert!(ops.calls().is_empty());
    }
}

#[test]
fn no_zone_skips_the_placement() {
    let ops = FakeOps::new(config_with_zone_and_game(None, true));

    assert!(matches!(
        apply_zone_with(&ops, hwnd(), Trigger::Hotkey),
        Outcome::Declined(Declined::NoZone)
    ));
    assert!(ops.calls().is_empty());
}

#[test]
fn an_unreadable_config_is_declined_by_a_user_trigger_and_skipped_by_an_automatic_one() {
    let mut ops = tracked_ops(true);
    ops.config_ok = false;

    assert!(matches!(
        apply_zone_with(&ops, hwnd(), Trigger::Hotkey),
        Outcome::Declined(Declined::ConfigUnreadable)
    ));
    assert!(matches!(
        apply_zone_with(&ops, hwnd(), Trigger::AutoApply),
        Outcome::Skipped
    ));
}

#[test]
fn every_failure_a_user_can_hit_has_a_notice_and_successes_have_none() {
    let failures = [
        Outcome::Declined(Declined::NoZone),
        Outcome::Declined(Declined::NotTracked),
        Outcome::Declined(Declined::ProcessUnreadable),
        Outcome::Declined(Declined::ConfigUnreadable),
        Outcome::Refused,
        Outcome::RegionFailed,
        Outcome::Failed(Win32Error::WindowHung),
        Outcome::Failed(Win32Error::WindowMinimized),
    ];
    for trigger in [Trigger::Tray, Trigger::Hotkey] {
        for outcome in &failures {
            let notice = failure_notice(trigger, outcome);
            assert!(
                notice.as_deref().is_some_and(|n| !n.is_empty()),
                "{outcome:?}"
            );
        }
        assert_eq!(failure_notice(trigger, &Outcome::Applied), None);
        assert_eq!(failure_notice(trigger, &Outcome::Skipped), None);
    }
}

#[test]
fn the_notices_tell_the_reasons_apart() {
    let no_zone = failure_notice(Trigger::Hotkey, &Outcome::Declined(Declined::NoZone)).unwrap();
    let untracked =
        failure_notice(Trigger::Hotkey, &Outcome::Declined(Declined::NotTracked)).unwrap();
    let hung = failure_notice(Trigger::Hotkey, &Outcome::Failed(Win32Error::WindowHung)).unwrap();

    assert!(no_zone.contains("No Zone"));
    assert!(untracked.contains("tracked game"));
    assert!(hung.contains("isn't responding"));
}

#[test]
fn the_untracked_notice_only_points_to_the_tray_menu_for_the_hotkey() {
    let outcome = Outcome::Declined(Declined::NotTracked);

    let hotkey = failure_notice(Trigger::Hotkey, &outcome).unwrap();
    let tray = failure_notice(Trigger::Tray, &outcome).unwrap();

    assert!(hotkey.contains("tray menu"));
    assert!(!tray.contains("tray menu"));
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
    let save = calls.iter().position(|c| c == "track_game").unwrap();
    let place = calls.iter().position(|c| c == "place").unwrap();
    assert!(save < place);
    let saved = ops.config.borrow();
    assert_eq!(saved.tracked_games.len(), 1);
    assert_eq!(saved.tracked_games[0].display_name, "Thing Window");
    assert!(saved.tracked_games[0].auto_apply);
}

#[test]
fn tray_apply_still_places_with_an_in_memory_game_when_saving_the_tracked_game_fails() {
    let mut ops = FakeOps::new(Config {
        zone: Some(A_ZONE),
        ..Config::default()
    });
    ops.track_ok = false;

    let outcome = apply_zone_with(&ops, hwnd(), Trigger::Tray);

    assert!(matches!(outcome, Outcome::Applied));
    assert_eq!(ops.calls()[..3], ["eligibility", "track_game", "place"]);
    assert!(
        ops.config.borrow().tracked_games.is_empty(),
        "nothing was persisted"
    );
}

#[test]
fn tray_apply_keeps_the_tracking_when_no_zone_is_set() {
    let ops = FakeOps::new(Config::default());

    let outcome = apply_zone_with(&ops, hwnd(), Trigger::Tray);

    assert!(matches!(outcome, Outcome::Declined(Declined::NoZone)));
    assert_eq!(ops.calls(), ["eligibility", "track_game"]);
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

    assert!(matches!(outcome, Outcome::Refused));
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
