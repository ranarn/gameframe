//! The Games, Zone, General and About tabs.

use super::{HotkeyField, SettingsApp};
use crate::profile_store::{TaskbarHideMode, UpdateMode};
use crate::ui_helpers::section_separator;

impl SettingsApp<'_> {
    /// Renders the "Games" section: one row per tracked game with its
    /// display name, an auto-apply checkbox,
    /// and an immediate no-confirmation "Remove" button (see
    /// [`super::SettingsState::remove_tracked_game`]).
    ///
    /// Removal is deferred until after the loop (rather than mutating
    /// `self.state.tracked_games` while iterating it) to avoid mutating the
    /// `Vec` out from under `iter_mut`'s in-progress borrow.
    pub(super) fn games_section(&mut self, ui: &mut egui::Ui) {
        if self.state.tracked_games.is_empty() {
            ui.label(
                "No tracked games yet -- use \"Apply to Window\" from the tray menu to add one.",
            );
            return;
        }

        let mut to_remove: Option<usize> = None;
        let last_index = self.state.tracked_games.len() - 1;
        for (index, game) in self.state.tracked_games.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.label(&game.display_name);

                ui.checkbox(&mut game.auto_apply, "Auto-apply");
                ui.checkbox(&mut game.game_cursor, "Game cursor");

                if ui.button("Remove").clicked() {
                    to_remove = Some(index);
                }
            });

            if index != last_index {
                ui.separator();
            }
        }

        if let Some(index) = to_remove {
            self.state.remove_tracked_game(index);
        }
    }

    /// Renders the "Zone, hotkeys & taskbar" tab: Zone, global hotkeys and
    /// taskbar hiding.
    pub(super) fn zone_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Zone");
        self.zone_section(ui);
        section_separator(ui);
        ui.heading("Global hotkeys");
        self.hotkey_row(ui, "Apply / restore Zone:", HotkeyField::ToggleBorderless);
        self.hotkey_row(
            ui,
            "Toggle taskbar auto-hide:",
            HotkeyField::ToggleTaskbarAutohide,
        );
        self.hotkey_row(ui, "Toggle game cursor:", HotkeyField::ToggleGameCursor);
        // Re-sync immediately after the hotkey rows: a "Set hotkey" click
        // just above sets `state.capturing = Some(..)` within this same
        // `ui()` call, but the sync at the top of `ui()` already ran against
        // the *old* `None` state. Without this second call the hook wouldn't
        // install until the following frame, leaving a one-frame window
        // where physical Ctrl+Escape still reaches Windows' Start-menu
        // shortcut.
        self.sync_keyboard_hook();

        section_separator(ui);
        ui.heading("Taskbar hiding");
        ui.checkbox(
            &mut self.state.hide_taskbar_on_apply,
            "Hide taskbar when the Zone is applied",
        );
        if self.state.missing_taskbar_hotkey_warning() {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(
                        "No hotkey set for Toggle taskbar auto-hide \u{2014} set one so you can always bring the taskbar back",
                    )
                    .color(ui.visuals().warn_fg_color),
                )
                .wrap(),
            );
        }
        ui.add_enabled_ui(self.state.hide_taskbar_on_apply, |ui| {
            ui.radio_value(
                &mut self.state.taskbar_hide_mode,
                TaskbarHideMode::FullyHidden,
                "Fully hidden (no hover-reveal)",
            );
            ui.radio_value(
                &mut self.state.taskbar_hide_mode,
                TaskbarHideMode::HoverReveal,
                "Hover to reveal (classic Windows behavior)",
            );
        });
    }

    /// Renders the "General" tab: Startup and Diagnostics.
    pub(super) fn general_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Startup");
        ui.checkbox(&mut self.state.start_with_windows, "Start with Windows");

        section_separator(ui);
        ui.heading("Diagnostics");
        ui.checkbox(&mut self.state.log_ui_hangs, "Log UI hangs (diagnostics)");
        ui.label(
            "Writes a line to the log when GameFrame stops responding for a few seconds. Takes effect when you apply or save your changes; Close keeps the old state.",
        );
    }

    /// Renders the "About" tab: the about info, then the Updates section.
    pub(super) fn about_tab(&mut self, ui: &mut egui::Ui) {
        crate::about_ui::about_section(ui);

        section_separator(ui);
        ui.heading("Updates");
        ui.radio_value(
            &mut self.state.update_mode,
            UpdateMode::Auto,
            "Install updates automatically at startup",
        );
        ui.radio_value(
            &mut self.state.update_mode,
            UpdateMode::Ask,
            "Ask me (adds a tray item when an update is available)",
        );
        self.update_check_row(ui);
    }
}
