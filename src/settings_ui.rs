//! Lazily-instantiated egui Settings window: create/edit presets and assign
//! GameFrame's two global hotkey bindings, persisted via [`crate::profile_store`].
//!
//! Split in two:
//! - [`SettingsState`]: the editable view-model, plain data with no egui/OS
//!   dependency, fully unit tested below.
//! - [`SettingsApp`] / [`run_settings_window`]: the actual egui/eframe
//!   window construction, rendering, and teardown. This is OS/GPU-facing --
//!   it needs a live desktop/GPU context to open and render a window, and a
//!   headless/CI sandbox can't provide one -- so, like [`crate::tray_ui::Tray::new`],
//!   it is NOT exercised by this crate's automated tests. The
//!   lazily-instantiate-on-open / fully-drop-on-close memory behavior in
//!   particular can only be confirmed by manual/usertest verification on a
//!   real desktop session.

use crate::profile_store::{self, Config, HotkeyBindings, ProfileStoreError};
use crate::region_calc::{resolve_region, MonitorGeometry, Preset};
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

/// The editable state backing the Settings window: the preset list, the two
/// hotkey binding strings (empty = unbound), and the custom-region input
/// fields plus any validation error from the last attempt to add one.
///
/// Plain data, no egui/OS dependency -- fully unit testable.
///
/// Deliberate MVP design decision: there is no in-place field editing of an
/// existing preset. "Editing" a preset means removing it ([`Self::remove_preset`])
/// and adding a replacement ([`Self::add_custom_preset`] /
/// [`Self::add_builtin_preset`]). This is intentional scope for this slice,
/// not an oversight -- a future slice may add true in-place editing of an
/// existing `Custom` preset's coordinates if that turns out to be needed.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsState {
    pub presets: Vec<Preset>,
    pub toggle_borderless: String,
    pub toggle_taskbar_autohide: String,
    pub custom_x: String,
    pub custom_y: String,
    pub custom_width: String,
    pub custom_height: String,
    pub custom_region_error: Option<String>,
}

impl SettingsState {
    /// Builds the editable state from a loaded/default [`Config`]. Unbound
    /// hotkeys (`None`) become empty strings; the custom-region input
    /// fields always start blank.
    pub fn from_config(config: &Config) -> Self {
        Self {
            presets: config.presets.clone(),
            toggle_borderless: config.hotkeys.toggle_borderless.clone().unwrap_or_default(),
            toggle_taskbar_autohide: config.hotkeys.toggle_taskbar_autohide.clone().unwrap_or_default(),
            custom_x: String::new(),
            custom_y: String::new(),
            custom_width: String::new(),
            custom_height: String::new(),
            custom_region_error: None,
        }
    }

    /// Builds the [`Config`] to persist. A blank (or whitespace-only) hotkey
    /// field is treated as unbound (`None`).
    pub fn to_config(&self) -> Config {
        Config {
            presets: self.presets.clone(),
            hotkeys: HotkeyBindings {
                toggle_borderless: non_empty(&self.toggle_borderless),
                toggle_taskbar_autohide: non_empty(&self.toggle_taskbar_autohide),
            },
        }
    }

    /// Parses the custom X/Y/width/height fields and validates the
    /// resulting region against `monitor` via
    /// [`crate::region_calc::resolve_region`]. On success, appends the
    /// `Preset::Custom` to `self.presets` and clears `custom_region_error`.
    /// On failure (unparsable input, or a region `resolve_region` rejects),
    /// leaves `self.presets` unchanged and sets `custom_region_error` to a
    /// user-visible message instead.
    pub fn add_custom_preset(&mut self, monitor: &MonitorGeometry) {
        match self.parse_custom_preset(monitor) {
            Ok(preset) => {
                self.presets.push(preset);
                self.custom_region_error = None;
                self.custom_x.clear();
                self.custom_y.clear();
                self.custom_width.clear();
                self.custom_height.clear();
            }
            Err(message) => {
                self.custom_region_error = Some(message);
            }
        }
    }

    fn parse_custom_preset(&self, monitor: &MonitorGeometry) -> Result<Preset, String> {
        let x = parse_field("X", &self.custom_x)?;
        let y = parse_field("Y", &self.custom_y)?;
        let width = parse_field("width", &self.custom_width)?;
        let height = parse_field("height", &self.custom_height)?;

        let preset = Preset::Custom { x, y, width, height };
        let region = resolve_region(&preset, monitor).map_err(|e| e.to_string())?;

        // `resolve_region` clamps an on-monitor-origin region that overflows the
        // monitor's far edge instead of erroring (region_calc's own contract --
        // see its module docs). settings_ui must not silently accept a clamped
        // result that differs from what the user entered, so treat clamping as
        // a rejection here rather than duplicating region_calc's math.
        if region.width != width || region.height != height {
            return Err("region extends beyond the monitor's bounds".to_string());
        }

        Ok(preset)
    }

    /// Appends a built-in (non-custom) preset -- `LeftHalf`, `RightHalf`, or
    /// `FullScreen` -- to the preset list. These never need region
    /// validation (unlike `Custom`, they're always well-formed relative to
    /// whatever monitor they're resolved against later).
    ///
    /// A builtin variant already present in the list is not appended again
    /// (repeated clicks on e.g. "Add Left Half" are a no-op past the first),
    /// since a builtin preset has no distinguishing data and a duplicate row
    /// would just be visual clutter. `Custom` presets are never passed here
    /// (see [`Self::add_custom_preset`]) and may legitimately have several
    /// entries with different coordinates.
    pub fn add_builtin_preset(&mut self, preset: Preset) {
        if !self.presets.contains(&preset) {
            self.presets.push(preset);
        }
    }

    /// Removes the preset at `index`. Out-of-range indices are silently
    /// ignored rather than panicking, since the index comes from UI state
    /// that could in principle be stale by the time a click is processed.
    pub fn remove_preset(&mut self, index: usize) {
        if index < self.presets.len() {
            self.presets.remove(index);
        }
    }
}

fn parse_field(name: &str, value: &str) -> Result<i32, String> {
    value.trim().parse::<i32>().map_err(|_| format!("{name} must be a whole number"))
}

/// A human-readable label for a preset, for use in the Settings preset list
/// UI -- deliberately not `Preset`'s `Debug` formatting, which is meant for
/// diagnostics, not end-user display.
fn preset_label(preset: &Preset) -> String {
    match preset {
        Preset::LeftHalf => "Left Half".to_string(),
        Preset::RightHalf => "Right Half".to_string(),
        Preset::FullScreen => "Full Screen".to_string(),
        Preset::Custom { x, y, width, height } => {
            format!("Custom ({x}, {y}, {width}x{height})")
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

/// Queries the primary monitor's geometry via a minimal Win32 call.
///
/// OS-facing -- not exercised by this crate's automated tests, for the same
/// reason as [`crate::tray_ui::Tray::new`]. MVP scope targets the
/// primary/foreground monitor only (see `region_calc` module docs); no
/// monitor-enumeration module exists.
pub fn primary_monitor_geometry() -> MonitorGeometry {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

    // Safety: `GetSystemMetrics` is a simple, side-effect-free Win32 query
    // that takes a plain integer index and returns a plain integer; no
    // pointers or lifetimes are involved.
    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    MonitorGeometry { x: 0, y: 0, width, height }
}

/// Reasons opening the Settings window or persisting its result can fail.
#[derive(Debug)]
pub enum SettingsUiError {
    Config(ProfileStoreError),
    Window(eframe::Error),
}

impl std::fmt::Display for SettingsUiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingsUiError::Config(e) => write!(f, "failed to load/save the config: {e}"),
            SettingsUiError::Window(e) => write!(f, "failed to open the Settings window: {e}"),
        }
    }
}

impl std::error::Error for SettingsUiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SettingsUiError::Config(e) => Some(e),
            SettingsUiError::Window(e) => Some(e),
        }
    }
}

/// Opens the Settings window, blocking until it's closed, then returns.
///
/// Loads the current config from `config_dir` (via [`profile_store::load`]),
/// lets the user edit presets and hotkey bindings, and -- only if the user
/// clicks "Save" -- persists the result via [`profile_store::save`] before
/// returning. Closing the window any other way (the "Cancel" button, the
/// window's own close button) discards the edits.
///
/// OS/GPU-facing: constructs a real egui/wgpu context for the lifetime of
/// this call and fully drops it before returning (the `eframe::run_native`
/// call below owns the entire window/render lifecycle). Not exercised by
/// this crate's automated tests -- see module docs.
pub fn run_settings_window(config_dir: &Path) -> Result<(), SettingsUiError> {
    let config = profile_store::load(config_dir).map_err(SettingsUiError::Config)?;
    let state = SettingsState::from_config(&config);
    let monitor = primary_monitor_geometry();

    let saved_config: Rc<RefCell<Option<Config>>> = Rc::new(RefCell::new(None));
    let saved_config_for_app = Rc::clone(&saved_config);

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("GameFrame Settings")
            .with_inner_size([440.0, 520.0]),
        ..Default::default()
    };

    eframe::run_native(
        "GameFrame Settings",
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(SettingsApp { state, monitor, saved_config: saved_config_for_app }))
        }),
    )
    .map_err(SettingsUiError::Window)?;

    let to_save = saved_config.borrow_mut().take();
    if let Some(config) = to_save {
        profile_store::save(config_dir, &config).map_err(SettingsUiError::Config)?;
    }

    Ok(())
}

/// The live egui app backing the Settings window. Torn down entirely when
/// [`run_settings_window`] returns -- nothing here outlives that call.
struct SettingsApp {
    state: SettingsState,
    monitor: MonitorGeometry,
    saved_config: Rc<RefCell<Option<Config>>>,
}

impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Presets");
            let mut to_remove = None;
            for (index, preset) in self.state.presets.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(preset_label(preset));
                    if ui.small_button("Remove").clicked() {
                        to_remove = Some(index);
                    }
                });
            }
            if let Some(index) = to_remove {
                self.state.remove_preset(index);
            }

            ui.horizontal(|ui| {
                if ui.button("Add Left Half").clicked() {
                    self.state.add_builtin_preset(Preset::LeftHalf);
                }
                if ui.button("Add Right Half").clicked() {
                    self.state.add_builtin_preset(Preset::RightHalf);
                }
                if ui.button("Add Full Screen").clicked() {
                    self.state.add_builtin_preset(Preset::FullScreen);
                }
            });

            ui.separator();
            ui.label("Add custom preset (X, Y, width, height):");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.state.custom_x).desired_width(60.0));
                ui.add(egui::TextEdit::singleline(&mut self.state.custom_y).desired_width(60.0));
                ui.add(egui::TextEdit::singleline(&mut self.state.custom_width).desired_width(60.0));
                ui.add(egui::TextEdit::singleline(&mut self.state.custom_height).desired_width(60.0));
                if ui.button("Add").clicked() {
                    self.state.add_custom_preset(&self.monitor);
                }
            });
            if let Some(error) = &self.state.custom_region_error {
                ui.colored_label(egui::Color32::RED, error);
            }

            ui.separator();
            ui.heading("Global hotkeys");
            ui.horizontal(|ui| {
                ui.label("Toggle borderless:");
                ui.text_edit_singleline(&mut self.state.toggle_borderless);
            });
            ui.horizontal(|ui| {
                ui.label("Toggle taskbar autohide:");
                ui.text_edit_singleline(&mut self.state.toggle_taskbar_autohide);
            });

            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    *self.saved_config.borrow_mut() = Some(self.state.to_config());
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if ui.button("Cancel").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::region_calc::RegionCalcError;

    fn test_monitor() -> MonitorGeometry {
        MonitorGeometry { x: 0, y: 0, width: 1920, height: 1080 }
    }

    #[test]
    fn from_config_of_a_fresh_default_config_shows_unbound_hotkeys_and_builtin_presets() {
        let config = Config::default();

        let state = SettingsState::from_config(&config);

        assert_eq!(state.presets, vec![Preset::LeftHalf, Preset::RightHalf, Preset::FullScreen]);
        assert_eq!(state.toggle_borderless, "");
        assert_eq!(state.toggle_taskbar_autohide, "");
        assert_eq!(state.custom_region_error, None);
    }

    #[test]
    fn from_config_of_a_saved_config_shows_the_saved_hotkeys_and_presets() {
        let config = Config {
            presets: vec![Preset::FullScreen, Preset::Custom { x: 10, y: 10, width: 200, height: 100 }],
            hotkeys: HotkeyBindings {
                toggle_borderless: Some("Ctrl+Alt+B".to_string()),
                toggle_taskbar_autohide: Some("Ctrl+Alt+T".to_string()),
            },
        };

        let state = SettingsState::from_config(&config);

        assert_eq!(state.presets, config.presets);
        assert_eq!(state.toggle_borderless, "Ctrl+Alt+B");
        assert_eq!(state.toggle_taskbar_autohide, "Ctrl+Alt+T");
    }

    #[test]
    fn to_config_round_trips_bound_hotkeys() {
        let mut state = SettingsState::from_config(&Config::default());
        state.toggle_borderless = "Ctrl+Alt+B".to_string();
        state.toggle_taskbar_autohide = "Ctrl+Alt+D".to_string();

        let config = state.to_config();

        assert_eq!(config.hotkeys.toggle_borderless, Some("Ctrl+Alt+B".to_string()));
        assert_eq!(config.hotkeys.toggle_taskbar_autohide, Some("Ctrl+Alt+D".to_string()));
    }

    #[test]
    fn to_config_treats_a_blank_hotkey_field_as_unbound() {
        let mut state = SettingsState::from_config(&Config::default());
        state.toggle_borderless = "   ".to_string();

        let config = state.to_config();

        assert_eq!(config.hotkeys.toggle_borderless, None);
    }

    #[test]
    fn add_custom_preset_with_a_valid_region_appends_it_and_clears_any_error() {
        let mut state = SettingsState::from_config(&Config::default());
        state.custom_region_error = Some("stale error".to_string());
        state.custom_x = "100".to_string();
        state.custom_y = "50".to_string();
        state.custom_width = "800".to_string();
        state.custom_height = "600".to_string();

        state.add_custom_preset(&test_monitor());

        assert_eq!(state.custom_region_error, None);
        assert_eq!(
            state.presets.last(),
            Some(&Preset::Custom { x: 100, y: 50, width: 800, height: 600 })
        );
    }

    #[test]
    fn add_custom_preset_with_a_negative_position_is_rejected_with_a_visible_error() {
        let mut state = SettingsState::from_config(&Config::default());
        let original_presets = state.presets.clone();
        state.custom_x = "-10".to_string();
        state.custom_y = "0".to_string();
        state.custom_width = "800".to_string();
        state.custom_height = "600".to_string();

        state.add_custom_preset(&test_monitor());

        assert_eq!(state.presets, original_presets);
        assert_eq!(state.custom_region_error, Some(RegionCalcError::OutOfBounds.to_string()));
    }

    #[test]
    fn add_custom_preset_with_a_zero_or_negative_dimension_is_rejected_with_a_visible_error() {
        let mut state = SettingsState::from_config(&Config::default());
        let original_presets = state.presets.clone();
        state.custom_x = "0".to_string();
        state.custom_y = "0".to_string();
        state.custom_width = "0".to_string();
        state.custom_height = "600".to_string();

        state.add_custom_preset(&test_monitor());

        assert_eq!(state.presets, original_presets);
        assert_eq!(state.custom_region_error, Some(RegionCalcError::InvalidDimensions.to_string()));
    }

    #[test]
    fn add_custom_preset_that_would_be_clamped_by_resolve_region_is_rejected_with_a_visible_error() {
        // On-monitor origin, but the entered width/height overflow the monitor's far
        // edge -- region_calc::resolve_region would clamp this to a smaller region
        // rather than erroring. settings_ui must treat that clamping as a rejection,
        // not silently accept a region different from what the user entered.
        let mut state = SettingsState::from_config(&Config::default());
        let original_presets = state.presets.clone();
        state.custom_x = "1000".to_string();
        state.custom_y = "900".to_string();
        state.custom_width = "5000".to_string();
        state.custom_height = "5000".to_string();

        state.add_custom_preset(&test_monitor());

        assert_eq!(state.presets, original_presets);
        assert!(state.custom_region_error.is_some());
    }

    #[test]
    fn add_custom_preset_with_non_numeric_input_is_rejected_with_a_visible_error() {
        let mut state = SettingsState::from_config(&Config::default());
        let original_presets = state.presets.clone();
        state.custom_x = "not-a-number".to_string();
        state.custom_y = "0".to_string();
        state.custom_width = "800".to_string();
        state.custom_height = "600".to_string();

        state.add_custom_preset(&test_monitor());

        assert_eq!(state.presets, original_presets);
        assert!(state.custom_region_error.is_some());
    }

    #[test]
    fn add_builtin_preset_appends_a_not_yet_present_preset_without_touching_custom_region_fields() {
        let mut state = SettingsState::from_config(&Config::default());
        state.presets.clear();

        state.add_builtin_preset(Preset::LeftHalf);

        assert_eq!(state.presets, vec![Preset::LeftHalf]);
    }

    #[test]
    fn add_builtin_preset_does_not_append_a_duplicate_of_a_builtin_variant_already_present() {
        let mut state = SettingsState::from_config(&Config::default());

        state.add_builtin_preset(Preset::LeftHalf);

        assert_eq!(state.presets, vec![Preset::LeftHalf, Preset::RightHalf, Preset::FullScreen]);
    }

    #[test]
    fn remove_preset_removes_the_entry_at_the_given_index() {
        let mut state = SettingsState::from_config(&Config::default());

        state.remove_preset(1);

        assert_eq!(state.presets, vec![Preset::LeftHalf, Preset::FullScreen]);
    }

    #[test]
    fn remove_preset_with_an_out_of_range_index_does_not_panic_or_change_the_list() {
        let mut state = SettingsState::from_config(&Config::default());
        let original = state.presets.clone();

        state.remove_preset(99);

        assert_eq!(state.presets, original);
    }

    #[test]
    fn add_custom_preset_with_a_valid_region_clears_the_input_fields_so_add_is_not_repeatable() {
        let mut state = SettingsState::from_config(&Config::default());
        state.custom_x = "100".to_string();
        state.custom_y = "50".to_string();
        state.custom_width = "800".to_string();
        state.custom_height = "600".to_string();

        state.add_custom_preset(&test_monitor());

        assert_eq!(state.custom_x, "");
        assert_eq!(state.custom_y, "");
        assert_eq!(state.custom_width, "");
        assert_eq!(state.custom_height, "");
    }

    #[test]
    fn preset_label_renders_human_readable_names_instead_of_debug_formatting() {
        assert_eq!(preset_label(&Preset::LeftHalf), "Left Half");
        assert_eq!(preset_label(&Preset::RightHalf), "Right Half");
        assert_eq!(preset_label(&Preset::FullScreen), "Full Screen");
        assert_eq!(
            preset_label(&Preset::Custom { x: 10, y: 20, width: 300, height: 400 }),
            "Custom (10, 20, 300x400)"
        );
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
        state.custom_x = "10".to_string();
        state.custom_y = "10".to_string();
        state.custom_width = "300".to_string();
        state.custom_height = "200".to_string();
        state.add_custom_preset(&test_monitor());
        let to_persist = state.to_config();
        profile_store::save(&config_dir, &to_persist).unwrap();

        // "Reopen" Settings: reload from disk and rebuild the view-model.
        let reloaded_config = profile_store::load(&config_dir).unwrap();
        let reopened_state = SettingsState::from_config(&reloaded_config);

        assert_eq!(reopened_state.toggle_borderless, "Ctrl+Alt+B");
        assert!(reopened_state.presets.contains(&Preset::Custom {
            x: 10,
            y: 10,
            width: 300,
            height: 200
        }));
    }
}
