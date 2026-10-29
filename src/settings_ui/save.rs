//! Saving: merging the edits into the config on disk, persisting the Zone and
//! the live re-apply to running games.

use super::{SettingsExit, SettingsState, SettingsUiError, TickContext};
use crate::placement;
use crate::profile_store;
use crate::profile_store::{Config, HotkeyBindings, TrackedGame, Zone};
use crate::win32;
use std::path::Path;

/// The non-blocking status-line message shown for a tracked game whose live
/// re-apply attempt failed -- e.g. the window couldn't be
/// repositioned because of an access-denied error. The Zone itself was already
/// saved (the overlay persists it on Save); the message just explains that
/// the game will pick it up the next time it is applied (launch, hotkey or
/// tray "Apply to Window") instead of immediately.
///
/// Pure string formatting -- no OS dependency -- so it's unit tested in
/// complete isolation from the live re-apply attempt that decides whether to
/// show it.
fn live_apply_failure_message(display_name: &str) -> String {
    format!(
        "couldn't reposition {display_name} live -- the saved Zone will be used the next time it launches or you apply it"
    )
}

/// The config to persist when the Zone changes on its own (overlay Save or
/// Clear): the on-disk `config` with only its Zone replaced, so every other
/// field -- including edits Settings has not saved yet, which are not in
/// `config` -- is left exactly as it is on disk.
fn config_with_zone(mut config: Config, zone: Option<Zone>) -> Config {
    config.zone = zone;
    config
}

/// Persists `zone` to `config_dir` immediately (load, replace the Zone, save),
/// so the on-disk Zone -- which the background re-apply paths read -- never
/// disagrees with the Zone applied to running windows.
///
/// Returns the config as saved, so callers act on the on-disk taskbar settings
/// (`hide_taskbar_on_apply`, `taskbar_hide_mode`) rather than on unsaved
/// Settings state.
pub(crate) fn persist_zone(
    config_dir: &Path,
    zone: Option<Zone>,
) -> Result<Config, SettingsUiError> {
    let saved = profile_store::update(config_dir, |config| {
        *config = config_with_zone(std::mem::take(config), zone);
        config.clone()
    })
    .map_err(SettingsUiError::Config)?;
    // In the Settings child: the tray process caches the config, so it must
    // learn that the file changed.
    crate::settings_link::config_changed();
    Ok(saved)
}

/// Live re-applies the Zone on disk to every running tracked game in `games`
/// through [`placement::apply_zone`] (the Zone is shared, so a new one affects
/// all of them). Returns one status-line message per game whose re-apply
/// failed -- an empty list if every game either isn't running, was skipped
/// (e.g. refused by the eligibility filter; logged) or was repositioned.
///
/// A game that isn't running is skipped *before* any Zone resolution, so a
/// saved Zone that no longer fits the monitor never surfaces as a failure for a
/// game that was never running; its setting still applies on its next
/// launch/hotkey/tray-apply.
///
/// OS-facing -- not exercised by this crate's automated tests; the
/// orchestration is tested in `placement`, and
/// [`live_apply_failure_message`] is the pure message text.
pub(crate) fn live_reapply_all(config_dir: &Path, games: &[TrackedGame]) -> Vec<String> {
    games
        .iter()
        .filter_map(|game| {
            let hwnd = win32::resolve_running_window(&game.exe_path)?;
            let outcome = placement::apply_zone(config_dir, hwnd, placement::Trigger::LiveReapply);
            live_apply_status(&game.display_name, &outcome)
        })
        .collect()
}

/// The status-line message for one live re-apply [`placement::Outcome`], if it
/// warrants one.
fn live_apply_status(display_name: &str, outcome: &placement::Outcome) -> Option<String> {
    match outcome {
        placement::Outcome::Applied
        | placement::Outcome::Skipped
        | placement::Outcome::Declined(_)
        | placement::Outcome::RetryIn(_)
        // Refused by the eligibility filter: it would be refused on the next
        // launch too, so the "used next launch" message would be wrong;
        // skipped silently (logged by placement).
        | placement::Outcome::Refused => None,
        placement::Outcome::Failed(e) if e.is_window_hung() => {
            Some(live_apply_hung_message(display_name))
        }
        placement::Outcome::RegionFailed | placement::Outcome::Failed(_) => {
            Some(live_apply_failure_message(display_name))
        }
    }
}

/// The status-line message for a game that was not responding at the live
/// re-apply (already logged once where the hang was found).
fn live_apply_hung_message(display_name: &str) -> String {
    format!(
        "{display_name} is not responding -- the saved Zone will be used the next time it launches or you apply it"
    )
}

/// Applies Save to the config `on_disk`: only the fields the user changed
/// (`edited` differs from `opened`, what Settings opened with) are written, so
/// anything the tray or hotkeys changed on disk in a field the user left alone
/// -- a tracked game, the game-cursor toggle, a setting -- is not overwritten
/// with Settings' stale copy. A changed tracked-games list is merged with the
/// disk (see [`merge_tracked_games`]); `cursor.enabled` follows
/// [`merge_cursor_enabled`].
pub(super) fn apply_saved_changes(
    opened: &Config,
    edited: Config,
    cursor_enabled_seen: bool,
    on_disk: &mut Config,
) {
    // Moves, not clones: every field of `edited` is used at most once.
    let Config {
        zone,
        hotkeys,
        tracked_games,
        taskbar_hide_mode,
        hide_taskbar_on_apply,
        cursor,
        update_mode,
        log_ui_hangs,
    } = edited;

    // The Zone is persisted immediately by the zone editor and "Clear", not by
    // Save, so Save never writes it (`on_disk.zone` stays as it is).
    let _ = zone;
    let HotkeyBindings {
        toggle_borderless,
        toggle_taskbar_autohide,
        toggle_game_cursor,
    } = hotkeys;
    if toggle_borderless != opened.hotkeys.toggle_borderless {
        on_disk.hotkeys.toggle_borderless = toggle_borderless;
    }
    if toggle_taskbar_autohide != opened.hotkeys.toggle_taskbar_autohide {
        on_disk.hotkeys.toggle_taskbar_autohide = toggle_taskbar_autohide;
    }
    if toggle_game_cursor != opened.hotkeys.toggle_game_cursor {
        on_disk.hotkeys.toggle_game_cursor = toggle_game_cursor;
    }
    if tracked_games != opened.tracked_games {
        let merged =
            merge_tracked_games(&opened.tracked_games, tracked_games, &on_disk.tracked_games);
        on_disk.tracked_games = merged;
    }
    if taskbar_hide_mode != opened.taskbar_hide_mode {
        on_disk.taskbar_hide_mode = taskbar_hide_mode;
    }
    if hide_taskbar_on_apply != opened.hide_taskbar_on_apply {
        on_disk.hide_taskbar_on_apply = hide_taskbar_on_apply;
    }
    // `cursor.enabled` is also flipped by the tray's hotkey, so it is judged
    // against what Settings last saw rather than what it opened with; the
    // other cursor fields only the user edits.
    let mut cursor = cursor;
    let enabled = merge_cursor_enabled(cursor_enabled_seen, cursor.enabled, on_disk.cursor.enabled);
    cursor.enabled = opened.cursor.enabled;
    if cursor != opened.cursor {
        on_disk.cursor = cursor;
    }
    on_disk.cursor.enabled = enabled;
    if update_mode != opened.update_mode {
        on_disk.update_mode = update_mode;
    }
    if log_ui_hangs != opened.log_ui_hangs {
        on_disk.log_ui_hangs = log_ui_hangs;
    }
}

/// The `cursor.enabled` Save writes. The game-cursor hotkey persists its toggle
/// straight to disk and tells Settings over the link, which can still be in
/// flight when Save is clicked. A checkbox that still shows what Settings last
/// saw (`seen`) was not touched by the user, so the value on disk (the newer
/// toggle) wins; one the user changed wins over the disk.
fn merge_cursor_enabled(seen: bool, edited: bool, on_disk: bool) -> bool {
    if edited == seen { on_disk } else { edited }
}

/// The tracked-games list Save writes. Settings edits its own copy
/// (`edited`), but the tray's "Apply to Window" can add games on disk while
/// Settings is open (the tray process keeps running meanwhile). A game that is on
/// disk (`on_disk`) but was not in the list Settings opened with (`opened`) was
/// added in the meantime and is appended to `edited` so Save doesn't lose it.
/// A game in `opened` but missing from `edited` was removed by the user in
/// Settings and stays removed; a game already in `edited` is never duplicated.
///
/// Known limits: a game removed in Settings and then re-tracked through the
/// tray while Settings is open is dropped again on Save (it is in `opened`, so
/// it counts as removed), and per-game edits made on disk to a game already in
/// `opened` lose to Settings' copy.
///
/// Pure -- compares executable paths only.
fn merge_tracked_games(
    opened: &[TrackedGame],
    mut edited: Vec<TrackedGame>,
    on_disk: &[TrackedGame],
) -> Vec<TrackedGame> {
    for game in on_disk {
        let known_at_open = opened.iter().any(|g| g.exe_path == game.exe_path);
        let in_edited = edited.iter().any(|g| g.exe_path == game.exe_path);
        if !known_at_open && !in_edited {
            edited.push(game.clone());
        }
    }
    edited
}

/// Builds the Save result. Ticks once more first, so a game-cursor hotkey
/// toggle that landed after this frame's tick (but before the click was
/// handled) is adopted and not overwritten by the stale checkbox value.
pub(super) fn save_exit(
    state: &mut SettingsState,
    on_tick: &mut dyn FnMut(TickContext) -> Option<bool>,
) -> SettingsExit {
    if let Some(enabled) = on_tick(state.tick_context()) {
        state.apply_external_cursor_enabled(enabled);
    }
    SettingsExit::Saved(
        state.to_config(),
        state.start_with_windows,
        state.cursor_enabled_seen,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile_store::{TaskbarHideMode, UpdateMode};

    fn game(path: &str) -> TrackedGame {
        TrackedGame::new_for_cursor_setting(std::path::PathBuf::from(path), path.to_string(), true)
    }

    #[test]
    fn merge_tracked_games_keeps_a_game_added_on_disk_while_settings_was_open() {
        let opened = vec![game("a.exe")];
        let edited = vec![game("a.exe")];
        let on_disk = vec![game("a.exe"), game("b.exe")];

        let merged = merge_tracked_games(&opened, edited, &on_disk);

        assert_eq!(merged, vec![game("a.exe"), game("b.exe")]);
    }

    #[test]
    fn merge_tracked_games_keeps_a_game_the_user_removed_in_settings_removed() {
        let opened = vec![game("a.exe"), game("b.exe")];
        let edited = vec![game("a.exe")];
        let on_disk = opened.clone();

        let merged = merge_tracked_games(&opened, edited, &on_disk);

        assert_eq!(merged, vec![game("a.exe")]);
    }

    #[test]
    fn merge_tracked_games_keeps_the_users_edits_to_a_game_and_adds_no_duplicate() {
        let opened = vec![game("a.exe")];
        let mut edited_a = game("a.exe");
        edited_a.auto_apply = !edited_a.auto_apply;
        let on_disk = vec![game("a.exe"), game("b.exe")];

        let merged = merge_tracked_games(&opened, vec![edited_a.clone()], &on_disk);

        assert_eq!(merged, vec![edited_a, game("b.exe")]);
    }

    #[test]
    fn merge_tracked_games_does_not_duplicate_a_game_already_in_the_edited_list() {
        let merged = merge_tracked_games(&[], vec![game("b.exe")], &[game("b.exe")]);

        assert_eq!(merged, vec![game("b.exe")]);
    }

    #[test]
    fn save_keeps_a_cursor_toggle_the_tray_persisted_but_settings_has_not_heard_of() {
        // Opened with the cursor off; the hotkey turned it on, the message to
        // Settings was still in flight at Save: the checkbox shows "off" still.
        assert!(merge_cursor_enabled(false, false, true));
        // The user ticked the box themselves: that wins over the disk.
        assert!(merge_cursor_enabled(false, true, false));
        assert!(!merge_cursor_enabled(true, false, true));
        // Nothing happened anywhere.
        assert!(!merge_cursor_enabled(false, false, false));
    }

    #[test]
    fn save_keeps_disk_games_and_cursor_toggle_while_writing_the_users_edit() {
        let opened = Config::default();
        let edited = Config {
            log_ui_hangs: !opened.log_ui_hangs,
            ..Config::default()
        };

        let mut on_disk = Config::default();
        on_disk.cursor.enabled = true;
        on_disk.tracked_games = vec![game("b.exe")];

        apply_saved_changes(&opened, edited, false, &mut on_disk);

        assert_eq!(
            on_disk.log_ui_hangs, !opened.log_ui_hangs,
            "the user's edit is saved"
        );
        assert!(on_disk.cursor.enabled, "the tray's toggle survives");
        assert_eq!(on_disk.tracked_games, vec![game("b.exe")]);
    }

    #[test]
    fn save_leaves_fields_the_user_did_not_change_as_they_are_on_disk() {
        let opened = Config::default();
        let edited = opened.clone();

        // Everything differs on disk: changed by the tray/hotkeys meanwhile.
        let mut on_disk = Config {
            hide_taskbar_on_apply: !opened.hide_taskbar_on_apply,
            update_mode: UpdateMode::Ask,
            log_ui_hangs: !opened.log_ui_hangs,
            taskbar_hide_mode: TaskbarHideMode::HoverReveal,
            ..Config::default()
        };
        on_disk.hotkeys.toggle_borderless = Some("Ctrl+F1".to_string());
        on_disk.cursor.size += 5;
        on_disk.tracked_games = vec![game("b.exe")];
        let expected = on_disk.clone();

        apply_saved_changes(&opened, edited, false, &mut on_disk);

        assert_eq!(on_disk, expected);
    }

    #[test]
    fn save_never_writes_the_zone_the_editor_already_persisted() {
        let opened = Config::default();
        let mut edited = opened.clone();
        let zone = |width| {
            Some(Zone {
                x: 0,
                y: 0,
                width,
                height: 100,
            })
        };
        edited.zone = zone(10);
        edited.log_ui_hangs = !opened.log_ui_hangs;

        let mut on_disk = opened.clone();
        on_disk.zone = zone(20);
        let disk_zone = on_disk.zone;

        apply_saved_changes(&opened, edited, false, &mut on_disk);

        assert_eq!(on_disk.zone, disk_zone);
        assert_eq!(on_disk.log_ui_hangs, !opened.log_ui_hangs);
    }

    #[test]
    fn save_writes_a_changed_field_and_keeps_an_untouched_one_from_disk() {
        let opened = Config::default();
        let mut edited = opened.clone();
        edited.hotkeys.toggle_taskbar_autohide = Some("Ctrl+F2".to_string());
        edited.cursor.size += 3;

        let mut on_disk = opened.clone();
        on_disk.hotkeys.toggle_borderless = Some("Ctrl+F1".to_string());
        on_disk.cursor.enabled = true;

        apply_saved_changes(&opened, edited.clone(), false, &mut on_disk);

        assert_eq!(
            on_disk.hotkeys.toggle_taskbar_autohide,
            Some("Ctrl+F2".to_string())
        );
        assert_eq!(
            on_disk.hotkeys.toggle_borderless,
            Some("Ctrl+F1".to_string())
        );
        assert_eq!(on_disk.cursor.size, edited.cursor.size);
        assert!(
            on_disk.cursor.enabled,
            "the hotkey's toggle survives a cursor edit"
        );
    }

    #[test]
    fn save_does_not_overwrite_a_cursor_toggle_landing_between_the_last_tick_and_the_write() {
        // Issue #215: the checkbox still shows what Settings saw (off); the
        // hotkey turned the cursor on just before Save took the lock.
        let opened = Config::default();
        let edited = opened.clone();
        let mut on_disk = opened.clone();
        on_disk.cursor.enabled = true;

        apply_saved_changes(&opened, edited, opened.cursor.enabled, &mut on_disk);

        assert!(on_disk.cursor.enabled);
    }

    #[test]
    fn a_cursor_toggle_adopted_from_the_tray_is_the_new_baseline() {
        let mut state = SettingsState::from_config(&Config::default());
        state.apply_external_cursor_enabled(true);
        let SettingsExit::Saved(config, _, seen) = save_exit(&mut state, &mut |_| None) else {
            panic!("save_exit must produce Saved");
        };
        assert!(seen && config.cursor.enabled);
    }

    #[test]
    fn save_adopts_a_cursor_toggle_reported_by_the_final_tick() {
        let mut state = SettingsState::from_config(&Config::default());
        assert!(!state.cursor.enabled);

        let mut contexts = Vec::new();
        let mut on_tick = |context| {
            contexts.push(context);
            Some(true)
        };
        let SettingsExit::Saved(config, _, _) = save_exit(&mut state, &mut on_tick) else {
            panic!("save_exit must produce Saved");
        };
        assert!(config.cursor.enabled);
        assert_eq!(contexts.len(), 1);
    }

    #[test]
    fn save_keeps_the_checkbox_when_the_final_tick_reports_nothing() {
        let mut state = SettingsState::from_config(&Config::default());
        state.cursor.enabled = true;
        let SettingsExit::Saved(config, _, _) = save_exit(&mut state, &mut |_| None) else {
            panic!("save_exit must produce Saved");
        };
        assert!(config.cursor.enabled);
    }

    #[test]
    fn an_outside_cursor_toggle_updates_the_checkbox_and_save_writes_it() {
        let mut state = SettingsState::from_config(&Config::default());
        assert!(!state.cursor.enabled);

        // The game-cursor hotkey turns it on while Settings is open.
        state.apply_external_cursor_enabled(true);
        assert!(state.cursor.enabled);
        assert!(state.to_config().cursor.enabled);

        // The user then unticks the checkbox; a later outside toggle wins again.
        state.cursor.enabled = false;
        state.apply_external_cursor_enabled(true);
        assert!(state.to_config().cursor.enabled);
    }

    #[test]
    fn an_outside_cursor_toggle_leaves_other_unsaved_edits_alone() {
        let mut state = SettingsState::from_config(&Config::default());
        state.cursor.size += 10;
        let size = state.cursor.size;

        state.apply_external_cursor_enabled(true);

        assert_eq!(state.cursor.size, size);
    }

    #[test]
    fn a_declined_live_reapply_stays_off_the_status_line() {
        let declined = placement::Outcome::Declined(placement::Declined::NoZone);

        assert_eq!(live_apply_status("Thing", &declined), None);
    }

    #[test]
    fn a_hung_game_gets_the_not_responding_message_not_the_generic_one() {
        use crate::win32::Win32Error;
        let hung = placement::Outcome::Failed(Win32Error::WindowHung);
        let other = placement::Outcome::Failed(Win32Error::WindowMinimized);

        let hung_message = live_apply_status("Thing", &hung).unwrap();

        assert!(hung_message.contains("Thing is not responding"));
        assert_eq!(
            live_apply_status("Thing", &other),
            Some(live_apply_failure_message("Thing"))
        );
        assert_eq!(
            live_apply_status("Thing", &placement::Outcome::Applied),
            None
        );
    }

    #[test]
    fn live_apply_failure_message_names_the_game_and_explains_the_fallback() {
        let message = live_apply_failure_message("Thing");

        assert_eq!(
            message,
            "couldn't reposition Thing live -- the saved Zone will be used the next time it launches or you apply it"
        );
    }

    #[test]
    fn config_with_zone_replaces_only_the_zone_of_the_on_disk_config() {
        let mut on_disk = Config {
            zone: Some(Zone {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            }),
            ..Config::default()
        };
        on_disk.hotkeys.toggle_borderless = Some("Ctrl+B".to_string());
        let new_zone = Zone {
            x: 100,
            y: 50,
            width: 800,
            height: 600,
        };

        let updated = config_with_zone(on_disk.clone(), Some(new_zone));

        assert_eq!(updated.zone, Some(new_zone));
        assert_eq!(updated.hotkeys, on_disk.hotkeys);
        assert_eq!(updated.tracked_games, on_disk.tracked_games);
        assert_eq!(config_with_zone(on_disk, None).zone, None);
    }

    #[test]
    fn persist_zone_writes_the_zone_and_keeps_the_rest_of_the_on_disk_config() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let config = Config {
            hotkeys: HotkeyBindings {
                toggle_borderless: Some("Ctrl+B".to_string()),
                ..HotkeyBindings::default()
            },
            hide_taskbar_on_apply: false,
            ..Config::default()
        };
        profile_store::save(dir, &config).unwrap();
        let zone = Zone {
            x: 10,
            y: 20,
            width: 300,
            height: 200,
        };

        let saved = persist_zone(dir, Some(zone)).unwrap();

        let reloaded = profile_store::load(dir).unwrap();
        assert_eq!(reloaded.zone, Some(zone));
        assert_eq!(reloaded.hotkeys, config.hotkeys);
        assert_eq!(
            saved, reloaded,
            "persist_zone must return the on-disk config the live re-apply reads"
        );
        assert!(!saved.hide_taskbar_on_apply);
    }
}
