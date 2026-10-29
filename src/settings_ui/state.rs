//! The editable view-model of the Settings window: plain data with no egui/OS
//! dependency.

use super::{TickContext, format_captured_hotkey, is_physical_modifier_key};
use crate::hotkeys::{BindingOutcome, BindingReport, HotkeyAction};
use crate::profile_store::{
    Config, CursorSettings, HotkeyBindings, TaskbarHideMode, TrackedGame, UpdateMode, Zone,
};

/// The editable state backing the Settings window: the single optional
/// [`Zone`], the three hotkey binding strings (empty = unbound) and the other
/// Settings fields.
///
/// Plain data, no egui/OS dependency -- fully unit testable.
///
/// The Zone is set/replaced entirely by the zone-editor overlay (see
/// [`crate::zone_overlay::run_zone_editor_overlay`]) -- there is no scratch
/// input state to hold here, it is written directly once the overlay
/// reports a Save.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsState {
    pub zone: Option<Zone>,
    pub toggle_borderless: String,
    pub toggle_taskbar_autohide: String,
    pub toggle_game_cursor: String,
    /// The game cursor's settings, edited by the "Game cursor" section.
    pub cursor: CursorSettings,
    /// `Some(field)` while a press-to-bind capture is in progress for that
    /// hotkey field (waiting for the next non-modifier key-down); `None`
    /// otherwise. See [`Self::start_capture`]/[`Self::handle_key_event`].
    pub capturing: Option<HotkeyField>,
    /// `true` immediately after a key press during capture was ignored
    /// because it has no name in GameFrame's hotkey vocabulary (see
    /// `hotkeys::key_display_name`) -- lets the UI show a transient "key not
    /// supported" hint instead of silently doing nothing. Reset whenever a
    /// new capture starts or the current one ends (completed or cancelled).
    pub unsupported_key_pressed: bool,
    /// `Some(message)` immediately after a capture would complete with the
    /// same combo already bound to the *other* hotkey field -- lets the UI
    /// surface an "already bound" error instead of silently accepting a
    /// binding that would later fail at `Hotkeys::register`. Reset whenever
    /// a new capture starts or the current one ends.
    pub duplicate_hotkey_error: Option<String>,
    /// The tracked-games list, editable in place by the Settings "Games"
    /// section (auto-apply per row, and immediate
    /// no-confirmation removal) -- entries are added by
    /// [`crate::placement::apply_zone`]'s `Trigger::Tray` branch.
    pub tracked_games: Vec<TrackedGame>,
    /// Which technique GameFrame uses to hide the taskbar whenever any call
    /// site hides it -- `FullyHidden` (the default, no hover-reveal) or
    /// `HoverReveal` (the native `SHAppBarMessage` behavior, opt-in). Set via
    /// a radio choice in the Settings window.
    pub taskbar_hide_mode: TaskbarHideMode,
    /// Whether applying the Zone also hides the taskbar -- the global
    /// "Hide taskbar when the Zone is applied" checkbox.
    pub hide_taskbar_on_apply: bool,
    /// The "Start with Windows" checkbox. Not part of [`Config`] -- the
    /// registry is its source of truth (see [`crate::autostart`]) -- so
    /// [`Self::from_config`] leaves it `false` and [`super::run_settings_window`]
    /// fills it from the registry; Save writes it back only if it changed.
    pub start_with_windows: bool,
    /// What the startup update check does with an available update (the
    /// radio choice under Updates).
    pub update_mode: UpdateMode,
    /// The Log UI hangs (diagnostics) checkbox.
    pub log_ui_hangs: bool,
    /// The open tab. UI-only (never persisted), but kept here rather than on
    /// [`super::SettingsApp`] so it survives the "Edit Zone" close-and-reopen.
    pub tab: SettingsTab,
    /// Bindings that failed to register when GameFrame last registered its
    /// hotkeys (see [`Self::set_hotkey_reports`]). Read through
    /// [`Self::hotkey_failure`], which hides one once its binding was edited.
    hotkey_failures: Vec<BindingReport>,
    /// The `cursor.enabled` this session last saw on disk: what it opened with,
    /// or the last value the game-cursor hotkey reported. Save keeps the value
    /// on disk when the checkbox still shows this (see `save::merge_cursor_enabled`).
    pub(super) cursor_enabled_seen: bool,
}

/// The Settings window's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    Games,
    Cursor,
    /// Zone, global hotkeys and taskbar hiding.
    Zone,
    /// Startup and diagnostics.
    General,
    /// Version, license, repository, the optional donation link and the
    /// update settings.
    About,
}

impl SettingsTab {
    /// Every tab in tab-bar order.
    pub const ALL: [Self; 5] = [
        Self::Games,
        Self::Cursor,
        Self::Zone,
        Self::General,
        Self::About,
    ];

    /// The tab-bar label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Games => "Games",
            Self::Cursor => "Cursor",
            Self::Zone => "Zone, hotkeys & taskbar",
            Self::General => "General",
            Self::About => "About",
        }
    }

    /// The tab Settings opens on: without a Zone the Games and Cursor tabs
    /// are greyed out, so open on the one that lets the user draw it.
    pub fn initial(zone: Option<Zone>) -> Self {
        if zone.is_none() {
            Self::Zone
        } else {
            Self::default()
        }
    }
}

/// Which of GameFrame's three hotkey fields a press-to-bind capture applies
/// to. See [`SettingsState::capturing`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)] // mirrors `hotkeys::HotkeyAction`
pub enum HotkeyField {
    ToggleBorderless,
    ToggleTaskbarAutohide,
    ToggleGameCursor,
}

impl From<HotkeyAction> for HotkeyField {
    fn from(action: HotkeyAction) -> Self {
        match action {
            HotkeyAction::ToggleBorderless => HotkeyField::ToggleBorderless,
            HotkeyAction::ToggleTaskbarAutohide => HotkeyField::ToggleTaskbarAutohide,
            HotkeyAction::ToggleGameCursor => HotkeyField::ToggleGameCursor,
        }
    }
}

impl SettingsState {
    /// Builds the editable state from a loaded/default [`Config`]. Unbound
    /// hotkeys (`None`) become empty strings.
    pub fn from_config(config: &Config) -> Self {
        Self {
            zone: config.zone,
            toggle_borderless: config.hotkeys.toggle_borderless.clone().unwrap_or_default(),
            toggle_taskbar_autohide: config
                .hotkeys
                .toggle_taskbar_autohide
                .clone()
                .unwrap_or_default(),
            toggle_game_cursor: config
                .hotkeys
                .toggle_game_cursor
                .clone()
                .unwrap_or_default(),
            cursor: config.cursor.clone(),
            capturing: None,
            unsupported_key_pressed: false,
            duplicate_hotkey_error: None,
            tracked_games: config.tracked_games.clone(),
            taskbar_hide_mode: config.taskbar_hide_mode,
            hide_taskbar_on_apply: config.hide_taskbar_on_apply,
            start_with_windows: false,
            update_mode: config.update_mode,
            log_ui_hangs: config.log_ui_hangs,
            tab: SettingsTab::initial(config.zone),
            hotkey_failures: Vec::new(),
            cursor_enabled_seen: config.cursor.enabled,
        }
    }

    /// Records which bindings failed to register at the last registration.
    /// The report was made while GameFrame's own registrations held the
    /// combos, so Settings never probes the OS itself (it would see its own
    /// bindings as "in use").
    pub fn set_hotkey_reports(&mut self, reports: &[BindingReport]) {
        self.hotkey_failures = reports
            .iter()
            .filter(|r| matches!(r.outcome, BindingOutcome::Failed(_)))
            .cloned()
            .collect();
    }

    /// Why `field`'s binding failed to register, if it did and the binding is
    /// still the one that failed. Rebinding or clearing hides it: the new
    /// combination is only tried (and the indicator settled) on Save.
    pub fn hotkey_failure(&self, field: HotkeyField) -> Option<&str> {
        self.hotkey_failures
            .iter()
            .find(|f| HotkeyField::from(f.action) == field && f.binding == self.binding(field))
            .and_then(|f| match &f.outcome {
                BindingOutcome::Failed(reason) => Some(reason.as_str()),
                BindingOutcome::Registered => None,
            })
    }

    /// Builds the [`Config`] to persist. A blank (or whitespace-only) hotkey
    /// field is treated as unbound (`None`).
    pub fn to_config(&self) -> Config {
        Config {
            zone: self.zone,
            hotkeys: HotkeyBindings {
                toggle_borderless: non_empty(&self.toggle_borderless),
                toggle_taskbar_autohide: non_empty(&self.toggle_taskbar_autohide),
                toggle_game_cursor: non_empty(&self.toggle_game_cursor),
            },
            tracked_games: self.tracked_games.clone(),
            taskbar_hide_mode: self.taskbar_hide_mode,
            hide_taskbar_on_apply: self.hide_taskbar_on_apply,
            cursor: self.cursor.clone(),
            update_mode: self.update_mode,
            log_ui_hangs: self.log_ui_hangs,
        }
    }

    /// The tick context for the Settings window: hotkeys are swallowed while a
    /// hotkey capture is in progress.
    pub fn tick_context(&self) -> TickContext {
        if self.capturing.is_some() {
            TickContext::SettingsCapturing
        } else {
            TickContext::Settings
        }
    }

    /// Adopts a `cursor.enabled` change made outside Settings while it is open
    /// (the game-cursor hotkey persists it straight to disk), so the checkbox
    /// shows it and Save writes it. Other unsaved edits are left alone.
    pub fn apply_external_cursor_enabled(&mut self, enabled: bool) {
        self.cursor.enabled = enabled;
        self.cursor_enabled_seen = enabled;
    }

    /// Replaces the Zone entirely with `zone` -- the result of a completed
    /// zone-editor overlay session
    /// ([`crate::zone_overlay::run_zone_editor_overlay`] returning
    /// `Ok(Some(zone))`). There is at most one Zone, so this always
    /// replaces rather than appends.
    pub fn set_zone(&mut self, zone: Zone) {
        self.zone = Some(zone);
    }

    /// Clears the Zone entirely (back to "no zone set").
    pub fn clear_zone(&mut self) {
        self.zone = None;
    }

    pub fn start_capture(&mut self, field: HotkeyField) {
        self.capturing = Some(field);
        self.unsupported_key_pressed = false;
        self.duplicate_hotkey_error = None;
    }

    /// Ends the current capture (if any) without changing any hotkey binding.
    /// The single reset point for `capturing`/`unsupported_key_pressed`/
    /// `duplicate_hotkey_error` used by every cancellation path (Escape,
    /// the "Cancel" button, and `clear_hotkey`'s same-field cancel) so the
    /// three flags can't drift out of sync between call sites.
    pub fn cancel_capture(&mut self) {
        self.capturing = None;
        self.unsupported_key_pressed = false;
        self.duplicate_hotkey_error = None;
    }

    pub fn handle_key_event(&mut self, modifiers: egui::Modifiers, key: egui::Key) {
        let Some(field) = self.capturing else {
            return;
        };

        if key == egui::Key::Escape && modifiers.is_none() {
            self.cancel_capture();
            return;
        }

        let Some(combo) = format_captured_hotkey(modifiers, key) else {
            // A physical-modifier-only press stays silently listening (expected,
            // not an error); anything else is a genuinely unmapped key.
            if !is_physical_modifier_key(key) {
                self.unsupported_key_pressed = true;
            }
            return;
        };

        let already_bound = [
            (HotkeyField::ToggleBorderless, &self.toggle_borderless),
            (
                HotkeyField::ToggleTaskbarAutohide,
                &self.toggle_taskbar_autohide,
            ),
            (HotkeyField::ToggleGameCursor, &self.toggle_game_cursor),
        ]
        .into_iter()
        .any(|(other, binding)| other != field && !binding.is_empty() && *binding == combo);
        if already_bound {
            self.duplicate_hotkey_error =
                Some(format!("{combo} is already bound to another hotkey"));
            return;
        }

        *self.binding_mut(field) = combo;
        self.cancel_capture();
    }

    /// Removes the tracked game at `index` immediately, with no confirmation
    /// -- matching this module's existing immediate-no-confirm pattern (see
    /// [`Self::clear_hotkey`]/[`Self::clear_zone`]).
    pub fn remove_tracked_game(&mut self, index: usize) {
        self.tracked_games.remove(index);
    }

    /// Completes the current capture (if any) directly with "Ctrl+Escape"
    /// when `ctrl_escape_suppressed` is `true` -- the direct-completion path
    /// needed because `keyboard_hook::keyboard_hook_proc` swallows the
    /// physical Ctrl+Escape combination before it can ever arrive as a
    /// normal `egui::Event::Key` for [`Self::handle_key_event`] to observe.
    /// A no-op when `ctrl_escape_suppressed` is `false`, or when nothing is
    /// currently capturing.
    ///
    /// Deliberately reuses [`Self::handle_key_event`]'s existing, already-
    /// tested Ctrl+Escape formatting/duplicate-check logic rather than
    /// duplicating it -- takes the already-polled flag as a plain `bool`
    /// (see [`crate::keyboard_hook::take_suppressed_ctrl_escape`]) so this
    /// stays pure and unit-testable without a real OS-level hook; only the
    /// polling call itself is OS-facing.
    pub fn complete_capture_with_ctrl_escape_if_suppressed(
        &mut self,
        ctrl_escape_suppressed: bool,
    ) {
        if ctrl_escape_suppressed {
            self.handle_key_event(
                egui::Modifiers {
                    ctrl: true,
                    ..egui::Modifiers::NONE
                },
                egui::Key::Escape,
            );
        }
    }

    /// The stored binding string behind `field`.
    pub fn binding(&self, field: HotkeyField) -> &str {
        match field {
            HotkeyField::ToggleBorderless => &self.toggle_borderless,
            HotkeyField::ToggleTaskbarAutohide => &self.toggle_taskbar_autohide,
            HotkeyField::ToggleGameCursor => &self.toggle_game_cursor,
        }
    }

    fn binding_mut(&mut self, field: HotkeyField) -> &mut String {
        match field {
            HotkeyField::ToggleBorderless => &mut self.toggle_borderless,
            HotkeyField::ToggleTaskbarAutohide => &mut self.toggle_taskbar_autohide,
            HotkeyField::ToggleGameCursor => &mut self.toggle_game_cursor,
        }
    }

    pub fn clear_hotkey(&mut self, field: HotkeyField) {
        self.binding_mut(field).clear();
        if self.capturing == Some(field) {
            self.cancel_capture();
        }
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile_store;

    #[test]
    fn from_config_and_to_config_round_trip_the_global_taskbar_flag() {
        let mut config = Config::default();
        assert!(!SettingsState::from_config(&config).hide_taskbar_on_apply);

        config.hide_taskbar_on_apply = true;
        let state = SettingsState::from_config(&config);
        assert!(state.hide_taskbar_on_apply);
        assert!(state.to_config().hide_taskbar_on_apply);

        let mut state = state;
        state.hide_taskbar_on_apply = false;
        assert!(!state.to_config().hide_taskbar_on_apply);
    }

    #[test]
    fn from_config_of_a_fresh_default_config_shows_unbound_hotkeys_and_no_zone() {
        let config = Config::default();

        let state = SettingsState::from_config(&config);

        assert_eq!(state.zone, None);
        assert_eq!(state.toggle_borderless, "");
        assert_eq!(state.toggle_taskbar_autohide, "");
        assert_eq!(state.taskbar_hide_mode, TaskbarHideMode::FullyHidden);
    }

    #[test]
    fn from_config_of_a_saved_config_shows_the_saved_hotkeys_and_zone() {
        let config = Config {
            zone: Some(Zone {
                x: 10,
                y: 10,
                width: 200,
                height: 100,
            }),
            hotkeys: HotkeyBindings {
                toggle_borderless: Some("Ctrl+Alt+B".to_string()),
                toggle_taskbar_autohide: Some("Ctrl+Alt+T".to_string()),
                toggle_game_cursor: Some("Ctrl+Alt+M".to_string()),
            },
            tracked_games: Vec::new(),
            taskbar_hide_mode: TaskbarHideMode::HoverReveal,
            hide_taskbar_on_apply: true,
            cursor: CursorSettings {
                enabled: true,
                size: 72,
                ..CursorSettings::default()
            },
            update_mode: UpdateMode::Auto,
            log_ui_hangs: true,
        };

        let state = SettingsState::from_config(&config);

        assert_eq!(
            state.zone,
            Some(Zone {
                x: 10,
                y: 10,
                width: 200,
                height: 100
            })
        );
        assert_eq!(state.toggle_borderless, "Ctrl+Alt+B");
        assert_eq!(state.toggle_taskbar_autohide, "Ctrl+Alt+T");
        assert_eq!(state.toggle_game_cursor, "Ctrl+Alt+M");
        assert_eq!(state.taskbar_hide_mode, TaskbarHideMode::HoverReveal);
        assert_eq!(
            state.to_config(),
            config,
            "game cursor settings round-trip through Settings"
        );
    }

    #[test]
    fn a_combo_bound_to_any_other_hotkey_is_rejected() {
        let mut state = SettingsState::from_config(&Config::default());
        state.toggle_game_cursor = "Ctrl+Alt+M".to_string();
        state.start_capture(HotkeyField::ToggleBorderless);

        state.handle_key_event(
            egui::Modifiers {
                ctrl: true,
                alt: true,
                ..egui::Modifiers::NONE
            },
            egui::Key::M,
        );

        assert_eq!(state.toggle_borderless, "");
        assert_eq!(
            state.duplicate_hotkey_error,
            Some("Ctrl+Alt+M is already bound to another hotkey".to_string())
        );
    }

    #[test]
    fn the_game_cursor_hotkey_can_be_captured_and_cleared() {
        let mut state = SettingsState::from_config(&Config::default());
        state.start_capture(HotkeyField::ToggleGameCursor);

        state.handle_key_event(
            egui::Modifiers {
                ctrl: true,
                alt: true,
                ..egui::Modifiers::NONE
            },
            egui::Key::M,
        );
        assert_eq!(state.toggle_game_cursor, "Ctrl+Alt+M");
        assert_eq!(
            state.to_config().hotkeys.toggle_game_cursor,
            Some("Ctrl+Alt+M".to_string())
        );

        state.clear_hotkey(HotkeyField::ToggleGameCursor);
        assert_eq!(state.to_config().hotkeys.toggle_game_cursor, None);
    }

    #[test]
    fn log_ui_hangs_round_trips_through_the_settings_state() {
        for value in [true, false] {
            let config = Config {
                log_ui_hangs: value,
                ..Config::default()
            };
            let state = SettingsState::from_config(&config);
            assert_eq!(state.log_ui_hangs, value);
            assert_eq!(state.to_config().log_ui_hangs, value);
        }
    }

    #[test]
    fn update_mode_round_trips_through_the_settings_state() {
        for mode in [UpdateMode::Auto, UpdateMode::Ask] {
            let config = Config {
                update_mode: mode,
                ..Config::default()
            };

            let state = SettingsState::from_config(&config);

            assert_eq!(state.update_mode, mode);
            assert_eq!(state.to_config().update_mode, mode);
        }
    }

    #[test]
    fn to_config_round_trips_the_taskbar_hide_mode() {
        let mut state = SettingsState::from_config(&Config::default());
        state.taskbar_hide_mode = TaskbarHideMode::HoverReveal;

        let config = state.to_config();

        assert_eq!(config.taskbar_hide_mode, TaskbarHideMode::HoverReveal);
    }

    #[test]
    fn to_config_round_trips_bound_hotkeys() {
        let mut state = SettingsState::from_config(&Config::default());
        state.toggle_borderless = "Ctrl+Alt+B".to_string();
        state.toggle_taskbar_autohide = "Ctrl+Alt+D".to_string();

        let config = state.to_config();

        assert_eq!(
            config.hotkeys.toggle_borderless,
            Some("Ctrl+Alt+B".to_string())
        );
        assert_eq!(
            config.hotkeys.toggle_taskbar_autohide,
            Some("Ctrl+Alt+D".to_string())
        );
    }

    #[test]
    fn settings_opens_on_the_zone_tab_when_no_zone_exists() {
        let state = SettingsState::from_config(&Config::default());

        assert_eq!(state.tab, SettingsTab::Zone);
    }

    #[test]
    fn tabs_are_ordered_and_labelled() {
        let labels: Vec<&str> = SettingsTab::ALL.iter().map(|t| t.label()).collect();

        assert_eq!(
            labels,
            [
                "Games",
                "Cursor",
                "Zone, hotkeys & taskbar",
                "General",
                "About"
            ]
        );
    }

    #[test]
    fn every_tab_is_in_the_tab_bar() {
        // Exhaustive on purpose: a new variant fails to compile here until it
        // is listed, and then fails the test until it is in `ALL`.
        let every = |tab: SettingsTab| match tab {
            SettingsTab::Games
            | SettingsTab::Cursor
            | SettingsTab::Zone
            | SettingsTab::General
            | SettingsTab::About => tab,
        };
        for tab in [
            SettingsTab::Games,
            SettingsTab::Cursor,
            SettingsTab::Zone,
            SettingsTab::General,
            SettingsTab::About,
        ] {
            assert!(SettingsTab::ALL.contains(&every(tab)), "{tab:?}");
        }
    }

    #[test]
    fn settings_opens_on_the_games_tab_when_a_zone_exists() {
        let config = Config {
            zone: Some(Zone {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            }),
            ..Config::default()
        };

        let state = SettingsState::from_config(&config);

        assert_eq!(state.tab, SettingsTab::Games);
    }

    #[test]
    fn from_config_carries_the_tracked_games_list_through() {
        let mut config = Config::default();
        config
            .tracked_games
            .push(TrackedGame::new_for_cursor_setting(
                std::path::PathBuf::from("C:\\Games\\thing.exe"),
                "Thing".to_string(),
                true,
            ));

        let state = SettingsState::from_config(&config);

        assert_eq!(state.tracked_games, config.tracked_games);
    }

    #[test]
    fn to_config_carries_edited_tracked_games_through() {
        let mut state = SettingsState::from_config(&Config::default());
        state
            .tracked_games
            .push(TrackedGame::new_for_cursor_setting(
                std::path::PathBuf::from("C:\\Games\\thing.exe"),
                "Thing".to_string(),
                true,
            ));
        state.tracked_games[0].auto_apply = false;

        let config = state.to_config();

        assert_eq!(config.tracked_games.len(), 1);
        assert!(!config.tracked_games[0].auto_apply);
    }

    #[test]
    fn remove_tracked_game_removes_the_entry_at_the_given_index_without_confirmation() {
        let mut state = SettingsState::from_config(&Config::default());
        state
            .tracked_games
            .push(TrackedGame::new_for_cursor_setting(
                std::path::PathBuf::from("C:\\Games\\a.exe"),
                "A".to_string(),
                true,
            ));
        state
            .tracked_games
            .push(TrackedGame::new_for_cursor_setting(
                std::path::PathBuf::from("C:\\Games\\b.exe"),
                "B".to_string(),
                true,
            ));

        state.remove_tracked_game(0);

        assert_eq!(state.tracked_games.len(), 1);
        assert_eq!(state.tracked_games[0].display_name, "B");
    }

    #[test]
    fn to_config_treats_a_blank_hotkey_field_as_unbound() {
        let mut state = SettingsState::from_config(&Config::default());
        state.toggle_borderless = "   ".to_string();

        let config = state.to_config();

        assert_eq!(config.hotkeys.toggle_borderless, None);
    }

    #[test]
    fn zone_round_trips_through_from_config_and_to_config() {
        let zone = Zone {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        };
        let config = Config {
            zone: Some(zone),
            ..Config::default()
        };

        let round_tripped = SettingsState::from_config(&config).to_config();

        assert_eq!(round_tripped.zone, Some(zone));
        assert_eq!(
            SettingsState::from_config(&Config::default())
                .to_config()
                .zone,
            None
        );
    }

    #[test]
    fn set_zone_sets_the_zone_returned_by_the_zone_editor_overlay() {
        let mut state = SettingsState::from_config(&Config::default());

        state.set_zone(Zone {
            x: 100,
            y: 50,
            width: 800,
            height: 600,
        });

        assert_eq!(
            state.zone,
            Some(Zone {
                x: 100,
                y: 50,
                width: 800,
                height: 600
            })
        );
        assert_eq!(
            state.to_config().zone,
            Some(Zone {
                x: 100,
                y: 50,
                width: 800,
                height: 600
            })
        );
    }

    #[test]
    fn set_zone_replaces_any_previously_set_zone_rather_than_appending() {
        let mut state = SettingsState::from_config(&Config::default());
        state.zone = Some(Zone {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        });

        state.set_zone(Zone {
            x: 200,
            y: 200,
            width: 300,
            height: 300,
        });

        assert_eq!(
            state.zone,
            Some(Zone {
                x: 200,
                y: 200,
                width: 300,
                height: 300
            })
        );
    }

    #[test]
    fn clear_zone_removes_the_zone() {
        let mut state = SettingsState::from_config(&Config::default());
        state.zone = Some(Zone {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        });

        state.clear_zone();

        assert_eq!(state.zone, None);
    }

    #[test]
    fn handle_key_event_while_capturing_a_field_sets_that_fields_binding_and_stops_listening() {
        let mut state = SettingsState::from_config(&Config::default());
        state.start_capture(HotkeyField::ToggleBorderless);

        let modifiers = egui::Modifiers {
            ctrl: true,
            alt: true,
            ..egui::Modifiers::NONE
        };
        state.handle_key_event(modifiers, egui::Key::B);

        assert_eq!(state.toggle_borderless, "Ctrl+Alt+B");
        assert_eq!(state.capturing, None);
    }

    #[test]
    fn handle_key_event_targets_the_taskbar_autohide_field_when_that_field_is_capturing() {
        let mut state = SettingsState::from_config(&Config::default());
        state.start_capture(HotkeyField::ToggleTaskbarAutohide);

        state.handle_key_event(egui::Modifiers::NONE, egui::Key::T);

        assert_eq!(state.toggle_taskbar_autohide, "T");
        assert_eq!(state.toggle_borderless, "");
        assert_eq!(state.capturing, None);
    }

    #[test]
    fn handle_key_event_with_escape_cancels_the_capture_without_changing_the_existing_binding() {
        let mut state = SettingsState::from_config(&Config::default());
        state.toggle_borderless = "Ctrl+Alt+B".to_string();
        state.start_capture(HotkeyField::ToggleBorderless);

        state.handle_key_event(egui::Modifiers::NONE, egui::Key::Escape);

        assert_eq!(state.toggle_borderless, "Ctrl+Alt+B");
        assert_eq!(state.capturing, None);
    }

    #[test]
    fn complete_capture_with_ctrl_escape_if_suppressed_records_the_combo_when_the_hook_swallowed_it()
     {
        // `keyboard_hook::keyboard_hook_proc` swallows the physical
        // Ctrl+Escape press before it can ever arrive as an
        // `egui::Event::Key`, so this is the only path that can complete the
        // capture for that specific combo.
        let mut state = SettingsState::from_config(&Config::default());
        state.start_capture(HotkeyField::ToggleBorderless);

        state.complete_capture_with_ctrl_escape_if_suppressed(true);

        assert_eq!(state.toggle_borderless, "Ctrl+Escape");
        assert_eq!(state.capturing, None);
    }

    #[test]
    fn complete_capture_with_ctrl_escape_if_suppressed_is_a_no_op_when_the_hook_did_not_swallow_anything()
     {
        let mut state = SettingsState::from_config(&Config::default());
        state.start_capture(HotkeyField::ToggleBorderless);

        state.complete_capture_with_ctrl_escape_if_suppressed(false);

        assert_eq!(state.toggle_borderless, "");
        assert_eq!(state.capturing, Some(HotkeyField::ToggleBorderless));
    }

    #[test]
    fn complete_capture_with_ctrl_escape_if_suppressed_is_a_no_op_when_nothing_is_capturing() {
        let mut state = SettingsState::from_config(&Config::default());

        state.complete_capture_with_ctrl_escape_if_suppressed(true);

        assert_eq!(state.toggle_borderless, "");
        assert_eq!(state.capturing, None);
    }

    #[test]
    fn handle_key_event_with_ctrl_escape_records_the_combo_instead_of_cancelling() {
        let mut state = SettingsState::from_config(&Config::default());
        state.start_capture(HotkeyField::ToggleBorderless);

        let modifiers = egui::Modifiers {
            ctrl: true,
            ..egui::Modifiers::NONE
        };
        state.handle_key_event(modifiers, egui::Key::Escape);

        assert_eq!(state.toggle_borderless, "Ctrl+Escape");
        assert_eq!(state.capturing, None);
    }

    #[test]
    fn handle_key_event_with_only_a_modifier_key_keeps_listening_without_completing_the_capture() {
        let mut state = SettingsState::from_config(&Config::default());
        state.start_capture(HotkeyField::ToggleBorderless);

        let modifiers = egui::Modifiers {
            ctrl: true,
            ..egui::Modifiers::NONE
        };
        state.handle_key_event(modifiers, egui::Key::ControlLeft);

        assert_eq!(state.toggle_borderless, "");
        assert_eq!(state.capturing, Some(HotkeyField::ToggleBorderless));
    }

    #[test]
    fn handle_key_event_with_an_unmapped_key_keeps_listening_and_flags_it_as_unsupported() {
        let mut state = SettingsState::from_config(&Config::default());
        state.start_capture(HotkeyField::ToggleBorderless);

        // `Copy` has no entry in `key_display_name`'s vocabulary.
        state.handle_key_event(egui::Modifiers::NONE, egui::Key::Copy);

        assert_eq!(state.toggle_borderless, "");
        assert_eq!(state.capturing, Some(HotkeyField::ToggleBorderless));
        assert!(state.unsupported_key_pressed);
    }

    #[test]
    fn handle_key_event_when_nothing_is_capturing_is_a_no_op() {
        let mut state = SettingsState::from_config(&Config::default());

        state.handle_key_event(egui::Modifiers::NONE, egui::Key::B);

        assert_eq!(state.toggle_borderless, "");
        assert_eq!(state.capturing, None);
    }

    #[test]
    fn start_capture_clears_a_stale_unsupported_key_flag_from_a_previous_capture() {
        let mut state = SettingsState::from_config(&Config::default());
        state.start_capture(HotkeyField::ToggleBorderless);
        state.handle_key_event(egui::Modifiers::NONE, egui::Key::Copy);
        assert!(state.unsupported_key_pressed);

        state.start_capture(HotkeyField::ToggleTaskbarAutohide);

        assert!(!state.unsupported_key_pressed);
    }

    #[test]
    fn cancel_capture_resets_capturing_and_clears_unsupported_key_and_duplicate_error_flags() {
        let mut state = SettingsState::from_config(&Config::default());
        state.start_capture(HotkeyField::ToggleBorderless);
        state.unsupported_key_pressed = true;
        state.duplicate_hotkey_error =
            Some("Ctrl+Alt+B is already bound to another hotkey".to_string());

        state.cancel_capture();

        assert_eq!(state.capturing, None);
        assert!(!state.unsupported_key_pressed);
        assert_eq!(state.duplicate_hotkey_error, None);
    }

    #[test]
    fn clear_hotkey_unbinds_the_given_field_without_touching_the_other_field() {
        let mut state = SettingsState::from_config(&Config::default());
        state.toggle_borderless = "Ctrl+Alt+B".to_string();
        state.toggle_taskbar_autohide = "Ctrl+Alt+T".to_string();

        state.clear_hotkey(HotkeyField::ToggleBorderless);

        assert_eq!(state.toggle_borderless, "");
        assert_eq!(state.toggle_taskbar_autohide, "Ctrl+Alt+T");
    }

    #[test]
    fn handle_key_event_rejects_a_combo_already_bound_to_the_other_field() {
        let mut state = SettingsState::from_config(&Config::default());
        state.toggle_taskbar_autohide = "Ctrl+Alt+B".to_string();
        state.start_capture(HotkeyField::ToggleBorderless);

        let modifiers = egui::Modifiers {
            ctrl: true,
            alt: true,
            ..egui::Modifiers::NONE
        };
        state.handle_key_event(modifiers, egui::Key::B);

        assert_eq!(state.toggle_borderless, "");
        assert_eq!(state.capturing, Some(HotkeyField::ToggleBorderless));
        assert!(state.duplicate_hotkey_error.is_some());
    }

    #[test]
    fn handle_key_event_completing_a_capture_clears_a_stale_duplicate_error() {
        let mut state = SettingsState::from_config(&Config::default());
        state.toggle_taskbar_autohide = "Ctrl+Alt+B".to_string();
        state.start_capture(HotkeyField::ToggleBorderless);
        state.handle_key_event(
            egui::Modifiers {
                ctrl: true,
                alt: true,
                ..egui::Modifiers::NONE
            },
            egui::Key::B,
        );
        assert!(state.duplicate_hotkey_error.is_some());

        state.handle_key_event(egui::Modifiers::NONE, egui::Key::T);

        assert_eq!(state.toggle_borderless, "T");
        assert_eq!(state.duplicate_hotkey_error, None);
    }

    #[test]
    fn clear_hotkey_also_cancels_an_in_progress_capture_for_that_same_field() {
        let mut state = SettingsState::from_config(&Config::default());
        state.toggle_borderless = "Ctrl+Alt+B".to_string();
        state.start_capture(HotkeyField::ToggleBorderless);

        state.clear_hotkey(HotkeyField::ToggleBorderless);

        assert_eq!(state.toggle_borderless, "");
        assert_eq!(state.capturing, None);
    }

    #[test]
    fn clear_hotkey_does_not_cancel_a_capture_in_progress_for_the_other_field() {
        let mut state = SettingsState::from_config(&Config::default());
        state.toggle_borderless = "Ctrl+Alt+B".to_string();
        state.start_capture(HotkeyField::ToggleTaskbarAutohide);

        state.clear_hotkey(HotkeyField::ToggleBorderless);

        assert_eq!(state.toggle_borderless, "");
        assert_eq!(state.capturing, Some(HotkeyField::ToggleTaskbarAutohide));
    }

    #[test]
    fn saving_and_reopening_settings_reflects_the_previously_saved_values() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("GameFrame");

        // First "open": fresh config, defaults.
        let initial_config = profile_store::load(&config_dir).unwrap();
        let mut state = SettingsState::from_config(&initial_config);
        assert_eq!(state.toggle_borderless, "");

        // User edits and saves.
        state.toggle_borderless = "Ctrl+Alt+B".to_string();
        state.set_zone(Zone {
            x: 10,
            y: 10,
            width: 300,
            height: 200,
        });
        let to_persist = state.to_config();
        profile_store::save(&config_dir, &to_persist).unwrap();

        // "Reopen" Settings: load the saved config and rebuild the view-model.
        let reloaded_config = profile_store::load(&config_dir).unwrap();
        let reopened_state = SettingsState::from_config(&reloaded_config);

        assert_eq!(reopened_state.toggle_borderless, "Ctrl+Alt+B");
        assert_eq!(
            reopened_state.zone,
            Some(Zone {
                x: 10,
                y: 10,
                width: 300,
                height: 200
            })
        );
    }

    fn failed_report(action: HotkeyAction, binding: &str) -> BindingReport {
        BindingReport {
            action,
            binding: binding.to_string(),
            outcome: BindingOutcome::Failed("already in use by another program".to_string()),
        }
    }

    #[test]
    fn hotkey_failure_shows_the_reason_only_for_the_failed_binding() {
        let mut state = SettingsState::from_config(&Config::default());
        state.toggle_borderless = "Ctrl+Alt+B".to_string();
        state.toggle_taskbar_autohide = "Ctrl+Alt+T".to_string();
        state.set_hotkey_reports(&[
            failed_report(HotkeyAction::ToggleTaskbarAutohide, "Ctrl+Alt+T"),
            BindingReport {
                action: HotkeyAction::ToggleBorderless,
                binding: "Ctrl+Alt+B".to_string(),
                outcome: BindingOutcome::Registered,
            },
        ]);

        assert_eq!(
            state.hotkey_failure(HotkeyField::ToggleTaskbarAutohide),
            Some("already in use by another program")
        );
        assert_eq!(state.hotkey_failure(HotkeyField::ToggleBorderless), None);
        assert_eq!(state.hotkey_failure(HotkeyField::ToggleGameCursor), None);
    }

    #[test]
    fn hotkey_failure_clears_once_the_binding_is_changed_or_cleared() {
        let mut state = SettingsState::from_config(&Config::default());
        state.toggle_borderless = "Ctrl+Alt+B".to_string();
        state.set_hotkey_reports(&[failed_report(HotkeyAction::ToggleBorderless, "Ctrl+Alt+B")]);
        assert!(
            state
                .hotkey_failure(HotkeyField::ToggleBorderless)
                .is_some()
        );

        state.toggle_borderless = "Ctrl+Alt+F".to_string();
        assert_eq!(state.hotkey_failure(HotkeyField::ToggleBorderless), None);

        state.toggle_borderless = "Ctrl+Alt+B".to_string();
        state.clear_hotkey(HotkeyField::ToggleBorderless);
        assert_eq!(state.hotkey_failure(HotkeyField::ToggleBorderless), None);
    }
}
