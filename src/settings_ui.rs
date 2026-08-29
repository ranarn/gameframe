//! Lazily-instantiated egui Settings window: adjust the depth-from-edge of
//! the `Left`/`Right` base presets, set/clear the single optional `Custom`
//! preset, and assign GameFrame's two global hotkey bindings, persisted via
//! [`crate::profile_store`].
//!
//! Split in two:
//! - [`SettingsState`]: the editable view-model, plain data with no egui/OS
//!   dependency, fully unit tested below. [`preset_thumbnails`]/
//!   [`normalize_region`] are pure helpers that turn that state into the data
//!   a thumbnail painter needs, also fully unit tested.
//! - [`SettingsApp`] / [`run_settings_window`]: the actual egui/eframe
//!   window construction, rendering, and teardown. This is OS/GPU-facing --
//!   it needs a live desktop/GPU context to open and render a window, and a
//!   headless/CI sandbox can't provide one -- so, like [`crate::tray_ui::Tray::new`],
//!   it is NOT exercised by this crate's automated tests. The
//!   lazily-instantiate-on-open / fully-drop-on-close memory behavior in
//!   particular can only be confirmed by manual/usertest verification on a
//!   real desktop session.

use crate::profile_store::{self, Config, CustomRegion, HotkeyBindings, Presets, ProfileStoreError};
use crate::region_calc::{
    resolve_region, MonitorGeometry, Preset, Region, MAX_DEPTH_PERCENT, MIN_DEPTH_PERCENT,
};
use crate::win32;
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

/// The editable state backing the Settings window: the `Left`/`Right`
/// depth-from-edge percentages, the single optional `Custom` preset, the two
/// hotkey binding strings (empty = unbound), and the custom-region input
/// fields plus any validation error from the last attempt to set one.
///
/// Plain data, no egui/OS dependency -- fully unit testable.
///
/// The four base presets (`Left`, `Right`, `HalfLeft`, `HalfRight`) are
/// always present and are never added/removed -- only `Left`/`Right`'s
/// depth-from-edge is adjustable, edited in place. `Custom` is the single
/// optional preset: setting it while one already exists replaces it, it is
/// never appended to a list.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsState {
    pub left_depth_percent: u8,
    pub right_depth_percent: u8,
    pub custom: Option<CustomRegion>,
    pub toggle_borderless: String,
    pub toggle_taskbar_autohide: String,
    pub custom_x: String,
    pub custom_y: String,
    pub custom_width: String,
    pub custom_height: String,
    pub custom_region_error: Option<String>,
    /// `Some(field)` while a press-to-bind capture is in progress for that
    /// hotkey field (waiting for the next non-modifier key-down); `None`
    /// otherwise. See [`Self::start_capture`]/[`Self::handle_key_event`].
    pub capturing: Option<HotkeyField>,
    /// `true` immediately after a key press during capture was ignored
    /// because it has no name in GameFrame's hotkey vocabulary (see
    /// [`key_display_name`]) -- lets the UI show a transient "key not
    /// supported" hint instead of silently doing nothing. Reset whenever a
    /// new capture starts or the current one ends (completed or cancelled).
    pub unsupported_key_pressed: bool,
    /// `Some(message)` immediately after a capture would complete with the
    /// same combo already bound to the *other* hotkey field -- lets the UI
    /// surface an "already bound" error instead of silently accepting a
    /// binding that would later fail at `Hotkeys::register`. Reset whenever
    /// a new capture starts or the current one ends.
    pub duplicate_hotkey_error: Option<String>,
}

/// Which of GameFrame's two hotkey fields a press-to-bind capture applies
/// to. See [`SettingsState::capturing`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyField {
    ToggleBorderless,
    ToggleTaskbarAutohide,
}

impl SettingsState {
    /// Builds the editable state from a loaded/default [`Config`]. Unbound
    /// hotkeys (`None`) become empty strings; the custom-region input
    /// fields always start blank (they're a scratch area for entering a
    /// replacement, not a display of the current `Custom`'s coordinates).
    pub fn from_config(config: &Config) -> Self {
        Self {
            left_depth_percent: config.presets.left_depth_percent,
            right_depth_percent: config.presets.right_depth_percent,
            custom: config.presets.custom,
            toggle_borderless: config.hotkeys.toggle_borderless.clone().unwrap_or_default(),
            toggle_taskbar_autohide: config.hotkeys.toggle_taskbar_autohide.clone().unwrap_or_default(),
            custom_x: String::new(),
            custom_y: String::new(),
            custom_width: String::new(),
            custom_height: String::new(),
            custom_region_error: None,
            capturing: None,
            unsupported_key_pressed: false,
            duplicate_hotkey_error: None,
        }
    }

    /// Builds the [`Presets`] to persist from the current depth percentages
    /// and `Custom` value.
    pub fn to_presets(&self) -> Presets {
        Presets {
            left_depth_percent: self.left_depth_percent,
            right_depth_percent: self.right_depth_percent,
            custom: self.custom,
        }
    }

    /// Builds the [`Config`] to persist. A blank (or whitespace-only) hotkey
    /// field is treated as unbound (`None`).
    pub fn to_config(&self) -> Config {
        Config {
            presets: self.to_presets(),
            hotkeys: HotkeyBindings {
                toggle_borderless: non_empty(&self.toggle_borderless),
                toggle_taskbar_autohide: non_empty(&self.toggle_taskbar_autohide),
            },
        }
    }

    /// Parses the custom X/Y/width/height fields and validates the
    /// resulting region against `monitor` via
    /// [`crate::region_calc::resolve_region`]. On success, replaces
    /// `self.custom` with the new value (there is at most one `Custom`, so
    /// this always replaces rather than appends) and clears
    /// `custom_region_error`. On failure (unparsable input, or a region
    /// `resolve_region` rejects), leaves `self.custom` unchanged and sets
    /// `custom_region_error` to a user-visible message instead.
    pub fn set_custom_preset(&mut self, monitor: &MonitorGeometry) {
        match self.parse_custom_preset(monitor) {
            Ok(custom) => {
                self.custom = Some(custom);
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

    /// Clears the `Custom` preset entirely (back to "none set").
    pub fn clear_custom_preset(&mut self) {
        self.custom = None;
    }

    fn parse_custom_preset(&self, monitor: &MonitorGeometry) -> Result<CustomRegion, String> {
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

        Ok(CustomRegion { x, y, width, height })
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

        if key == egui::Key::Escape {
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

        let other_binding = match field {
            HotkeyField::ToggleBorderless => &self.toggle_taskbar_autohide,
            HotkeyField::ToggleTaskbarAutohide => &self.toggle_borderless,
        };
        if !other_binding.is_empty() && *other_binding == combo {
            self.duplicate_hotkey_error =
                Some(format!("{combo} is already bound to the other hotkey"));
            return;
        }

        match field {
            HotkeyField::ToggleBorderless => self.toggle_borderless = combo,
            HotkeyField::ToggleTaskbarAutohide => self.toggle_taskbar_autohide = combo,
        }
        self.cancel_capture();
    }

    pub fn clear_hotkey(&mut self, field: HotkeyField) {
        match field {
            HotkeyField::ToggleBorderless => self.toggle_borderless.clear(),
            HotkeyField::ToggleTaskbarAutohide => self.toggle_taskbar_autohide.clear(),
        }
        if self.capturing == Some(field) {
            self.cancel_capture();
        }
    }
}

fn parse_field(name: &str, value: &str) -> Result<i32, String> {
    value.trim().parse::<i32>().map_err(|_| format!("{name} must be a whole number"))
}

/// A human-readable label for a preset, for use as a thumbnail's caption in
/// the Settings preset UI -- deliberately not `Preset`'s `Debug` formatting,
/// which is meant for diagnostics, not end-user display.
fn preset_label(preset: &Preset) -> String {
    match preset {
        Preset::Left { depth_percent } => format!("Left ({depth_percent}%)"),
        Preset::Right { depth_percent } => format!("Right ({depth_percent}%)"),
        Preset::HalfLeft => "Half Left".to_string(),
        Preset::HalfRight => "Half Right".to_string(),
        Preset::Custom { x, y, width, height } => {
            format!("Custom ({x}, {y}, {width}x{height})")
        }
    }
}

/// A resolved [`Region`] expressed as proportions (`0.0..=1.0`) of the
/// monitor it was resolved against, so a fixed-size thumbnail area can
/// render it without knowing real monitor pixel dimensions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizedRegion {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

fn normalize_region(region: &Region, monitor: &MonitorGeometry) -> NormalizedRegion {
    NormalizedRegion {
        x: (region.x - monitor.x) as f32 / monitor.width as f32,
        y: (region.y - monitor.y) as f32 / monitor.height as f32,
        width: region.width as f32 / monitor.width as f32,
        height: region.height as f32 / monitor.height as f32,
    }
}

/// The graphical-thumbnail data for a single preset: a caption, its region
/// normalized against `monitor` (if it resolved), whether it's the currently
/// active preset, and -- if resolution failed -- an error to surface instead
/// of silently omitting the preset from the row. Only `Custom` can fail to
/// resolve (e.g. coordinates persisted while running on a different monitor
/// configuration); the four base presets always resolve.
#[derive(Debug, Clone, PartialEq)]
pub struct PresetThumbnail {
    pub label: String,
    pub region: Option<NormalizedRegion>,
    pub is_active: bool,
    pub error: Option<String>,
}

pub fn preset_thumbnails(state: &SettingsState, monitor: &MonitorGeometry) -> Vec<PresetThumbnail> {
    let presets = state.to_presets();
    let active = win32::active_preset(&presets);

    presets
        .all()
        .into_iter()
        .map(|preset| match resolve_region(&preset, monitor) {
            Ok(region) => PresetThumbnail {
                label: preset_label(&preset),
                region: Some(normalize_region(&region, monitor)),
                is_active: preset == active,
                error: None,
            },
            Err(e) => PresetThumbnail {
                label: preset_label(&preset),
                region: None,
                is_active: false,
                error: Some(e.to_string()),
            },
        })
        .collect()
}

/// `true` for egui's eight physical modifier-key variants (Shift/Control/Alt/
/// Super, left or right). A press of one of these alone must never complete
/// a hotkey capture -- only combined with a subsequent non-modifier key.
fn is_physical_modifier_key(key: egui::Key) -> bool {
    matches!(
        key,
        egui::Key::ShiftLeft
            | egui::Key::ShiftRight
            | egui::Key::ControlLeft
            | egui::Key::ControlRight
            | egui::Key::AltLeft
            | egui::Key::AltRight
            | egui::Key::SuperLeft
            | egui::Key::SuperRight
    )
}

/// Maps a physical, non-modifier `egui::Key` to the name token GameFrame's
/// stored hotkey strings use for it -- the same token `global_hotkey::hotkey::HotKey`'s
/// `FromStr` (via `resolve_bindings`) already accepts, so no changes to the
/// persistence/parsing layer are needed. Returns `None` for keys with no
/// name in that vocabulary (the capture is then left listening rather than
/// completing with an unparsable binding).
///
/// `Enter` and `Space` are deliberately excluded from this vocabulary even
/// though `global_hotkey`/`resolve_bindings` support them -- see the comment
/// at their removed match arms below for why they can never actually be
/// captured via the "Set hotkey" button.
fn key_display_name(key: egui::Key) -> Option<&'static str> {
    use egui::Key;
    Some(match key {
        Key::A => "A",
        Key::B => "B",
        Key::C => "C",
        Key::D => "D",
        Key::E => "E",
        Key::F => "F",
        Key::G => "G",
        Key::H => "H",
        Key::I => "I",
        Key::J => "J",
        Key::K => "K",
        Key::L => "L",
        Key::M => "M",
        Key::N => "N",
        Key::O => "O",
        Key::P => "P",
        Key::Q => "Q",
        Key::R => "R",
        Key::S => "S",
        Key::T => "T",
        Key::U => "U",
        Key::V => "V",
        Key::W => "W",
        Key::X => "X",
        Key::Y => "Y",
        Key::Z => "Z",
        Key::Num0 => "0",
        Key::Num1 => "1",
        Key::Num2 => "2",
        Key::Num3 => "3",
        Key::Num4 => "4",
        Key::Num5 => "5",
        Key::Num6 => "6",
        Key::Num7 => "7",
        Key::Num8 => "8",
        Key::Num9 => "9",
        Key::F1 => "F1",
        Key::F2 => "F2",
        Key::F3 => "F3",
        Key::F4 => "F4",
        Key::F5 => "F5",
        Key::F6 => "F6",
        Key::F7 => "F7",
        Key::F8 => "F8",
        Key::F9 => "F9",
        Key::F10 => "F10",
        Key::F11 => "F11",
        Key::F12 => "F12",
        Key::F13 => "F13",
        Key::F14 => "F14",
        Key::F15 => "F15",
        Key::F16 => "F16",
        Key::F17 => "F17",
        Key::F18 => "F18",
        Key::F19 => "F19",
        Key::F20 => "F20",
        Key::F21 => "F21",
        Key::F22 => "F22",
        Key::F23 => "F23",
        Key::F24 => "F24",
        Key::ArrowDown => "Down",
        Key::ArrowLeft => "Left",
        Key::ArrowRight => "Right",
        Key::ArrowUp => "Up",
        Key::Tab => "Tab",
        Key::Backspace => "Backspace",
        // Enter and Space are deliberately excluded: `hotkey_row` calls
        // `request_focus()` on the "Set hotkey"/"Cancel" button while capturing,
        // and egui treats Enter/Space as a click (`Response::clicked()`, via
        // `Flags::FAKE_PRIMARY_CLICKED`) on that focused widget before the raw
        // `Event::Key` for Enter/Space ever reaches `handle_key_event`. That makes
        // them impossible to actually capture as a bindable main key, so they're
        // left out of this vocabulary rather than advertised as a broken promise.
        Key::Insert => "Insert",
        Key::Delete => "Delete",
        Key::Home => "Home",
        Key::End => "End",
        Key::PageUp => "PageUp",
        Key::PageDown => "PageDown",
        Key::Minus => "Minus",
        Key::Period => "Period",
        Key::Quote => "Quote",
        Key::Semicolon => "Semicolon",
        Key::Slash => "Slash",
        Key::Backslash => "Backslash",
        Key::Equals => "Equal",
        Key::Backtick => "Backquote",
        Key::Comma => "Comma",
        // Anything else (including `Escape`, handled separately as the
        // capture-cancel key, and keys with no token in GameFrame's stored
        // string vocabulary) is not representable -- leave the capture
        // listening rather than complete it with an unparsable binding.
        _ => return None,
    })
}

/// Converts a captured key-down event (the held `modifiers` plus the
/// physical, non-modifier `key` that completed the chord) into GameFrame's
/// existing `"Ctrl+Alt+B"`-style stored hotkey string convention.
///
/// Deliberately NOT `HotKey::Display`, whose `into_string()` round-trips to
/// a different, lowercase, spelled-out format (`"control+alt+keyb"`) that
/// doesn't match what `profile_store`/`resolve_bindings` already expect --
/// see `docs/research/hotkey-capture-widget.md`. Returns `None` if `key` is
/// itself a physical modifier key (a modifier-only press never completes a
/// capture) or has no known name in GameFrame's hotkey vocabulary.
///
/// Note: egui's `Modifiers` has no Windows/Super-key flag on this platform
/// (`mac_cmd` is Mac-only, `command` mirrors `ctrl` on Windows) -- so a
/// captured combo can only carry Ctrl/Alt/Shift, matching every binding
/// GameFrame has ever stored to date.
fn format_captured_hotkey(modifiers: egui::Modifiers, key: egui::Key) -> Option<String> {
    if is_physical_modifier_key(key) {
        return None;
    }
    let name = key_display_name(key)?;

    let mut combo = String::new();
    if modifiers.ctrl {
        combo.push_str("Ctrl+");
    }
    if modifiers.alt {
        combo.push_str("Alt+");
    }
    if modifiers.shift {
        combo.push_str("Shift+");
    }
    combo.push_str(name);
    Some(combo)
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
            .with_inner_size([440.0, 560.0])
            .with_icon(window_icon()),
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

/// GameFrame's corner-bracket mark from [`gameframe::icon`], rendered at
/// 256x256 as recommended by [`egui::IconData`]'s own doc comment, for the
/// Settings window's title-bar/taskbar icon.
fn window_icon() -> egui::IconData {
    const SIZE: u32 = 256;
    egui::IconData { rgba: gameframe::icon::mark_rgba(SIZE), width: SIZE, height: SIZE }
}

/// The live egui app backing the Settings window. Torn down entirely when
/// [`run_settings_window`] returns -- nothing here outlives that call.
struct SettingsApp {
    state: SettingsState,
    monitor: MonitorGeometry,
    saved_config: Rc<RefCell<Option<Config>>>,
}

impl SettingsApp {
    fn hotkey_row(&mut self, ui: &mut egui::Ui, label: &str, field: HotkeyField) {
        let is_capturing = self.state.capturing == Some(field);

        ui.horizontal(|ui| {
            ui.label(label);

            if is_capturing {
                ui.colored_label(egui::Color32::YELLOW, "Press a key... (Esc to cancel)");
            } else {
                let bound = match field {
                    HotkeyField::ToggleBorderless => &self.state.toggle_borderless,
                    HotkeyField::ToggleTaskbarAutohide => &self.state.toggle_taskbar_autohide,
                };
                ui.monospace(if bound.is_empty() { "Not bound" } else { bound.as_str() });
            }

            let set_hotkey_response = ui.button(if is_capturing { "Cancel" } else { "Set hotkey" });
            if set_hotkey_response.clicked() {
                if is_capturing {
                    self.state.cancel_capture();
                } else {
                    self.state.start_capture(field);
                    // Steal keyboard focus away from any other focused widget
                    // (e.g. a custom-preset TextEdit) so the captured key-down
                    // isn't also delivered to it as an Event::Text.
                    set_hotkey_response.request_focus();
                }
            }
            if ui.button("Clear").clicked() {
                self.state.clear_hotkey(field);
            }
        });

        if is_capturing {
            if self.state.unsupported_key_pressed {
                ui.colored_label(egui::Color32::RED, "That key isn't supported -- try another.");
            }
            if let Some(error) = &self.state.duplicate_hotkey_error {
                ui.colored_label(egui::Color32::RED, error);
            }

            for event in ui.ctx().input(|i| i.events.clone()) {
                if let egui::Event::Key { physical_key: Some(key), pressed: true, repeat: false, modifiers, .. } = event
                {
                    self.state.handle_key_event(modifiers, key);
                }
            }
        }
    }

    /// Renders the graphical thumbnail row: one small monitor-outline
    /// rectangle per preset (from [`preset_thumbnails`]), with the resolved
    /// region highlighted inside it and the currently active preset called
    /// out distinctly.
    fn preset_thumbnails_row(&self, ui: &mut egui::Ui) {
        const THUMBNAIL_SIZE: egui::Vec2 = egui::Vec2::new(84.0, 48.0);
        let active_color = egui::Color32::from_rgb(90, 170, 250);
        let inactive_color = egui::Color32::from_rgb(120, 120, 120);

        ui.horizontal_wrapped(|ui| {
            for thumbnail in preset_thumbnails(&self.state, &self.monitor) {
                ui.vertical(|ui| {
                    let (outline_rect, _response) =
                        ui.allocate_exact_size(THUMBNAIL_SIZE, egui::Sense::hover());
                    let painter = ui.painter();
                    painter.rect_stroke(
                        outline_rect,
                        2.0,
                        egui::Stroke::new(1.0, egui::Color32::DARK_GRAY),
                        egui::StrokeKind::Outside,
                    );

                    match thumbnail.region {
                        Some(region) => {
                            let highlight_rect = egui::Rect::from_min_size(
                                outline_rect.min
                                    + egui::vec2(region.x * THUMBNAIL_SIZE.x, region.y * THUMBNAIL_SIZE.y),
                                egui::vec2(region.width * THUMBNAIL_SIZE.x, region.height * THUMBNAIL_SIZE.y),
                            );
                            let color = if thumbnail.is_active { active_color } else { inactive_color };
                            painter.rect_filled(highlight_rect, 0.0, color);
                        }
                        None => {
                            // Unresolvable preset (only `Custom` can hit this --
                            // e.g. coordinates persisted while running on a
                            // different monitor configuration). Surface a visible
                            // placeholder instead of silently dropping the entry.
                            painter.line_segment(
                                [outline_rect.left_top(), outline_rect.right_bottom()],
                                egui::Stroke::new(1.5, egui::Color32::RED),
                            );
                            painter.line_segment(
                                [outline_rect.right_top(), outline_rect.left_bottom()],
                                egui::Stroke::new(1.5, egui::Color32::RED),
                            );
                        }
                    }

                    ui.label(&thumbnail.label);
                    if thumbnail.is_active {
                        ui.colored_label(active_color, "Active");
                    }
                    if let Some(error) = &thumbnail.error {
                        ui.colored_label(egui::Color32::RED, format!("Unavailable: {error}"))
                            .on_hover_text(
                                "This preset's coordinates don't fit the current monitor. \
                                 Re-set it under Custom below.",
                            );
                    }
                });
            }
        });
    }
}

impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Presets");
            self.preset_thumbnails_row(ui);

            ui.separator();
            ui.label("Left depth from edge:");
            ui.add(egui::Slider::new(
                &mut self.state.left_depth_percent,
                MIN_DEPTH_PERCENT..=MAX_DEPTH_PERCENT,
            ).suffix("%"));
            ui.label("Right depth from edge:");
            ui.add(egui::Slider::new(
                &mut self.state.right_depth_percent,
                MIN_DEPTH_PERCENT..=MAX_DEPTH_PERCENT,
            ).suffix("%"));

            ui.separator();
            ui.label("Custom preset (X, Y, width, height):");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.state.custom_x).desired_width(60.0));
                ui.add(egui::TextEdit::singleline(&mut self.state.custom_y).desired_width(60.0));
                ui.add(egui::TextEdit::singleline(&mut self.state.custom_width).desired_width(60.0));
                ui.add(egui::TextEdit::singleline(&mut self.state.custom_height).desired_width(60.0));
                if ui.button("Set").clicked() {
                    self.state.set_custom_preset(&self.monitor);
                }
                if self.state.custom.is_some() && ui.button("Clear").clicked() {
                    self.state.clear_custom_preset();
                }
            });
            if let Some(error) = &self.state.custom_region_error {
                ui.colored_label(egui::Color32::RED, error);
            }

            ui.separator();
            ui.heading("Global hotkeys");
            self.hotkey_row(ui, "Toggle borderless:", HotkeyField::ToggleBorderless);
            self.hotkey_row(ui, "Toggle taskbar autohide:", HotkeyField::ToggleTaskbarAutohide);

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

    #[test]
    fn format_captured_hotkey_with_modifiers_and_a_letter_key_uses_gameframes_stored_string_convention() {
        let modifiers = egui::Modifiers { ctrl: true, alt: true, ..egui::Modifiers::NONE };

        let formatted = format_captured_hotkey(modifiers, egui::Key::B);

        assert_eq!(formatted, Some("Ctrl+Alt+B".to_string()));
    }

    #[test]
    fn format_captured_hotkey_rejects_a_modifier_only_press_as_an_incomplete_capture() {
        let modifiers = egui::Modifiers { ctrl: true, ..egui::Modifiers::NONE };

        let formatted = format_captured_hotkey(modifiers, egui::Key::ControlLeft);

        assert_eq!(formatted, None);
    }

    #[test]
    fn format_captured_hotkey_with_no_modifiers_is_just_the_key_name() {
        let formatted = format_captured_hotkey(egui::Modifiers::NONE, egui::Key::T);

        assert_eq!(formatted, Some("T".to_string()));
    }

    #[test]
    fn key_display_name_excludes_enter_and_space_because_they_are_unbindable_via_the_capture_button() {
        // Enter/Space are consumed as a click on the focused "Set hotkey"/"Cancel"
        // button before the raw `Event::Key` ever reaches `handle_key_event`, so
        // they can never actually complete a capture -- see the doc comment on
        // `key_display_name`.
        assert_eq!(key_display_name(egui::Key::Enter), None);
        assert_eq!(key_display_name(egui::Key::Space), None);
    }

    fn test_monitor() -> MonitorGeometry {
        MonitorGeometry { x: 0, y: 0, width: 1920, height: 1080 }
    }

    #[test]
    fn from_config_of_a_fresh_default_config_shows_unbound_hotkeys_and_default_depth_presets() {
        let config = Config::default();

        let state = SettingsState::from_config(&config);

        assert_eq!(state.left_depth_percent, config.presets.left_depth_percent);
        assert_eq!(state.right_depth_percent, config.presets.right_depth_percent);
        assert_eq!(state.custom, None);
        assert_eq!(state.toggle_borderless, "");
        assert_eq!(state.toggle_taskbar_autohide, "");
        assert_eq!(state.custom_region_error, None);
    }

    #[test]
    fn from_config_of_a_saved_config_shows_the_saved_hotkeys_and_presets() {
        let config = Config {
            presets: Presets {
                left_depth_percent: 20,
                right_depth_percent: 80,
                custom: Some(CustomRegion { x: 10, y: 10, width: 200, height: 100 }),
            },
            hotkeys: HotkeyBindings {
                toggle_borderless: Some("Ctrl+Alt+B".to_string()),
                toggle_taskbar_autohide: Some("Ctrl+Alt+T".to_string()),
            },
        };

        let state = SettingsState::from_config(&config);

        assert_eq!(state.left_depth_percent, 20);
        assert_eq!(state.right_depth_percent, 80);
        assert_eq!(state.custom, Some(CustomRegion { x: 10, y: 10, width: 200, height: 100 }));
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
    fn to_config_carries_the_current_depth_percentages_and_custom_preset() {
        let mut state = SettingsState::from_config(&Config::default());
        state.left_depth_percent = 15;
        state.right_depth_percent = 85;
        state.custom = Some(CustomRegion { x: 1, y: 2, width: 3, height: 4 });

        let config = state.to_config();

        assert_eq!(config.presets.left_depth_percent, 15);
        assert_eq!(config.presets.right_depth_percent, 85);
        assert_eq!(config.presets.custom, Some(CustomRegion { x: 1, y: 2, width: 3, height: 4 }));
    }

    #[test]
    fn set_custom_preset_with_a_valid_region_sets_it_and_clears_any_error() {
        let mut state = SettingsState::from_config(&Config::default());
        state.custom_region_error = Some("stale error".to_string());
        state.custom_x = "100".to_string();
        state.custom_y = "50".to_string();
        state.custom_width = "800".to_string();
        state.custom_height = "600".to_string();

        state.set_custom_preset(&test_monitor());

        assert_eq!(state.custom_region_error, None);
        assert_eq!(state.custom, Some(CustomRegion { x: 100, y: 50, width: 800, height: 600 }));
    }

    #[test]
    fn set_custom_preset_replaces_any_previously_set_custom_preset_rather_than_appending() {
        let mut state = SettingsState::from_config(&Config::default());
        state.custom = Some(CustomRegion { x: 0, y: 0, width: 100, height: 100 });
        state.custom_x = "200".to_string();
        state.custom_y = "200".to_string();
        state.custom_width = "300".to_string();
        state.custom_height = "300".to_string();

        state.set_custom_preset(&test_monitor());

        assert_eq!(state.custom, Some(CustomRegion { x: 200, y: 200, width: 300, height: 300 }));
    }

    #[test]
    fn set_custom_preset_with_a_negative_position_is_rejected_with_a_visible_error() {
        let mut state = SettingsState::from_config(&Config::default());
        let original_custom = state.custom;
        state.custom_x = "-10".to_string();
        state.custom_y = "0".to_string();
        state.custom_width = "800".to_string();
        state.custom_height = "600".to_string();

        state.set_custom_preset(&test_monitor());

        assert_eq!(state.custom, original_custom);
        assert_eq!(state.custom_region_error, Some(RegionCalcError::OutOfBounds.to_string()));
    }

    #[test]
    fn set_custom_preset_with_a_zero_or_negative_dimension_is_rejected_with_a_visible_error() {
        let mut state = SettingsState::from_config(&Config::default());
        let original_custom = state.custom;
        state.custom_x = "0".to_string();
        state.custom_y = "0".to_string();
        state.custom_width = "0".to_string();
        state.custom_height = "600".to_string();

        state.set_custom_preset(&test_monitor());

        assert_eq!(state.custom, original_custom);
        assert_eq!(state.custom_region_error, Some(RegionCalcError::InvalidDimensions.to_string()));
    }

    #[test]
    fn set_custom_preset_that_would_be_clamped_by_resolve_region_is_rejected_with_a_visible_error() {
        // On-monitor origin, but the entered width/height overflow the monitor's far
        // edge -- region_calc::resolve_region would clamp this to a smaller region
        // rather than erroring. settings_ui must treat that clamping as a rejection,
        // not silently accept a region different from what the user entered.
        let mut state = SettingsState::from_config(&Config::default());
        let original_custom = state.custom;
        state.custom_x = "1000".to_string();
        state.custom_y = "900".to_string();
        state.custom_width = "5000".to_string();
        state.custom_height = "5000".to_string();

        state.set_custom_preset(&test_monitor());

        assert_eq!(state.custom, original_custom);
        assert!(state.custom_region_error.is_some());
    }

    #[test]
    fn set_custom_preset_with_non_numeric_input_is_rejected_with_a_visible_error() {
        let mut state = SettingsState::from_config(&Config::default());
        let original_custom = state.custom;
        state.custom_x = "not-a-number".to_string();
        state.custom_y = "0".to_string();
        state.custom_width = "800".to_string();
        state.custom_height = "600".to_string();

        state.set_custom_preset(&test_monitor());

        assert_eq!(state.custom, original_custom);
        assert!(state.custom_region_error.is_some());
    }

    #[test]
    fn set_custom_preset_with_a_valid_region_clears_the_input_fields_so_set_is_not_repeatable() {
        let mut state = SettingsState::from_config(&Config::default());
        state.custom_x = "100".to_string();
        state.custom_y = "50".to_string();
        state.custom_width = "800".to_string();
        state.custom_height = "600".to_string();

        state.set_custom_preset(&test_monitor());

        assert_eq!(state.custom_x, "");
        assert_eq!(state.custom_y, "");
        assert_eq!(state.custom_width, "");
        assert_eq!(state.custom_height, "");
    }

    #[test]
    fn clear_custom_preset_removes_the_custom_preset() {
        let mut state = SettingsState::from_config(&Config::default());
        state.custom = Some(CustomRegion { x: 1, y: 2, width: 3, height: 4 });

        state.clear_custom_preset();

        assert_eq!(state.custom, None);
    }

    #[test]
    fn handle_key_event_while_capturing_a_field_sets_that_fields_binding_and_stops_listening() {
        let mut state = SettingsState::from_config(&Config::default());
        state.start_capture(HotkeyField::ToggleBorderless);

        let modifiers = egui::Modifiers { ctrl: true, alt: true, ..egui::Modifiers::NONE };
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
    fn handle_key_event_with_only_a_modifier_key_keeps_listening_without_completing_the_capture() {
        let mut state = SettingsState::from_config(&Config::default());
        state.start_capture(HotkeyField::ToggleBorderless);

        let modifiers = egui::Modifiers { ctrl: true, ..egui::Modifiers::NONE };
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
            Some("Ctrl+Alt+B is already bound to the other hotkey".to_string());

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

        let modifiers = egui::Modifiers { ctrl: true, alt: true, ..egui::Modifiers::NONE };
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
            egui::Modifiers { ctrl: true, alt: true, ..egui::Modifiers::NONE },
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
    fn preset_label_renders_human_readable_names_instead_of_debug_formatting() {
        assert_eq!(preset_label(&Preset::Left { depth_percent: 40 }), "Left (40%)");
        assert_eq!(preset_label(&Preset::Right { depth_percent: 60 }), "Right (60%)");
        assert_eq!(preset_label(&Preset::HalfLeft), "Half Left");
        assert_eq!(preset_label(&Preset::HalfRight), "Half Right");
        assert_eq!(
            preset_label(&Preset::Custom { x: 10, y: 20, width: 300, height: 400 }),
            "Custom (10, 20, 300x400)"
        );
    }

    #[test]
    fn normalize_region_expresses_a_region_as_proportions_of_the_monitor() {
        let monitor = test_monitor();
        let region = Region { x: 960, y: 0, width: 960, height: 1080 };

        let normalized = normalize_region(&region, &monitor);

        assert_eq!(normalized, NormalizedRegion { x: 0.5, y: 0.0, width: 0.5, height: 1.0 });
    }

    #[test]
    fn normalize_region_accounts_for_a_monitor_with_a_non_zero_origin() {
        let monitor = MonitorGeometry { x: -2560, y: 0, width: 2560, height: 1440 };
        let region = Region { x: -1280, y: 0, width: 1280, height: 1440 };

        let normalized = normalize_region(&region, &monitor);

        assert_eq!(normalized, NormalizedRegion { x: 0.5, y: 0.0, width: 0.5, height: 1.0 });
    }

    #[test]
    fn preset_thumbnails_lists_only_the_four_base_presets_when_no_custom_is_set() {
        let state = SettingsState::from_config(&Config::default());

        let thumbnails = preset_thumbnails(&state, &test_monitor());

        assert_eq!(thumbnails.len(), 4);
        assert_eq!(thumbnails[0].label, preset_label(&Preset::Left { depth_percent: state.left_depth_percent }));
        assert_eq!(thumbnails[1].label, preset_label(&Preset::Right { depth_percent: state.right_depth_percent }));
        assert_eq!(thumbnails[2].label, "Half Left");
        assert_eq!(thumbnails[3].label, "Half Right");
    }

    #[test]
    fn preset_thumbnails_appends_custom_when_one_is_set() {
        let mut state = SettingsState::from_config(&Config::default());
        state.custom = Some(CustomRegion { x: 10, y: 10, width: 200, height: 100 });

        let thumbnails = preset_thumbnails(&state, &test_monitor());

        assert_eq!(thumbnails.len(), 5);
        assert_eq!(thumbnails[4].label, "Custom (10, 10, 200x100)");
    }

    #[test]
    fn preset_thumbnails_surfaces_an_error_placeholder_instead_of_dropping_an_unresolvable_custom_preset() {
        // Simulates a Custom preset persisted while GameFrame ran on a
        // different (e.g. 4K) monitor: the stored coordinates lie entirely
        // outside `test_monitor`'s bounds, so `resolve_region` returns
        // `OutOfBounds`. The base presets always resolve, so only `Custom`
        // can hit this path (see the doc comment on `preset_thumbnails`).
        let mut state = SettingsState::from_config(&Config::default());
        state.custom = Some(CustomRegion { x: 5000, y: 5000, width: 200, height: 100 });

        let thumbnails = preset_thumbnails(&state, &test_monitor());

        assert_eq!(thumbnails.len(), 5, "an unresolvable preset must still be represented, not dropped");
        let custom_thumbnail = &thumbnails[4];
        assert_eq!(custom_thumbnail.label, "Custom (5000, 5000, 200x100)");
        assert_eq!(custom_thumbnail.region, None, "an unresolvable preset has no drawable region");
        assert!(custom_thumbnail.error.is_some(), "an unresolvable preset must surface an error");
        assert!(!custom_thumbnail.is_active);
    }

    #[test]
    fn preset_thumbnails_marks_only_the_active_preset() {
        // Today's single global-preset mechanism (crate::win32::active_preset)
        // always resolves to Left -- see that function's doc comment for why
        // this issue doesn't change that MVP hack.
        let state = SettingsState::from_config(&Config::default());

        let thumbnails = preset_thumbnails(&state, &test_monitor());

        assert!(thumbnails[0].is_active, "Left should be marked active");
        assert!(!thumbnails[1].is_active);
        assert!(!thumbnails[2].is_active);
        assert!(!thumbnails[3].is_active);
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
        state.left_depth_percent = 25;
        state.custom_x = "10".to_string();
        state.custom_y = "10".to_string();
        state.custom_width = "300".to_string();
        state.custom_height = "200".to_string();
        state.set_custom_preset(&test_monitor());
        let to_persist = state.to_config();
        profile_store::save(&config_dir, &to_persist).unwrap();

        // "Reopen" Settings: reload from disk and rebuild the view-model.
        let reloaded_config = profile_store::load(&config_dir).unwrap();
        let reopened_state = SettingsState::from_config(&reloaded_config);

        assert_eq!(reopened_state.toggle_borderless, "Ctrl+Alt+B");
        assert_eq!(reopened_state.left_depth_percent, 25);
        assert_eq!(
            reopened_state.custom,
            Some(CustomRegion { x: 10, y: 10, width: 300, height: 200 })
        );
    }
}
