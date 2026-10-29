//! Lazily-instantiated egui Settings window: draw/edit the single optional
//! Zone, and assign GameFrame's three global hotkey bindings, persisted via
//! [`crate::profile_store`].
//!
//! Split in two:
//! - [`SettingsState`]: the editable view-model, plain data with no egui/OS
//!   dependency, fully unit tested in `state`. [`zone_view`]/`zone::normalize_region`
//!   are pure helpers that turn that state into the data a thumbnail painter
//!   needs, also fully unit tested.
//! - [`SettingsApp`] / [`run_settings_window`]: the actual egui/eframe
//!   window construction, rendering, and teardown. This is OS/GPU-facing --
//!   it needs a live desktop/GPU context to open and render a window, and a
//!   headless/CI sandbox can't provide one -- so, like [`crate::tray_ui::Tray::new`],
//!   it is NOT exercised by this crate's automated tests. The
//!   lazily-instantiate-on-open / fully-drop-on-close memory behavior in
//!   particular can only be confirmed by manual/usertest verification on a
//!   real desktop session.
//!
//! Submodules: `state` ([`SettingsState`]), `window` (the eframe window per
//! session), `zone`, `hotkeys` (capture), `save` (merging edits, persisting
//! the Zone, live re-apply), `updates`, `cursor` and `tabs` (the Games and
//! General sections).

mod cursor;
mod hotkeys;
mod save;
mod state;
mod tabs;
mod updates;
mod window;
mod zone;

use cursor::*;
use hotkeys::*;
pub(crate) use save::*;
pub use state::*;
use updates::*;
pub use window::*;
pub use zone::*;

use crate::autostart::AutostartError;
use crate::keyboard_hook;
use crate::profile_store::ProfileStoreError;
use crate::region_calc::MonitorGeometry;
use std::cell::RefCell;
use std::rc::Rc;

/// Reasons opening the Settings window or persisting its result can fail.
#[derive(Debug)]
pub enum SettingsUiError {
    Config(ProfileStoreError),
    Window(eframe::Error),
    Autostart(AutostartError),
}

impl std::fmt::Display for SettingsUiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingsUiError::Config(e) => write!(f, "failed to load/save the config: {e}"),
            SettingsUiError::Window(e) => write!(f, "failed to open the Settings window: {e}"),
            SettingsUiError::Autostart(e) => write!(f, "failed to change Start with Windows: {e}"),
        }
    }
}

impl SettingsUiError {
    /// The plain-language notice for the user when Settings could not open or
    /// its Save did not go through (the technical detail stays in the log).
    pub(crate) fn notice(&self) -> &'static str {
        match self {
            SettingsUiError::Config(_) => {
                "Settings couldn't read or write GameFrame's settings file, so your changes may not have been saved. See the log."
            }
            SettingsUiError::Window(_) => "Settings couldn't open its window. See the log.",
            SettingsUiError::Autostart(_) => {
                "Your settings were saved, but Start with Windows couldn't be changed. See the log."
            }
        }
    }
}

impl std::error::Error for SettingsUiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SettingsUiError::Config(e) => Some(e),
            SettingsUiError::Window(e) => Some(e),
            SettingsUiError::Autostart(e) => Some(e),
        }
    }
}

/// The live egui app backing the Settings window. Torn down entirely when
/// [`run_settings_window`] returns -- nothing here outlives that call.
///
/// Also owns the transient live-re-apply status messages
/// ([`pending_status_messages`](SettingsApp::pending_status_messages)),
/// which -- unlike `state` -- deliberately don't survive a
/// [`SettingsExit::EditZone`] round trip unchanged: they're carried
/// forward by [`run_settings_window`] as plain `Vec<String>` data, not as
/// part of this struct, since this struct itself is fully torn down and
/// reconstructed by `window::run_settings_frame` on each session.
struct SettingsApp<'a> {
    /// Called once per frame (at least every [`crate::instance_channel::QUIT_POLL`]) to keep the tray
    /// process informed of which window is ticking (see
    /// [`crate::settings_link::ChildLink::tick`]). Returns the new
    /// `cursor.enabled` when the tray process's hotkey toggled it since the
    /// last call.
    on_tick: &'a mut dyn FnMut(TickContext) -> Option<bool>,
    /// Where the Zone is persisted immediately on Clear (the overlay-Save
    /// persist lives in [`run_settings_window`]).
    config_dir: std::path::PathBuf,
    state: SettingsState,
    monitor: MonitorGeometry,
    exit: Rc<RefCell<Option<SettingsExit>>>,
    /// Zero or more non-blocking status messages from the most recent
    /// commit-triggered live re-apply attempt -- e.g.
    /// `save::live_apply_failure_message` for a tracked game whose window
    /// couldn't be repositioned. Purely transient UI feedback: not part of
    /// [`SettingsState`] (never round-trips through `to_config`/persist),
    /// and replaced wholesale -- not accumulated -- on each new
    /// commit-triggered attempt.
    pending_status_messages: Vec<String>,
    /// `Some(guard)` while `state.capturing` is `Some` (kept in sync each
    /// frame by [`Self::sync_keyboard_hook`]) -- suppresses the physical
    /// Ctrl+Escape combination so Windows doesn't steal it for the Start
    /// menu while a hotkey capture is listening. `None`
    /// otherwise; dropping it (either via that sync, or via `SettingsApp`
    /// itself being torn down when the Settings window closes) uninstalls
    /// the hook, per [`keyboard_hook::KeyboardHookGuard`]'s RAII contract.
    keyboard_hook: Option<keyboard_hook::KeyboardHookGuard>,
    /// Textures for the "Game cursor" section, re-rendered only when the
    /// cursor settings change (see [`Self::refresh_cursor_previews`]).
    cursor_previews: Option<CursorPreviews>,
    /// The "Check for updates now" button's state; dropped with the window.
    update_check: UpdateCheck,
    /// The "Install update" button's state, once a check found an update.
    update_install: UpdateInstall,
}

impl eframe::App for SettingsApp<'_> {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Heartbeat for the hang watchdog; idle again when the frame is built.
        // `on_tick` (tray, hotkey and update events) runs below, inside this scope.
        crate::watchdog::beat();
        let _idle = crate::watchdog::IdleOnDrop;
        self.sync_keyboard_hook();
        // The flag tells the app to swallow hotkey presses during a capture,
        // so pressing a bound combo to rebind it doesn't also run its action.
        // A game-cursor hotkey press while Settings is open changed
        // `cursor.enabled` on disk: show it in the checkbox so Save keeps it.
        if let Some(enabled) = (self.on_tick)(self.state.tick_context()) {
            self.state.apply_external_cursor_enabled(enabled);
        }

        // A Quit (installer/updater) closes Settings as Cancel does. Polled
        // here, not only via the window procedure's WM_CLOSE, so a Quit that
        // arrived before this window existed is not lost.
        if crate::instance_channel::quit_pending() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ui.ctx()
            .request_repaint_after(crate::instance_channel::QUIT_POLL);

        // Save/Cancel stay pinned below whichever tab is open.
        egui::Panel::bottom("settings_buttons").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    *self.exit.borrow_mut() = Some(save_exit(&mut self.state, self.on_tick));
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if ui.button("Cancel").clicked() {
                    *self.exit.borrow_mut() = Some(SettingsExit::Cancelled);
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(crate::version::current_label())
                            .weak()
                            .small(),
                    );
                });
            });
            ui.add_space(2.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            // Non-blocking status line(s): zero or more messages
            // from the most recent commit-triggered live re-apply attempt.
            // Rendered above the tabs so they're visible whichever tab is
            // open.
            if !self.pending_status_messages.is_empty() {
                for message in &self.pending_status_messages {
                    ui.colored_label(ui.visuals().warn_fg_color, message);
                }
                ui.separator();
            }

            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.state.tab, SettingsTab::Games, "Games");
                ui.selectable_value(&mut self.state.tab, SettingsTab::Cursor, "Cursor");
                ui.selectable_value(
                    &mut self.state.tab,
                    SettingsTab::General,
                    "Zone, hotkeys & taskbar",
                );
                ui.selectable_value(&mut self.state.tab, SettingsTab::About, "About");
            });
            ui.separator();

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match self.state.tab {
                    SettingsTab::Games | SettingsTab::Cursor => {
                        let enabled = zone_dependent_tabs_enabled(self.state.zone.as_ref());
                        if !enabled {
                            ui.colored_label(ui.visuals().warn_fg_color, ZONE_REQUIRED_NOTE);
                            ui.add_space(4.0);
                        }
                        ui.add_enabled_ui(enabled, |ui| {
                            if self.state.tab == SettingsTab::Games {
                                self.games_section(ui);
                            } else {
                                self.cursor_section(ui);
                            }
                        });
                    }
                    SettingsTab::General => self.general_section(ui),
                    SettingsTab::About => crate::about_ui::about_section(ui),
                });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_errors_map_to_distinct_plain_language_notices() {
        let save = SettingsUiError::Config(ProfileStoreError::Io(std::io::Error::other("disk")));
        let autostart =
            SettingsUiError::Autostart(AutostartError::CurrentExe(std::io::Error::other("exe")));

        assert!(save.notice().contains("settings file"));
        assert!(autostart.notice().contains("Start with Windows"));
        assert_ne!(save.notice(), autostart.notice());
    }
}
