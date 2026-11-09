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
mod exe_icon;
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
    /// What the edits are compared against; moved by each successful Apply and
    /// shared with [`run_settings_window`], which keeps it across the zone
    /// editor's close-then-reopen.
    baseline: Rc<RefCell<Baseline>>,
    /// Whether the last Apply succeeded (shown as a note by the buttons until
    /// the next edit).
    applied_note: bool,
    /// The status message the last failed Apply put in
    /// [Self::pending_status_messages], so the next Apply can retire it
    /// without touching the live re-apply messages.
    apply_error: Option<String>,
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
    /// Games tab icons, extracted once per exe path.
    exe_icons: exe_icon::ExeIcons,
}

/// Size of the Save and Exit / Apply / Close buttons, in points.
const BUTTON_WIDTH: f32 = 110.0;
const BUTTON_HEIGHT: f32 = 32.0;
/// Font size of the version label next to the buttons, in points.
const VERSION_LABEL_SIZE: f32 = 14.0;

impl SettingsApp<'_> {
    /// Whether the edits differ from what the last Apply (or opening) left.
    fn edited_since_apply(&self) -> bool {
        let baseline = self.baseline.borrow();
        self.state.to_config() != baseline.config
            || self.state.start_with_windows != baseline.start_with_windows
    }

    /// Apply: persists the edits (see [`apply_settings`]) and tells the tray
    /// process to refresh the hotkeys and game cursor, with the window left
    /// open. Returns whether it succeeded; a failure shows its notice on the
    /// status line and leaves the baseline unchanged.
    ///
    /// Blocks the frame only for the config write and the autostart registry
    /// write; the tray-side refresh is a fire-and-forget message.
    fn apply(&mut self) -> bool {
        let result = apply_settings(
            &self.config_dir,
            &mut self.baseline.borrow_mut(),
            &mut self.state,
            self.on_tick,
            &mut |enabled| crate::autostart::set_enabled(enabled),
        );
        match result {
            Ok(()) => {
                if let Some(previous) = self.apply_error.take() {
                    self.pending_status_messages.retain(|m| *m != previous);
                }
                self.applied_note = true;
                true
            }
            Err(e) => {
                log!("failed to apply the settings: {e}");
                let notice = e.notice().to_string();
                if let Some(previous) = self.apply_error.replace(notice.clone()) {
                    self.pending_status_messages.retain(|m| *m != previous);
                }
                self.pending_status_messages.push(notice);
                self.applied_note = false;
                false
            }
        }
    }
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

        // Save and Exit / Apply / Close stay pinned below whichever tab is
        // open, laid out from the right; the version label sits on the left.
        egui::Panel::bottom("settings_buttons").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(crate::version::current_label())
                        .weak()
                        .size(VERSION_LABEL_SIZE),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let size = egui::vec2(BUTTON_WIDTH, BUTTON_HEIGHT);
                    let save_and_exit = egui::Button::new(
                        egui::RichText::new("Save and Exit").color(egui::Color32::WHITE),
                    )
                    .fill(ui.visuals().selection.bg_fill)
                    .min_size(size);
                    if ui.add(save_and_exit).clicked() && self.apply() {
                        *self.exit.borrow_mut() = Some(SettingsExit::Closed);
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if ui.add(egui::Button::new("Apply").min_size(size)).clicked() {
                        self.apply();
                    }
                    if ui.add(egui::Button::new("Close").min_size(size)).clicked() {
                        *self.exit.borrow_mut() = Some(SettingsExit::Closed);
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if self.applied_note && !self.edited_since_apply() {
                        ui.label(egui::RichText::new("Applied").weak());
                    }
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
                for tab in SettingsTab::ALL {
                    ui.selectable_value(&mut self.state.tab, tab, tab.label());
                }
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
                    SettingsTab::Zone => self.zone_tab(ui),
                    SettingsTab::General => self.general_section(ui),
                    SettingsTab::About => self.about_tab(ui),
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
