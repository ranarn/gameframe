//! Lazily-instantiated egui Settings window: draw/edit the single optional
//! Zone, and assign GameFrame's three global hotkey bindings, persisted via
//! [`crate::profile_store`].
//!
//! Split in two:
//! - [`SettingsState`]: the editable view-model, plain data with no egui/OS
//!   dependency, fully unit tested below. [`zone_view`]/[`normalize_region`]
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

use crate::autostart::{self, AutostartError};
use crate::cursor_style::{self, CursorShape};
use crate::hotkeys::{BindingOutcome, BindingReport, HotkeyAction};
use crate::keyboard_hook;
use crate::profile_store::{
    self, Config, CursorSettings, HotkeyBindings, ProfileStoreError, TaskbarHideMode, TrackedGame,
    UpdateMode, Zone,
};
use crate::region_calc::{MonitorGeometry, Region, resolve_region};
use crate::taskbar;
use crate::updater::{CheckOutcome, SpawnError, UpdateError, UpdateInfo};
use crate::win32;
use crate::zone_overlay;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Title of the Settings window; `crate::instance_channel` finds the window by it.
pub(crate) const SETTINGS_TITLE: &str = "GameFrame Settings";

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
    /// The tracked-games list, editable in place by the Settings "Games"
    /// section (auto-apply per row, and immediate
    /// no-confirmation removal) -- see [`crate::app`]'s apply paths for how
    /// entries are added.
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
    /// [`Self::from_config`] leaves it `false` and [`run_settings_window`]
    /// fills it from the registry; Save writes it back only if it changed.
    pub start_with_windows: bool,
    /// What the startup update check does with an available update (the
    /// radio choice under Updates).
    pub update_mode: UpdateMode,
    /// The Log UI hangs (diagnostics) checkbox.
    pub log_ui_hangs: bool,
    /// The open tab. UI-only (never persisted), but kept here rather than on
    /// [`SettingsApp`] so it survives the "Edit zone" close-and-reopen.
    pub tab: SettingsTab,
    /// Bindings that failed to register when GameFrame last registered its
    /// hotkeys (see [`Self::set_hotkey_reports`]). Read through
    /// [`Self::hotkey_failure`], which hides one once its binding was edited.
    hotkey_failures: Vec<BindingReport>,
}

/// The Settings window's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    Games,
    Cursor,
    /// Zone, hotkeys, taskbar hiding, startup and updates.
    General,
    /// Version, license, repository and the optional donation link.
    About,
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
            tab: SettingsTab::default(),
            hotkey_failures: Vec::new(),
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

    /// Adopts a `cursor.enabled` change made outside Settings while it is open
    /// (the game-cursor hotkey persists it straight to disk), so the checkbox
    /// shows it and Save writes it. Other unsaved edits are left alone.
    pub fn apply_external_cursor_enabled(&mut self, enabled: bool) {
        self.cursor.enabled = enabled;
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

/// The Settings window's fixed inner size, shared between the
/// `ViewportBuilder`'s `.with_inner_size(...)` call and
/// [`centered_window_position`] so the two never drift apart.
const SETTINGS_WINDOW_SIZE: [f32; 2] = [560.0, 800.0];

/// [`SETTINGS_WINDOW_SIZE`], shortened to fit `monitor` (leaving room for the
/// taskbar and title bar) -- the content scrolls, so Save/Cancel stay
/// reachable on small screens.
fn settings_window_size(monitor: &MonitorGeometry) -> [f32; 2] {
    let [width, height] = SETTINGS_WINDOW_SIZE;
    [width, height.min(monitor.height as f32 - 120.0).max(300.0)]
}

/// Computes the top-left position that centers a window of `window_size` on
/// `monitor`. Pure arithmetic, no OS dependency -- mirrors the
/// `region_calc::resolve_region` / `zone_editor::normalize` pure/OS-facing
/// split.
fn centered_window_position(monitor: &MonitorGeometry, window_size: [f32; 2]) -> [f32; 2] {
    [
        monitor.x as f32 + (monitor.width as f32 - window_size[0]) / 2.0,
        monitor.y as f32 + (monitor.height as f32 - window_size[1]) / 2.0,
    ]
}

fn normalize_region(region: &Region, monitor: &MonitorGeometry) -> NormalizedRegion {
    NormalizedRegion {
        x: (region.x - monitor.x) as f32 / monitor.width as f32,
        y: (region.y - monitor.y) as f32 / monitor.height as f32,
        width: region.width as f32 / monitor.width as f32,
        height: region.height as f32 / monitor.height as f32,
    }
}

/// What the Settings "Zone" section shows for the current Zone against the
/// current monitor.
#[derive(Debug, Clone, PartialEq)]
pub enum ZoneView {
    /// No Zone is set: the section shows "No zone set" and a "Draw zone"
    /// button.
    NotSet,
    /// The Zone resolves on this monitor: draw it, normalized, on the
    /// monitor thumbnail.
    Resolved(NormalizedRegion),
    /// A Zone is set but can't be resolved on the current monitor (e.g.
    /// coordinates persisted while running on a different monitor
    /// configuration). Carries the red reason to show.
    Unresolvable(String),
}

/// Whether the Games and Cursor tabs' controls are usable: only once a Zone
/// exists (even one that doesn't fit this monitor -- the user already made
/// it). Without one they are greyed out and the Zone section says why.
pub fn zone_dependent_tabs_enabled(zone: Option<&Zone>) -> bool {
    zone.is_some()
}

/// Shown on the greyed-out tabs while no Zone exists.
const ZONE_REQUIRED_NOTE: &str = "A Zone must be configured first. Open the \"Zone, hotkeys & taskbar\" tab and click \"Draw zone\".";
/// Shown in the Zone section itself while no Zone exists.
const ZONE_SECTION_REQUIRED_NOTE: &str = "A Zone must be configured first: click \"Draw zone\" to set the screen area games are placed into.";

/// Decides what the Zone section shows for `zone` against `monitor` -- the
/// pure data behind the thumbnail painter and the red unresolvable reason.
pub fn zone_view(zone: Option<&Zone>, monitor: &MonitorGeometry) -> ZoneView {
    match zone {
        None => ZoneView::NotSet,
        Some(zone) => match resolve_region(zone, monitor) {
            Ok(region) => ZoneView::Resolved(normalize_region(&region, monitor)),
            Err(e) => ZoneView::Unresolvable(e.to_string()),
        },
    }
}

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
        "couldn't reposition {display_name} live -- the saved zone will be used the next time it launches or you apply it"
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
    let config = profile_store::load(config_dir).map_err(SettingsUiError::Config)?;
    let updated = config_with_zone(config, zone);
    profile_store::save(config_dir, &updated).map_err(SettingsUiError::Config)?;
    Ok(updated)
}

/// Attempts the live re-apply for a single tracked game, via
/// [`win32::resolve_running_window`] + [`taskbar::live_reapply_running_window`].
/// Returns `Some(message)` only when an attempt was actually made and failed
/// (the game was running, but repositioning it hit an OS-level error) -- a
/// game that isn't currently running is silently skipped (`None`) *before*
/// the Zone's region is even resolved: its setting still saved normally and
/// will apply on its next launch/hotkey/tray-apply. This ordering matters --
/// a not-running game must never surface a region-resolution error as a
/// live-apply failure.
///
/// OS-facing (delegates to [`win32::resolve_running_window`]/
/// [`taskbar::live_reapply_running_window`], which perform the real toolhelp
/// snapshot/window enumeration/repositioning) -- not exercised by this
/// crate's automated tests, per this crate's established convention;
/// [`live_apply_failure_message`] above is the pure decision this delegates
/// to for its one testable piece (the message text).
fn live_reapply_tracked_game(
    game: &TrackedGame,
    zone: &Zone,
    monitor: &MonitorGeometry,
    hide_taskbar_on_apply: bool,
    taskbar_hide_mode: TaskbarHideMode,
) -> Option<String> {
    // Not-running must always short-circuit before region resolution is even
    // attempted: a saved Zone that no longer fits the current monitor would
    // otherwise surface a spurious failure message for a game that was
    // never actually running.
    let hwnd = win32::resolve_running_window(&game.exe_path)?;

    let region = match resolve_region(zone, monitor) {
        Ok(region) => region,
        Err(e) => {
            log!(
                "live re-apply: failed to resolve the Zone for {}: {e}",
                game.display_name
            );
            return Some(live_apply_failure_message(&game.display_name));
        }
    };

    match taskbar::live_reapply_running_window(
        hwnd,
        &game.display_name,
        hide_taskbar_on_apply,
        taskbar_hide_mode,
        region,
    ) {
        win32::LiveApplyOutcome::Applied => None,
        win32::LiveApplyOutcome::Failed(e) => {
            log!("live re-apply failed for {}: {e}", game.display_name);
            Some(live_apply_failure_message(&game.display_name))
        }
    }
}

/// Live re-applies a newly-saved `zone` to every tracked game in `games`
/// (the Zone is shared, so a new one affects all of them). Returns one
/// status-line message per game whose live re-apply attempt failed -- an
/// empty list if every game either isn't running or was repositioned
/// successfully.
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs.
fn live_reapply_all(
    games: &[TrackedGame],
    zone: &Zone,
    monitor: &MonitorGeometry,
    hide_taskbar_on_apply: bool,
    taskbar_hide_mode: TaskbarHideMode,
) -> Vec<String> {
    games
        .iter()
        .filter_map(|game| {
            live_reapply_tracked_game(
                game,
                zone,
                monitor,
                hide_taskbar_on_apply,
                taskbar_hide_mode,
            )
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
/// `FromStr` (via `register_each`) already accepts, so no changes to the
/// persistence/parsing layer are needed. Returns `None` for keys with no
/// name in that vocabulary (the capture is then left listening rather than
/// completing with an unparsable binding).
///
/// `Enter` and `Space` are deliberately excluded from this vocabulary even
/// though `global_hotkey` supports them -- see the comment
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
        // `Escape` is bindable as a main key ONLY when combined with a held
        // modifier (e.g. `Ctrl+Escape`) -- a bare, unmodified Escape press is
        // intercepted earlier by `handle_key_event`'s cancel-capture check
        // and never reaches this function at all.
        Key::Escape => "Escape",
        // Anything else (keys with no token in GameFrame's stored string
        // vocabulary) is not representable -- leave the capture listening
        // rather than complete it with an unparsable binding.
        _ => return None,
    })
}

/// Maps a physical, non-modifier `egui::Key` to its `global_hotkey::hotkey::Code`
/// counterpart (a `keyboard_types::Code` re-export) -- exactly the same
/// vocabulary [`key_display_name`] accepts, so the two lookups never drift
/// apart on which physical keys GameFrame's capture widget supports. Returns `None` for keys
/// outside that vocabulary (including `Enter`/`Space`, excluded from
/// `key_display_name` for the reason documented there).
fn key_to_code(key: egui::Key) -> Option<Code> {
    use egui::Key;
    Some(match key {
        Key::A => Code::KeyA,
        Key::B => Code::KeyB,
        Key::C => Code::KeyC,
        Key::D => Code::KeyD,
        Key::E => Code::KeyE,
        Key::F => Code::KeyF,
        Key::G => Code::KeyG,
        Key::H => Code::KeyH,
        Key::I => Code::KeyI,
        Key::J => Code::KeyJ,
        Key::K => Code::KeyK,
        Key::L => Code::KeyL,
        Key::M => Code::KeyM,
        Key::N => Code::KeyN,
        Key::O => Code::KeyO,
        Key::P => Code::KeyP,
        Key::Q => Code::KeyQ,
        Key::R => Code::KeyR,
        Key::S => Code::KeyS,
        Key::T => Code::KeyT,
        Key::U => Code::KeyU,
        Key::V => Code::KeyV,
        Key::W => Code::KeyW,
        Key::X => Code::KeyX,
        Key::Y => Code::KeyY,
        Key::Z => Code::KeyZ,
        Key::Num0 => Code::Digit0,
        Key::Num1 => Code::Digit1,
        Key::Num2 => Code::Digit2,
        Key::Num3 => Code::Digit3,
        Key::Num4 => Code::Digit4,
        Key::Num5 => Code::Digit5,
        Key::Num6 => Code::Digit6,
        Key::Num7 => Code::Digit7,
        Key::Num8 => Code::Digit8,
        Key::Num9 => Code::Digit9,
        Key::F1 => Code::F1,
        Key::F2 => Code::F2,
        Key::F3 => Code::F3,
        Key::F4 => Code::F4,
        Key::F5 => Code::F5,
        Key::F6 => Code::F6,
        Key::F7 => Code::F7,
        Key::F8 => Code::F8,
        Key::F9 => Code::F9,
        Key::F10 => Code::F10,
        Key::F11 => Code::F11,
        Key::F12 => Code::F12,
        Key::F13 => Code::F13,
        Key::F14 => Code::F14,
        Key::F15 => Code::F15,
        Key::F16 => Code::F16,
        Key::F17 => Code::F17,
        Key::F18 => Code::F18,
        Key::F19 => Code::F19,
        Key::F20 => Code::F20,
        Key::F21 => Code::F21,
        Key::F22 => Code::F22,
        Key::F23 => Code::F23,
        Key::F24 => Code::F24,
        Key::ArrowDown => Code::ArrowDown,
        Key::ArrowLeft => Code::ArrowLeft,
        Key::ArrowRight => Code::ArrowRight,
        Key::ArrowUp => Code::ArrowUp,
        Key::Tab => Code::Tab,
        Key::Backspace => Code::Backspace,
        Key::Insert => Code::Insert,
        Key::Delete => Code::Delete,
        Key::Home => Code::Home,
        Key::End => Code::End,
        Key::PageUp => Code::PageUp,
        Key::PageDown => Code::PageDown,
        Key::Minus => Code::Minus,
        Key::Period => Code::Period,
        Key::Quote => Code::Quote,
        Key::Semicolon => Code::Semicolon,
        Key::Slash => Code::Slash,
        Key::Backslash => Code::Backslash,
        Key::Equals => Code::Equal,
        Key::Backtick => Code::Backquote,
        Key::Comma => Code::Comma,
        // See `key_display_name`'s matching `Escape` arm -- only reachable
        // here when a modifier was held (an unmodified Escape cancels the
        // capture before `format_captured_hotkey` is ever called).
        Key::Escape => Code::Escape,
        // Anything else has no `Code` counterpart in the vocabulary GameFrame
        // supports -- see `key_display_name`'s matching fallback.
        _ => return None,
    })
}

/// Translates egui's held modifier flags into `global_hotkey::hotkey::Modifiers`.
/// Only Ctrl/Alt/Shift are mapped -- per `format_captured_hotkey`'s existing
/// doc comment, egui's `Modifiers` carries no distinct Windows/Super-key flag
/// on this (Windows-only) platform, so no binding GameFrame has ever captured
/// or stored could carry `Modifiers::SUPER`.
fn modifiers_to_hotkey_modifiers(modifiers: egui::Modifiers) -> Modifiers {
    let mut mods = Modifiers::empty();
    if modifiers.ctrl {
        mods |= Modifiers::CONTROL;
    }
    if modifiers.alt {
        mods |= Modifiers::ALT;
    }
    if modifiers.shift {
        mods |= Modifiers::SHIFT;
    }
    mods
}

/// Constructs a `global_hotkey::hotkey::HotKey` directly from a captured
/// chord (held `modifiers` plus the physical, non-modifier `key` that
/// completed it) via `HotKey::new(mods, code)`, rather than solely through a
/// later `FromStr` round-trip of the persisted string (that round-trip still
/// happens separately, in `hotkeys::register_each`, on every app startup --
/// unavoidable since the persisted config is a plain string, not a living
/// `HotKey`).
///
/// Returns `None` if `key` is itself a physical modifier key, or has no
/// `Code` counterpart in GameFrame's supported vocabulary ([`key_to_code`])
/// -- the same two cases [`format_captured_hotkey`] already leaves the
/// capture listening for instead of completing.
fn build_captured_hotkey(modifiers: egui::Modifiers, key: egui::Key) -> Option<HotKey> {
    if is_physical_modifier_key(key) {
        return None;
    }
    let code = key_to_code(key)?;
    Some(HotKey::new(
        Some(modifiers_to_hotkey_modifiers(modifiers)),
        code,
    ))
}

/// Converts a captured key-down event (the held `modifiers` plus the
/// physical, non-modifier `key` that completed the chord) into GameFrame's
/// existing `"Ctrl+Alt+B"`-style stored hotkey string convention.
///
/// Deliberately NOT `HotKey::Display`, whose `into_string()` round-trips to
/// a different, lowercase, spelled-out format (`"control+alt+keyb"`) that
/// doesn't match what `profile_store`/`register_each` already expect.
/// Returns `None` if `key` is itself a physical modifier key (a
/// modifier-only press never completes a capture) or has no known name in
/// GameFrame's hotkey vocabulary.
///
/// Note: egui's `Modifiers` has no Windows/Super-key flag on this platform
/// (`mac_cmd` is Mac-only, `command` mirrors `ctrl` on Windows) -- so a
/// captured combo can only carry Ctrl/Alt/Shift, matching every binding
/// GameFrame has ever stored to date.
fn format_captured_hotkey(modifiers: egui::Modifiers, key: egui::Key) -> Option<String> {
    // Validate via a directly-constructed `HotKey` first -- a chord
    // that can't build one (unrepresentable physical key) must not produce a
    // stored string either. The modifier substrings below are then read back off
    // that same constructed `HotKey`'s `mods`, not recomputed independently from
    // `modifiers`, so the two can't drift apart.
    let hotkey = build_captured_hotkey(modifiers, key)?;
    let name = key_display_name(key)?;

    let mut combo = String::new();
    if hotkey.mods.contains(Modifiers::CONTROL) {
        combo.push_str("Ctrl+");
    }
    if hotkey.mods.contains(Modifiers::ALT) {
        combo.push_str("Alt+");
    }
    if hotkey.mods.contains(Modifiers::SHIFT) {
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
/// reason as [`crate::tray_ui::Tray::new`]. Only the primary monitor is
/// supported (see `region_calc` module docs); there is no monitor-enumeration
/// module.
pub fn primary_monitor_geometry() -> MonitorGeometry {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

    // Safety: `GetSystemMetrics` is a simple, side-effect-free Win32 query
    // that takes a plain integer index and returns a plain integer; no
    // pointers or lifetimes are involved.
    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    MonitorGeometry {
        x: 0,
        y: 0,
        width,
        height,
    }
}

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

impl std::error::Error for SettingsUiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SettingsUiError::Config(e) => Some(e),
            SettingsUiError::Window(e) => Some(e),
            SettingsUiError::Autostart(e) => Some(e),
        }
    }
}

/// What closed the Settings window: a completed Save (carrying the
/// [`Config`] to persist), a Cancel/close-without-saving, or a request to
/// open the zone-editor overlay for the Zone (carrying the current
/// in-progress [`SettingsState`], plus this session's current pending
/// live-re-apply status messages ([`SettingsApp::pending_status_messages`])
/// so neither is lost while the overlay is open).
///
/// [`run_settings_window`] loops on this: `EditZone` re-opens the
/// Settings window (with the overlay's result, and the carried-forward
/// status messages, folded back in) rather than ending the session, since
/// opening the zone editor is not itself a decision to close Settings.
enum SettingsExit {
    /// The config to persist, plus the "Start with Windows" choice (which
    /// lives in the registry, not in [`Config`]).
    Saved(Config, bool),
    Cancelled,
    /// Carries the in-progress [`SettingsState`] plus this session's current
    /// pending live-re-apply status messages ([`SettingsApp::pending_status_messages`])
    /// -- so a message surfaced by a Zone commit made
    /// just before "Edit zone" was clicked isn't lost when this
    /// session's `SettingsApp` is torn down; [`run_settings_window`] carries
    /// it forward as the next session's initial status messages.
    EditZone(SettingsState, Vec<String>),
}

/// The tracked-games list Save writes. Settings edits its own copy
/// (`edited`), but the tray's "Apply to Window" can add games on disk while
/// Settings is open (the main loop keeps running meanwhile). A game that is on
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

/// Which window's frame loop is ticking the app's event handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickContext {
    /// The Settings window is open.
    Settings,
    /// Settings is waiting for the next key to bind; hotkey presses are dropped.
    SettingsCapturing,
    /// The zone-editor overlay is open (Settings is closed meanwhile).
    ZoneEditor,
}

/// Opens the Settings window, blocking until it's closed, then returns.
///
/// Loads the current config from `config_dir` (via [`profile_store::load`]),
/// lets the user edit the Zone and hotkey bindings, and -- only if the user
/// clicks "Save" -- persists the result via [`profile_store::save`] before
/// returning. Closing the window any other way (the "Cancel" button, the
/// window's own close button) discards the edits -- except the Zone, which
/// Save/Cancel don't govern: saving in the zone editor overlay (and "Clear")
/// persists it immediately, so the disk and the windows it was live-applied
/// to never disagree.
///
/// Clicking "Draw zone"/"Edit zone" opens the zone-editor overlay
/// ([`zone_overlay::run_zone_editor_overlay`]) *sequentially* rather than
/// nesting it inside the Settings window's own event loop -- egui/winit
/// don't support one native event loop starting another while it's still
/// running. Concretely: the Settings window closes, the overlay runs to
/// completion, its result (if any) is folded into the in-progress state,
/// and Settings re-opens with that updated state -- the same
/// close-then-reopen sequencing already used for "Show Settings" itself
/// from the tray's message loop (see `app::run_event_loop`).
///
/// Also threads the non-blocking live-re-apply status messages across that
/// same close-then-reopen boundary: `status_messages` starts empty, is
/// replaced wholesale by whatever [`SettingsExit::EditZone`] carried out of
/// the just-closed session (either its untouched carried-forward messages,
/// or a fresh set if the overlay's save triggered a live re-apply), and is
/// handed to the next [`run_settings_frame`]
/// call as its `initial_status_messages` -- so a status line surfaced right
/// before "Edit zone" was clicked survives the round trip through
/// the overlay instead of vanishing with the torn-down `SettingsApp`.
///
/// OS/GPU-facing: constructs a real egui/wgpu context for the lifetime of
/// each iteration and fully drops it before the next (or before returning).
/// Not exercised by this crate's automated tests -- see module docs.
pub fn run_settings_window(
    config_dir: &Path,
    hotkey_reports: &[BindingReport],
    on_tick: &mut dyn FnMut(TickContext) -> Option<bool>,
) -> Result<(), SettingsUiError> {
    let config = profile_store::load(config_dir).map_err(SettingsUiError::Config)?;
    let opened_games = config.tracked_games.clone();
    let mut state = SettingsState::from_config(&config);
    state.set_hotkey_reports(hotkey_reports);
    let started_with_windows = autostart::is_enabled();
    state.start_with_windows = started_with_windows;
    let monitor = primary_monitor_geometry();
    let mut status_messages: Vec<String> = Vec::new();

    loop {
        match run_settings_frame(config_dir, state, monitor, status_messages, &mut *on_tick)? {
            SettingsExit::Saved(mut config, start_with_windows) => {
                // Keep what the tray and hotkeys changed on disk while
                // Settings was open. A failed re-read must not abort the Save.
                match profile_store::load(config_dir) {
                    Ok(on_disk) => {
                        config.tracked_games = merge_tracked_games(
                            &opened_games,
                            std::mem::take(&mut config.tracked_games),
                            &on_disk.tracked_games,
                        );
                    }
                    Err(e) => log!("could not re-read config to merge on Save: {e}"),
                }
                profile_store::save(config_dir, &config).map_err(SettingsUiError::Config)?;
                // Only touch the registry on an actual change, so saving
                // unrelated settings never re-points an existing entry at
                // whichever exe happens to be running (e.g. a dev build).
                if start_with_windows != started_with_windows {
                    autostart::set_enabled(start_with_windows)
                        .map_err(SettingsUiError::Autostart)?;
                }
                return Ok(());
            }
            SettingsExit::Cancelled => return Ok(()),
            SettingsExit::EditZone(mut latest_state, latest_status_messages) => {
                // Carried forward as-is unless the overlay actually commits a
                // new zone below (a live re-apply only fires on an accepted
                // save) -- cancelling/failing to open the overlay is not
                // itself a commit, so whatever status messages were already
                // showing must survive it.
                status_messages = latest_status_messages;
                // The overlay keeps the tray and hotkeys working too; a
                // game-cursor toggle made meanwhile is folded into the state.
                let mut cursor_toggled = None;
                let mut tick = || {
                    if let Some(enabled) = on_tick(TickContext::ZoneEditor) {
                        cursor_toggled = Some(enabled);
                    }
                };
                let overlay = zone_overlay::run_zone_editor_overlay(
                    latest_state.zone,
                    latest_state.taskbar_hide_mode,
                    &mut tick,
                );
                if let Some(enabled) = cursor_toggled {
                    latest_state.apply_external_cursor_enabled(enabled);
                }
                match overlay {
                    Ok(Some(zone)) => {
                        // Persist first, then apply: the Zone on disk (read by
                        // the drift and launch re-apply paths) must never
                        // disagree with the Zone the windows are placed in.
                        match persist_zone(config_dir, Some(zone)) {
                            Ok(saved) => {
                                latest_state.set_zone(zone);
                                // The on-disk taskbar settings, not the
                                // unsaved Settings state.
                                status_messages = live_reapply_all(
                                    &saved.tracked_games,
                                    &zone,
                                    &monitor,
                                    saved.hide_taskbar_on_apply,
                                    saved.taskbar_hide_mode,
                                );
                            }
                            Err(e) => {
                                log!("failed to save the zone: {e}");
                                status_messages = vec![format!("couldn't save the zone: {e}")];
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => log!("failed to open the zone editor overlay: {e}"),
                }
                // A Quit that arrived during the overlay ended it; do not
                // reopen Settings, let the main loop exit.
                if crate::instance_channel::quit_pending() {
                    return Ok(());
                }
                state = latest_state;
            }
        }
    }
}

/// Runs a single Settings window session to completion (see
/// [`run_settings_window`]'s doc comment for why this is a separate,
/// re-enterable step rather than one `eframe::run_native` call for the
/// whole function).
///
/// `initial_status_messages` seeds the new [`SettingsApp`]'s
/// [`SettingsApp::pending_status_messages`] -- carried over from
/// the previous session's [`SettingsExit::EditZone`], or empty on the
/// very first call from [`run_settings_window`]. Any close other than an
/// explicit Save, Cancel, or "Draw zone"/"Edit zone" click -- concretely, the
/// window's own close button -- is treated as [`SettingsExit::Cancelled`]:
/// this function's documented "discards the edits" contract for any
/// non-Save close.
fn run_settings_frame(
    config_dir: &Path,
    state: SettingsState,
    monitor: MonitorGeometry,
    initial_status_messages: Vec<String>,
    on_tick: &mut dyn FnMut(TickContext) -> Option<bool>,
) -> Result<SettingsExit, SettingsUiError> {
    let exit: Rc<RefCell<Option<SettingsExit>>> = Rc::new(RefCell::new(None));
    let exit_for_app = Rc::clone(&exit);
    let config_dir = config_dir.to_path_buf();

    // Window geometry is not persisted, so the window opens centered on the
    // monitor every time.
    let native_options = eframe::NativeOptions {
        persist_window: false,
        viewport: egui::ViewportBuilder::default()
            .with_title(SETTINGS_TITLE)
            .with_inner_size(settings_window_size(&monitor))
            .with_position(centered_window_position(
                &monitor,
                settings_window_size(&monitor),
            ))
            .with_icon(window_icon()),
        ..Default::default()
    };

    eframe::run_native(
        SETTINGS_TITLE,
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(SettingsApp {
                config_dir,
                state,
                monitor,
                exit: exit_for_app,
                pending_status_messages: initial_status_messages,
                keyboard_hook: None,
                cursor_previews: None,
                update_check: UpdateCheck::Idle,
                update_install: UpdateInstall::Idle,
                on_tick,
            }))
        }),
    )
    .map_err(SettingsUiError::Window)?;

    // A close via the window's own close button (rather than Save/Cancel/Edit
    // zone) leaves `exit` empty -- treat that the same as an explicit
    // Cancel, matching this function's documented "discards the edits"
    // contract for any non-Save close.
    Ok(exit.borrow_mut().take().unwrap_or(SettingsExit::Cancelled))
}

/// GameFrame's corner-bracket mark from [`gameframe::icon`], rendered at
/// 256x256 as recommended by [`egui::IconData`]'s own doc comment, for the
/// Settings window's title-bar/taskbar icon.
fn window_icon() -> egui::IconData {
    const SIZE: u32 = 256;
    egui::IconData {
        rgba: gameframe::icon::mark_rgba(SIZE),
        width: SIZE,
        height: SIZE,
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
/// reconstructed by [`run_settings_frame`] on each session.
struct SettingsApp<'a> {
    /// Called once per frame (at least every [`QUIT_POLL`]) so the main
    /// loop's tray, hotkey and update events keep being handled while this
    /// window blocks that loop. Returns the new `cursor.enabled` when a
    /// hotkey press toggled it during the call.
    on_tick: &'a mut dyn FnMut(TickContext) -> Option<bool>,
    /// Where the Zone is persisted immediately on Clear (the overlay-Save
    /// persist lives in [`run_settings_window`]).
    config_dir: std::path::PathBuf,
    state: SettingsState,
    monitor: MonitorGeometry,
    exit: Rc<RefCell<Option<SettingsExit>>>,
    /// Zero or more non-blocking status messages from the most recent
    /// commit-triggered live re-apply attempt -- e.g.
    /// [`live_apply_failure_message`] for a tracked game whose window
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

/// Progress of the Settings window's update check.
enum UpdateCheck {
    Idle,
    /// Running on a worker thread, which sends the result here.
    Running(std::sync::mpsc::Receiver<Result<CheckOutcome, UpdateError>>),
    Done(Result<CheckOutcome, UpdateError>),
    /// Not started: another check (the tray's) was already running.
    Busy,
    /// The worker ended without sending a result.
    Lost,
}

/// Progress of installing the update an `UpdateCheck` found.
enum UpdateInstall {
    Idle,
    /// Running on a worker thread, which sends the result here.
    Running(std::sync::mpsc::Receiver<Result<(), UpdateError>>),
    /// The installer was started (at this time); it asks GameFrame to quit.
    Started(Instant),
    /// The installer was started but GameFrame wasn't quit within
    /// [`INSTALL_QUIT_TIMEOUT`].
    TimedOut,
    Failed(UpdateError),
    /// Not started: another install was already running.
    Busy,
    /// Not started: the worker thread could not be created.
    NotStarted,
    /// The worker ended without sending a result.
    Lost,
}

/// How long Settings waits, after the installer started, for it to quit
/// GameFrame before reporting that the installer didn't finish.
const INSTALL_QUIT_TIMEOUT: Duration = Duration::from_secs(120);

impl UpdateInstall {
    /// Moves `Started` to `TimedOut` once [`INSTALL_QUIT_TIMEOUT`] has passed
    /// between its start and `now`.
    fn expire(&mut self, now: Instant) {
        if let Self::Started(since) = self
            && now.saturating_duration_since(*since) >= INSTALL_QUIT_TIMEOUT
        {
            *self = Self::TimedOut;
        }
    }
}

/// The rendered game-cursor preview and variant icons, plus the settings
/// they were rendered from.
struct CursorPreviews {
    settings: CursorSettings,
    preview: egui::TextureHandle,
    shapes: Vec<(CursorShape, egui::TextureHandle)>,
}

impl SettingsApp<'_> {
    fn hotkey_row(&mut self, ui: &mut egui::Ui, label: &str, field: HotkeyField) {
        let is_capturing = self.state.capturing == Some(field);

        ui.horizontal(|ui| {
            ui.label(label);

            if is_capturing {
                ui.colored_label(egui::Color32::YELLOW, "Press a key... (Esc to cancel)");
            } else {
                let bound = self.state.binding(field);
                ui.monospace(if bound.is_empty() { "Not bound" } else { bound });
            }
            if let Some(reason) = self.state.hotkey_failure(field) {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("⚠ Not working: {reason}"));
            }

            let set_hotkey_response = ui.button(if is_capturing { "Cancel" } else { "Set hotkey" });
            if set_hotkey_response.clicked() {
                if is_capturing {
                    self.state.cancel_capture();
                } else {
                    self.state.start_capture(field);
                    // Steal keyboard focus away from any other focused widget
                    // so the captured key-down isn't also delivered to it as
                    // an Event::Text.
                    set_hotkey_response.request_focus();
                }
            }
            if ui.button("Clear").clicked() {
                self.state.clear_hotkey(field);
            }
        });

        if is_capturing {
            if self.state.unsupported_key_pressed {
                ui.colored_label(
                    egui::Color32::RED,
                    "That key isn't supported -- try another.",
                );
            }
            if let Some(error) = &self.state.duplicate_hotkey_error {
                ui.colored_label(egui::Color32::RED, error);
            }

            for event in ui.ctx().input(|i| i.events.clone()) {
                if let egui::Event::Key {
                    physical_key: Some(key),
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } = event
                {
                    self.state.handle_key_event(modifiers, key);
                }
            }
        }
    }

    /// Renders the "Games" section: one row per tracked game with its
    /// display name, an auto-apply checkbox,
    /// and an immediate no-confirmation "Remove" button (see
    /// [`SettingsState::remove_tracked_game`]).
    ///
    /// Removal is deferred until after the loop (rather than mutating
    /// `self.state.tracked_games` while iterating it) to avoid mutating the
    /// `Vec` out from under `iter_mut`'s in-progress borrow.
    fn games_section(&mut self, ui: &mut egui::Ui) {
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

    /// Re-renders the variant icons and the preview whenever the cursor
    /// settings changed since the last frame -- with the same rasterizer the
    /// overlay uses, so the preview is exactly what appears in game.
    fn refresh_cursor_previews(&mut self, ctx: &egui::Context) {
        let settings = self.state.cursor.sanitized();
        if self
            .cursor_previews
            .as_ref()
            .is_some_and(|p| p.settings == settings)
        {
            return;
        }
        let game_cursor = crate::cursor_overlay::last_game_cursor();
        let to_texture = |name: String, s: &CursorSettings| {
            let bitmap = cursor_style::render_preview(s, game_cursor.as_ref());
            let size = [bitmap.width as usize, bitmap.height as usize];
            let image = egui::ColorImage::from_rgba_premultiplied(size, &bitmap.pixels);
            ctx.load_texture(name, image, egui::TextureOptions::LINEAR)
        };
        let shapes = CursorShape::ALL
            .into_iter()
            .map(|shape| {
                let icon = CursorSettings {
                    shape,
                    size: 30,
                    thickness: settings.thickness.clamp(2, 4),
                    border_width: settings.border_width.min(2),
                    glow_size: settings.glow_size.min(5),
                    ..settings.clone()
                };
                (shape, to_texture(format!("cursor-icon-{shape:?}"), &icon))
            })
            .collect();
        let preview = to_texture("cursor-preview".to_string(), &settings);
        self.cursor_previews = Some(CursorPreviews {
            settings,
            preview,
            shapes,
        });
    }

    /// Renders the "Cursor" tab: pointer variant, size, colors, border,
    /// glow, a live preview on a dark and a light background, and the
    /// pulse/hide behavior.
    fn cursor_section(&mut self, ui: &mut egui::Ui) {
        self.refresh_cursor_previews(ui.ctx());
        let Some(previews) = self.cursor_previews.as_ref() else {
            return;
        };
        let c = &mut self.state.cursor;

        ui.checkbox(&mut c.enabled, "Show the game cursor");
        ui.label(
            egui::RichText::new(
                "A big, high-contrast pointer for games whose own cursor gets lost in the action. \
                 Shake the mouse to make it pulse.",
            )
            .weak(),
        );

        ui.label("Pointer:");
        ui.horizontal_wrapped(|ui| {
            for (shape, texture) in &previews.shapes {
                let image = egui::Image::from_texture(egui::load::SizedTexture::new(
                    texture.id(),
                    texture.size_vec2(),
                ))
                .fit_to_exact_size(egui::vec2(32.0, 32.0));
                let button =
                    egui::Button::image_and_text(image, shape.label()).selected(c.shape == *shape);
                if ui.add(button).clicked() {
                    c.shape = *shape;
                }
            }
        });
        if c.shape == CursorShape::GameOwn {
            ui.label(
                egui::RichText::new(
                    "Builds on the game's own cursor: an enlarged copy behind it (Size; at 32 px or less, \
                     only the border and glow), following its outline and switching with it. The \
                     preview shows the last cursor seen in a game (an arrow until then).",
                )
                .weak(),
            );
        }

        egui::Grid::new("game_cursor_style")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("Size");
                ui.add(egui::Slider::new(&mut c.size, cursor_style::SIZE_RANGE).suffix(" px"));
                ui.end_row();

                if c.shape.uses_thickness() {
                    ui.label("Thickness");
                    ui.add(
                        egui::Slider::new(&mut c.thickness, cursor_style::THICKNESS_RANGE)
                            .suffix(" px"),
                    );
                    ui.end_row();
                }

                if c.uses_color() {
                    ui.label("Color");
                    ui.color_edit_button_srgb(&mut c.color);
                    ui.end_row();
                }

                ui.label("Opacity");
                ui.add(egui::Slider::new(&mut c.opacity, cursor_style::OPACITY_RANGE).suffix(" %"));
                ui.end_row();

                ui.checkbox(&mut c.border, "Border");
                ui.add_enabled_ui(c.border, |ui| {
                    ui.horizontal(|ui| {
                        ui.color_edit_button_srgb(&mut c.border_color);
                        ui.add(
                            egui::Slider::new(
                                &mut c.border_width,
                                cursor_style::BORDER_WIDTH_RANGE,
                            )
                            .suffix(" px"),
                        );
                    });
                });
                ui.end_row();

                ui.checkbox(&mut c.glow, "Glow");
                ui.add_enabled_ui(c.glow, |ui| {
                    ui.horizontal(|ui| {
                        ui.color_edit_button_srgb(&mut c.glow_color);
                        ui.add(
                            egui::Slider::new(&mut c.glow_size, cursor_style::GLOW_SIZE_RANGE)
                                .suffix(" px"),
                        );
                    });
                });
                ui.end_row();

                ui.label("");
                ui.add_enabled_ui(c.glow, |ui| {
                    ui.add(
                        egui::Slider::new(&mut c.glow_strength, cursor_style::GLOW_STRENGTH_RANGE)
                            .text("glow strength %"),
                    );
                });
                ui.end_row();
            });

        ui.horizontal(|ui| {
            for background in [egui::Color32::from_gray(24), egui::Color32::from_gray(225)] {
                const BOX: f32 = 150.0;
                let (rect, _) = ui.allocate_exact_size(egui::vec2(BOX, BOX), egui::Sense::hover());
                ui.painter().rect_filled(rect, 4.0, background);
                ui.painter().rect_stroke(
                    rect,
                    4.0,
                    egui::Stroke::new(1.0, egui::Color32::GRAY),
                    egui::StrokeKind::Inside,
                );
                let size = previews.preview.size_vec2();
                let scale = (BOX - 10.0) / size.x.max(size.y);
                let shown = if scale < 1.0 { size * scale } else { size };
                let image_rect = egui::Rect::from_center_size(rect.center(), shown);
                ui.painter().image(
                    previews.preview.id(),
                    image_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
        });

        ui.label(
            egui::RichText::new(
                "Shown only while the mouse is over a game with \"Game cursor\" ticked on the Games tab.",
            )
            .weak(),
        );
        ui.checkbox(
            &mut c.shake_to_find,
            "Shake the mouse to make the cursor pulse",
        );
        ui.checkbox(&mut c.hide_when_cursor_hidden, "Hide when the game hides its cursor")
            .on_hover_text("Leave off for games that draw their own cursor -- they hide the Windows one all the time.");
    }

    /// Renders the "Zone, hotkeys & taskbar" tab.
    fn general_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Zone");
        self.zone_section(ui);
        ui.separator();
        ui.heading("Global hotkeys");
        self.hotkey_row(ui, "Toggle borderless:", HotkeyField::ToggleBorderless);
        self.hotkey_row(
            ui,
            "Toggle taskbar autohide:",
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

        ui.separator();
        ui.heading("Taskbar hiding");
        ui.checkbox(
            &mut self.state.hide_taskbar_on_apply,
            "Hide taskbar when the Zone is applied",
        );
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

        ui.separator();
        ui.heading("Startup");
        ui.checkbox(&mut self.state.start_with_windows, "Start with Windows");

        ui.separator();
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

        ui.separator();
        ui.heading("Diagnostics");
        ui.checkbox(&mut self.state.log_ui_hangs, "Log UI hangs (diagnostics)");
        ui.label(
            "Writes a line to the log when GameFrame stops responding for a few seconds. Takes effect when you save and close Settings; Cancel keeps the old state.",
        );
    }

    /// Renders the "Check for updates now" button and its status line. The
    /// check runs on a worker thread that wakes the UI when it has a result.
    fn update_check_row(&mut self, ui: &mut egui::Ui) {
        if let UpdateCheck::Running(receiver) = &self.update_check {
            match receiver.try_recv() {
                Ok(result) => self.update_check = UpdateCheck::Done(result),
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.update_check = UpdateCheck::Lost;
                }
            }
        }

        ui.horizontal(|ui| {
            // A new check while an install runs would drop the install's result.
            let installing = matches!(
                self.update_install,
                UpdateInstall::Running(_) | UpdateInstall::Started(_)
            );
            let enabled = !installing && !matches!(self.update_check, UpdateCheck::Running(_));
            if ui
                .add_enabled(enabled, egui::Button::new("Check for updates now"))
                .clicked()
            {
                let (sender, receiver) = std::sync::mpsc::channel();
                let ctx = ui.ctx().clone();
                let started = crate::updater::spawn_check(move |result| {
                    if matches!(result, Ok(CheckOutcome::UpToDate)) {
                        crate::update_ui::notify(crate::update_ui::UpdateEvent::UpToDate);
                    }
                    let _ = sender.send(result);
                    ctx.request_repaint();
                });
                self.update_check = match started {
                    Ok(()) => UpdateCheck::Running(receiver),
                    Err(SpawnError::Busy) => UpdateCheck::Busy,
                    Err(SpawnError::ThreadFailed) => UpdateCheck::Lost,
                };
                self.update_install = UpdateInstall::Idle;
            }
            match &self.update_check {
                UpdateCheck::Idle => {}
                UpdateCheck::Running(_) => {
                    ui.spinner();
                    ui.label("Checking...");
                }
                UpdateCheck::Busy => {
                    ui.label("Another update check is still running.");
                }
                UpdateCheck::Lost => {
                    ui.colored_label(
                        egui::Color32::LIGHT_RED,
                        "Update check failed: it ended unexpectedly",
                    );
                }
                UpdateCheck::Done(result) => {
                    let message = crate::updater::result_message(result);
                    if result.is_ok() {
                        ui.label(message);
                    } else {
                        ui.colored_label(egui::Color32::LIGHT_RED, message);
                    }
                }
            }
        });

        if let UpdateCheck::Done(Ok(CheckOutcome::UpdateAvailable(info))) = &self.update_check {
            let info = info.clone();
            ui.horizontal(|ui| self.update_install_row(ui, info));
        }
    }

    /// Renders the "Install update" button and its status line. The install
    /// runs on a worker thread that wakes the UI when it has a result; on
    /// success the installer quits GameFrame (closing this window) and starts
    /// it again.
    fn update_install_row(&mut self, ui: &mut egui::Ui, info: UpdateInfo) {
        self.update_install.expire(Instant::now());
        if matches!(self.update_install, UpdateInstall::Started(_)) {
            // Nothing else wakes the UI if the installer never quits GameFrame.
            ui.ctx().request_repaint_after(Duration::from_secs(5));
        }
        if let UpdateInstall::Running(receiver) = &self.update_install {
            match receiver.try_recv() {
                Ok(Ok(())) => {
                    self.update_install = UpdateInstall::Started(Instant::now());
                }
                Ok(Err(e)) => self.update_install = UpdateInstall::Failed(e),
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.update_install = UpdateInstall::Lost;
                }
            }
        }

        let busy = matches!(
            self.update_install,
            UpdateInstall::Running(_) | UpdateInstall::Started(_)
        );
        let label = format!("Install v{}", info.version);
        if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
            let (sender, receiver) = std::sync::mpsc::channel();
            let ctx = ui.ctx().clone();
            let started = crate::update_ui::spawn_install(info, move |result| {
                // With the window gone nobody reads the channel; a failure is
                // then shown in a box so it is never only in the log.
                if let Err(std::sync::mpsc::SendError(Err(e))) = sender.send(result) {
                    crate::message_box::show_warning_box(
                        &crate::update_install::install_error_message(&e),
                    );
                }
                ctx.request_repaint();
            });
            self.update_install = match started {
                Ok(()) => UpdateInstall::Running(receiver),
                Err(SpawnError::Busy) => UpdateInstall::Busy,
                Err(SpawnError::ThreadFailed) => UpdateInstall::NotStarted,
            };
        }
        match &self.update_install {
            UpdateInstall::Idle => {}
            UpdateInstall::Running(_) => {
                ui.spinner();
                ui.label("Downloading the update...");
            }
            UpdateInstall::Started(_) => {
                ui.label("Installing; GameFrame restarts in a moment.");
            }
            UpdateInstall::TimedOut => {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    "The installer didn't finish. Try installing again.",
                );
            }
            UpdateInstall::Failed(e) => {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    crate::update_install::install_error_message(e),
                );
            }
            UpdateInstall::Busy => {
                ui.label("An update is already being installed.");
            }
            UpdateInstall::NotStarted => {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    "Couldn't start the update install.",
                );
            }
            UpdateInstall::Lost => {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    "Installing the update failed: it ended unexpectedly",
                );
            }
        }
    }

    /// Renders the Zone section: a monitor-outline thumbnail with the Zone
    /// drawn on it (or "No zone set"), a red reason when the Zone can't be
    /// resolved on the current monitor (see [`zone_view`]), and a
    /// "Draw zone"/"Edit zone" button that closes Settings to open the
    /// zone-editor overlay (blank or pre-filled).
    fn zone_section(&mut self, ui: &mut egui::Ui) {
        const THUMBNAIL_SIZE: egui::Vec2 = egui::Vec2::new(128.0, 72.0);
        let fill_color = egui::Color32::from_rgb(90, 170, 250);
        let view = zone_view(self.state.zone.as_ref(), &self.monitor);

        ui.horizontal(|ui| {
            let (outline_rect, _response) =
                ui.allocate_exact_size(THUMBNAIL_SIZE, egui::Sense::hover());
            let painter = ui.painter();
            painter.rect_stroke(
                outline_rect,
                2.0,
                egui::Stroke::new(1.0, egui::Color32::DARK_GRAY),
                egui::StrokeKind::Outside,
            );
            match &view {
                ZoneView::Resolved(region) => {
                    let highlight_rect = egui::Rect::from_min_size(
                        outline_rect.min
                            + egui::vec2(region.x * THUMBNAIL_SIZE.x, region.y * THUMBNAIL_SIZE.y),
                        egui::vec2(
                            region.width * THUMBNAIL_SIZE.x,
                            region.height * THUMBNAIL_SIZE.y,
                        ),
                    );
                    painter.rect_filled(highlight_rect, 0.0, fill_color);
                }
                ZoneView::Unresolvable(_) => {
                    // Cross out the thumbnail in red so an unresolvable Zone
                    // is visible rather than silently drawing nothing.
                    painter.line_segment(
                        [outline_rect.left_top(), outline_rect.right_bottom()],
                        egui::Stroke::new(1.5, egui::Color32::RED),
                    );
                    painter.line_segment(
                        [outline_rect.right_top(), outline_rect.left_bottom()],
                        egui::Stroke::new(1.5, egui::Color32::RED),
                    );
                }
                ZoneView::NotSet => {}
            }

            ui.vertical(|ui| {
                match &self.state.zone {
                    Some(zone) => {
                        ui.monospace(format!(
                            "({}, {}, {}x{})",
                            zone.x, zone.y, zone.width, zone.height
                        ));
                    }
                    None => {
                        ui.label("No zone set");
                        ui.colored_label(egui::Color32::YELLOW, ZONE_SECTION_REQUIRED_NOTE);
                    }
                }
                if let ZoneView::Unresolvable(reason) = &view {
                    ui.colored_label(egui::Color32::RED, format!("Unavailable: {reason}"))
                        .on_hover_text("The zone doesn't fit the current monitor. Edit it to fix.");
                }
                ui.horizontal(|ui| {
                    let label = if self.state.zone.is_some() {
                        "Edit zone"
                    } else {
                        "Draw zone"
                    };
                    if ui.button(label).clicked() {
                        *self.exit.borrow_mut() = Some(SettingsExit::EditZone(
                            self.state.clone(),
                            self.pending_status_messages.clone(),
                        ));
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if self.state.zone.is_some() && ui.button("Clear").clicked() {
                        // Like an overlay Save, Clear persists immediately so
                        // disk and Settings never disagree about the Zone.
                        match persist_zone(&self.config_dir, None) {
                            Ok(_) => self.state.clear_zone(),
                            Err(e) => {
                                log!("failed to clear the zone: {e}");
                                self.pending_status_messages =
                                    vec![format!("couldn't clear the zone: {e}")];
                            }
                        }
                    }
                });
            });
        });
    }

    /// Installs/uninstalls [`Self::keyboard_hook`] to track `state.capturing`'s
    /// `Some`/`None` transitions: installed the moment a capture starts,
    /// uninstalled the moment it ends for any reason (successful capture,
    /// explicit cancel/Escape) via the guard's `Drop`. Called from
    /// [`eframe::App::ui`] -- both once at the very top of the frame, and
    /// again immediately after [`Self::hotkey_row`] runs (see the call site's
    /// comment for why: closes the one-frame install gap between a "Set
    /// hotkey" click and the hook actually going live). A no-op most calls,
    /// since it only ever acts when presence (`Option::is_some`) disagrees
    /// with `capturing`, not on every call a capture happens to still be
    /// active.
    ///
    /// While a capture is active, also polls
    /// [`keyboard_hook::take_suppressed_ctrl_escape`] and completes the
    /// capture directly when it reports the hook just swallowed a physical
    /// Ctrl+Escape press -- see
    /// [`SettingsState::complete_capture_with_ctrl_escape_if_suppressed`]'s
    /// doc comment for why that combo can never arrive as a normal
    /// `egui::Event::Key`.
    ///
    /// Doesn't need to separately cover the Settings window closing while
    /// still capturing -- that path is instead covered by `keyboard_hook`
    /// simply being a field of `SettingsApp` itself: when the window
    /// closes, the whole struct (and this field with it) drops regardless
    /// of what state it was left in, uninstalling the hook exactly the
    /// same way.
    fn sync_keyboard_hook(&mut self) {
        if self.state.capturing.is_some() {
            if self.keyboard_hook.is_none() {
                self.keyboard_hook = keyboard_hook::KeyboardHookGuard::install();
                // Known, intentional tradeoff: if `install()` returns `None`
                // (e.g. some restricted/sandboxed environment), this branch
                // retries every call for the rest of the capture session,
                // since `self.keyboard_hook` stays `None`. Harmless but
                // wasteful; avoiding it would need a separate "already tried
                // and failed" flag.
            }
            self.state.complete_capture_with_ctrl_escape_if_suppressed(
                keyboard_hook::take_suppressed_ctrl_escape(),
            );
        } else {
            self.keyboard_hook = None;
        }
    }
}

/// Builds the Save result. Ticks once more first, so a game-cursor hotkey
/// toggle that landed after this frame's tick (but before the click was
/// handled) is adopted and not overwritten by the stale checkbox value.
fn save_exit(
    state: &mut SettingsState,
    on_tick: &mut dyn FnMut(TickContext) -> Option<bool>,
) -> SettingsExit {
    let context = if state.capturing.is_some() {
        TickContext::SettingsCapturing
    } else {
        TickContext::Settings
    };
    if let Some(enabled) = on_tick(context) {
        state.apply_external_cursor_enabled(enabled);
    }
    SettingsExit::Saved(state.to_config(), state.start_with_windows)
}

impl eframe::App for SettingsApp<'_> {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.sync_keyboard_hook();
        // The flag tells the app to swallow hotkey presses during a capture,
        // so pressing a bound combo to rebind it doesn't also run its action.
        // A game-cursor hotkey press while Settings is open changed
        // `cursor.enabled` on disk: show it in the checkbox so Save keeps it.
        let context = if self.state.capturing.is_some() {
            TickContext::SettingsCapturing
        } else {
            TickContext::Settings
        };
        if let Some(enabled) = (self.on_tick)(context) {
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
                    ui.colored_label(egui::Color32::YELLOW, message);
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
                            ui.colored_label(egui::Color32::YELLOW, ZONE_REQUIRED_NOTE);
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

    fn game(path: &str) -> TrackedGame {
        TrackedGame::new_default(std::path::PathBuf::from(path), path.to_string())
    }

    #[test]
    fn install_started_times_out_after_the_timeout_and_not_before() {
        let start = Instant::now();
        let just_before = start + INSTALL_QUIT_TIMEOUT - Duration::from_millis(1);

        let mut state = UpdateInstall::Started(start);
        state.expire(just_before);
        assert!(matches!(state, UpdateInstall::Started(_)));

        state.expire(start + INSTALL_QUIT_TIMEOUT);
        assert!(matches!(state, UpdateInstall::TimedOut));
    }

    #[test]
    fn expire_leaves_other_install_states_alone() {
        let mut state = UpdateInstall::Idle;
        state.expire(Instant::now() + INSTALL_QUIT_TIMEOUT * 2);
        assert!(matches!(state, UpdateInstall::Idle));
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
    fn save_adopts_a_cursor_toggle_reported_by_the_final_tick() {
        let mut state = SettingsState::from_config(&Config::default());
        assert!(!state.cursor.enabled);

        let mut contexts = Vec::new();
        let mut on_tick = |context| {
            contexts.push(context);
            Some(true)
        };
        let SettingsExit::Saved(config, _) = save_exit(&mut state, &mut on_tick) else {
            panic!("save_exit must produce Saved");
        };
        assert!(config.cursor.enabled);
        assert_eq!(contexts.len(), 1);
    }

    #[test]
    fn save_keeps_the_checkbox_when_the_final_tick_reports_nothing() {
        let mut state = SettingsState::from_config(&Config::default());
        state.cursor.enabled = true;
        let SettingsExit::Saved(config, _) = save_exit(&mut state, &mut |_| None) else {
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
    fn live_apply_failure_message_names_the_game_and_explains_the_fallback() {
        let message = live_apply_failure_message("Thing");

        assert_eq!(
            message,
            "couldn't reposition Thing live -- the saved zone will be used the next time it launches or you apply it"
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
    fn format_captured_hotkey_with_modifiers_and_a_letter_key_uses_gameframes_stored_string_convention()
     {
        let modifiers = egui::Modifiers {
            ctrl: true,
            alt: true,
            ..egui::Modifiers::NONE
        };

        let formatted = format_captured_hotkey(modifiers, egui::Key::B);

        assert_eq!(formatted, Some("Ctrl+Alt+B".to_string()));
    }

    #[test]
    fn format_captured_hotkey_rejects_a_modifier_only_press_as_an_incomplete_capture() {
        let modifiers = egui::Modifiers {
            ctrl: true,
            ..egui::Modifiers::NONE
        };

        let formatted = format_captured_hotkey(modifiers, egui::Key::ControlLeft);

        assert_eq!(formatted, None);
    }

    #[test]
    fn format_captured_hotkey_with_no_modifiers_is_just_the_key_name() {
        let formatted = format_captured_hotkey(egui::Modifiers::NONE, egui::Key::T);

        assert_eq!(formatted, Some("T".to_string()));
    }

    #[test]
    fn key_to_code_maps_a_letter_key_to_its_physical_code_counterpart() {
        assert_eq!(key_to_code(egui::Key::B), Some(Code::KeyB));
    }

    #[test]
    fn build_captured_hotkey_constructs_a_hotkey_directly_from_modifiers_and_physical_key() {
        let modifiers = egui::Modifiers {
            ctrl: true,
            alt: true,
            ..egui::Modifiers::NONE
        };

        let hotkey = build_captured_hotkey(modifiers, egui::Key::B);

        assert_eq!(
            hotkey,
            Some(HotKey::new(
                Some(Modifiers::CONTROL | Modifiers::ALT),
                Code::KeyB
            ))
        );
    }

    #[test]
    fn build_captured_hotkey_returns_none_for_a_physical_modifier_only_press() {
        let modifiers = egui::Modifiers {
            ctrl: true,
            ..egui::Modifiers::NONE
        };

        let hotkey = build_captured_hotkey(modifiers, egui::Key::ControlLeft);

        assert_eq!(hotkey, None);
    }

    #[test]
    fn build_captured_hotkey_returns_none_for_a_key_outside_gameframes_hotkey_vocabulary() {
        // `Copy` has no `Code` counterpart in `key_to_code`'s vocabulary, mirroring
        // `key_display_name`'s rejection of the same key.
        let hotkey = build_captured_hotkey(egui::Modifiers::NONE, egui::Key::Copy);

        assert_eq!(hotkey, None);
    }

    #[test]
    fn format_captured_hotkey_only_completes_a_capture_that_also_constructs_a_valid_hotkey() {
        // Every key GameFrame's capture button can actually complete with must
        // also successfully construct a `global_hotkey::hotkey::HotKey`, and
        // the two must describe the same chord (same modifiers, same physical
        // key), not merely both succeed independently.
        let modifiers = egui::Modifiers {
            ctrl: true,
            shift: true,
            ..egui::Modifiers::NONE
        };

        let combo = format_captured_hotkey(modifiers, egui::Key::Q);
        let hotkey = build_captured_hotkey(modifiers, egui::Key::Q);

        assert_eq!(combo, Some("Ctrl+Shift+Q".to_string()));
        assert_eq!(
            hotkey,
            Some(HotKey::new(
                Some(Modifiers::CONTROL | Modifiers::SHIFT),
                Code::KeyQ
            ))
        );
    }

    #[test]
    fn key_display_name_excludes_enter_and_space_because_they_are_unbindable_via_the_capture_button()
     {
        // Enter/Space are consumed as a click on the focused "Set hotkey"/"Cancel"
        // button before the raw `Event::Key` ever reaches `handle_key_event`, so
        // they can never actually complete a capture -- see the doc comment on
        // `key_display_name`.
        assert_eq!(key_display_name(egui::Key::Enter), None);
        assert_eq!(key_display_name(egui::Key::Space), None);
    }

    fn test_monitor() -> MonitorGeometry {
        MonitorGeometry {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }
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
    fn from_config_carries_the_tracked_games_list_through() {
        let mut config = Config::default();
        config.tracked_games.push(TrackedGame::new_default(
            std::path::PathBuf::from("C:\\Games\\thing.exe"),
            "Thing".to_string(),
        ));

        let state = SettingsState::from_config(&config);

        assert_eq!(state.tracked_games, config.tracked_games);
    }

    #[test]
    fn to_config_carries_edited_tracked_games_through() {
        let mut state = SettingsState::from_config(&Config::default());
        state.tracked_games.push(TrackedGame::new_default(
            std::path::PathBuf::from("C:\\Games\\thing.exe"),
            "Thing".to_string(),
        ));
        state.tracked_games[0].auto_apply = false;

        let config = state.to_config();

        assert_eq!(config.tracked_games.len(), 1);
        assert!(!config.tracked_games[0].auto_apply);
    }

    #[test]
    fn remove_tracked_game_removes_the_entry_at_the_given_index_without_confirmation() {
        let mut state = SettingsState::from_config(&Config::default());
        state.tracked_games.push(TrackedGame::new_default(
            std::path::PathBuf::from("C:\\Games\\a.exe"),
            "A".to_string(),
        ));
        state.tracked_games.push(TrackedGame::new_default(
            std::path::PathBuf::from("C:\\Games\\b.exe"),
            "B".to_string(),
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
    fn normalize_region_expresses_a_region_as_proportions_of_the_monitor() {
        let monitor = test_monitor();
        let region = Region {
            x: 960,
            y: 0,
            width: 960,
            height: 1080,
        };

        let normalized = normalize_region(&region, &monitor);

        assert_eq!(
            normalized,
            NormalizedRegion {
                x: 0.5,
                y: 0.0,
                width: 0.5,
                height: 1.0
            }
        );
    }

    #[test]
    fn normalize_region_accounts_for_a_monitor_with_a_non_zero_origin() {
        let monitor = MonitorGeometry {
            x: -2560,
            y: 0,
            width: 2560,
            height: 1440,
        };
        let region = Region {
            x: -1280,
            y: 0,
            width: 1280,
            height: 1440,
        };

        let normalized = normalize_region(&region, &monitor);

        assert_eq!(
            normalized,
            NormalizedRegion {
                x: 0.5,
                y: 0.0,
                width: 0.5,
                height: 1.0
            }
        );
    }

    #[test]
    fn centered_window_position_centers_a_window_within_the_monitor() {
        let monitor = test_monitor();

        let position = centered_window_position(&monitor, [440.0, 560.0]);

        assert_eq!(position, [740.0, 260.0]);
    }

    #[test]
    fn centered_window_position_accounts_for_a_monitor_with_a_non_zero_origin() {
        let monitor = MonitorGeometry {
            x: -2560,
            y: 0,
            width: 2560,
            height: 1440,
        };

        let position = centered_window_position(&monitor, [440.0, 560.0]);

        assert_eq!(position, [-1500.0, 440.0]);
    }

    #[test]
    fn zone_dependent_tabs_are_disabled_without_a_zone_and_enabled_with_one() {
        assert!(!zone_dependent_tabs_enabled(None));
        let zone = Zone {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        assert!(zone_dependent_tabs_enabled(Some(&zone)));
    }

    #[test]
    fn zone_view_is_not_set_when_there_is_no_zone() {
        assert_eq!(zone_view(None, &test_monitor()), ZoneView::NotSet);
    }

    #[test]
    fn zone_view_normalizes_a_zone_that_fits_the_monitor() {
        let zone = Zone {
            x: 960,
            y: 0,
            width: 960,
            height: 1080,
        };

        let view = zone_view(Some(&zone), &test_monitor());

        assert_eq!(
            view,
            ZoneView::Resolved(NormalizedRegion {
                x: 0.5,
                y: 0.0,
                width: 0.5,
                height: 1.0
            })
        );
    }

    #[test]
    fn zone_view_surfaces_a_reason_for_a_zone_that_does_not_fit_the_monitor() {
        // Simulates a Zone persisted while GameFrame ran on a different
        // (e.g. 4K) monitor: its origin lies outside `test_monitor`'s bounds.
        let zone = Zone {
            x: 5000,
            y: 5000,
            width: 200,
            height: 100,
        };

        let view = zone_view(Some(&zone), &test_monitor());

        assert!(
            matches!(view, ZoneView::Unresolvable(ref reason) if !reason.is_empty()),
            "got {view:?}"
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
