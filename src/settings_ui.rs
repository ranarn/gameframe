//! Lazily-instantiated egui Settings window: draw/edit the single optional
//! Zone, and assign GameFrame's two global hotkey bindings, persisted via
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

use crate::profile_store::{
    self, Config, HotkeyBindings, ProfileStoreError, TaskbarHideMode, TrackedGame, Zone,
};
use crate::region_calc::{resolve_region, MonitorGeometry, Region};
use crate::autostart::{self, AutostartError};
use crate::keyboard_hook;
use crate::win32;
use crate::zone_editor::PinnedEdges;
use crate::zone_overlay;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

/// The editable state backing the Settings window: the single optional
/// [`Zone`] and the two hotkey binding strings (empty = unbound).
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
    /// site hides it (issue #42) -- `FullyHidden` (new default, no
    /// hover-reveal) or `HoverReveal` (today's pre-#42 native
    /// `SHAppBarMessage` behavior, opt-in). Set via a radio choice next to
    /// the global hotkeys in the Settings window.
    pub taskbar_hide_mode: TaskbarHideMode,
    /// Whether applying the Zone also hides the taskbar -- the global
    /// "Hide taskbar when the Zone is applied" checkbox (issue #49).
    pub hide_taskbar_on_apply: bool,
    /// The "Start with Windows" checkbox. Not part of [`Config`] -- the
    /// registry is its source of truth (see [`crate::autostart`]) -- so
    /// [`Self::from_config`] leaves it `false` and [`run_settings_window`]
    /// fills it from the registry; Save writes it back only if it changed.
    pub start_with_windows: bool,
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
    /// hotkeys (`None`) become empty strings.
    pub fn from_config(config: &Config) -> Self {
        Self {
            zone: config.zone,
            toggle_borderless: config.hotkeys.toggle_borderless.clone().unwrap_or_default(),
            toggle_taskbar_autohide: config.hotkeys.toggle_taskbar_autohide.clone().unwrap_or_default(),
            capturing: None,
            unsupported_key_pressed: false,
            duplicate_hotkey_error: None,
            tracked_games: config.tracked_games.clone(),
            taskbar_hide_mode: config.taskbar_hide_mode,
            hide_taskbar_on_apply: config.hide_taskbar_on_apply,
            start_with_windows: false,
        }
    }

    /// Builds the [`Config`] to persist. A blank (or whitespace-only) hotkey
    /// field is treated as unbound (`None`).
    pub fn to_config(&self) -> Config {
        Config {
            zone: self.zone,
            hotkeys: HotkeyBindings {
                toggle_borderless: non_empty(&self.toggle_borderless),
                toggle_taskbar_autohide: non_empty(&self.toggle_taskbar_autohide),
            },
            tracked_games: self.tracked_games.clone(),
            taskbar_hide_mode: self.taskbar_hide_mode,
            hide_taskbar_on_apply: self.hide_taskbar_on_apply,
        }
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
    /// normal `egui::Event::Key` for [`Self::handle_key_event`] to observe
    /// (issue #40's CRITICAL review finding). A no-op when
    /// `ctrl_escape_suppressed` is `false`, or when nothing is currently
    /// capturing.
    ///
    /// Deliberately reuses [`Self::handle_key_event`]'s existing, already-
    /// tested Ctrl+Escape formatting/duplicate-check logic rather than
    /// duplicating it -- takes the already-polled flag as a plain `bool`
    /// (see [`crate::keyboard_hook::take_suppressed_ctrl_escape`]) so this
    /// stays pure and unit-testable without a real OS-level hook; only the
    /// polling call itself is OS-facing.
    pub fn complete_capture_with_ctrl_escape_if_suppressed(&mut self, ctrl_escape_suppressed: bool) {
        if ctrl_escape_suppressed {
            self.handle_key_event(egui::Modifiers { ctrl: true, ..egui::Modifiers::NONE }, egui::Key::Escape);
        }
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
const SETTINGS_WINDOW_SIZE: [f32; 2] = [440.0, 560.0];

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
/// re-apply attempt (issue #33) failed -- e.g. the window couldn't be
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
fn persist_zone(config_dir: &Path, zone: Option<Zone>) -> Result<Config, SettingsUiError> {
    let config = profile_store::load(config_dir).map_err(SettingsUiError::Config)?;
    let updated = config_with_zone(config, zone);
    profile_store::save(config_dir, &updated).map_err(SettingsUiError::Config)?;
    Ok(updated)
}

/// Attempts issue #33's live re-apply for a single tracked game, via
/// [`win32::resolve_running_window`] + [`win32::live_reapply_running_window`].
/// Returns `Some(message)` only when an attempt was actually made and failed
/// (the game was running, but repositioning it hit an OS-level error) -- a
/// game that isn't currently running is silently skipped (`None`) *before*
/// the Zone's region is even resolved: its setting still saved normally and
/// will apply on its next launch/hotkey/tray-apply. This ordering matters --
/// a not-running game must never surface a region-resolution error as a
/// live-apply failure.
///
/// OS-facing (delegates to [`win32::resolve_running_window`]/
/// [`win32::live_reapply_running_window`], which perform the real toolhelp
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
    // never actually running (issue #33's iteration-3 review fix).
    let hwnd = win32::resolve_running_window(&game.exe_path)?;

    let region = match resolve_region(zone, monitor) {
        Ok(region) => region,
        Err(e) => {
            eprintln!("live re-apply: failed to resolve the Zone for {}: {e}", game.display_name);
            return Some(live_apply_failure_message(&game.display_name));
        }
    };

    match win32::live_reapply_running_window(
        hwnd,
        &game.display_name,
        hide_taskbar_on_apply,
        taskbar_hide_mode,
        region,
    ) {
        win32::LiveApplyOutcome::Applied => None,
        win32::LiveApplyOutcome::Failed(e) => {
            eprintln!("live re-apply failed for {}: {e}", game.display_name);
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
        // `Escape` is bindable as a main key ONLY when combined with a held
        // modifier (e.g. `Ctrl+Escape`) -- a bare, unmodified Escape press is
        // intercepted earlier by `handle_key_event`'s cancel-capture check
        // (see issue #40) and never reaches this function at all.
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
/// apart on which physical keys GameFrame's capture widget supports. See
/// `docs/research/hotkey-capture-widget.md` section 3 for the egui-`Key`-vs-
/// `Code` cross-reference this table is built from. Returns `None` for keys
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
/// completed it) -- satisfying issue #29's acceptance criterion that the
/// captured chord construct a `HotKey` via `HotKey::new(mods, code)` rather
/// than solely through a later `FromStr` round-trip of the persisted string
/// (that round-trip still happens separately, in `hotkeys::resolve_bindings`,
/// on every app startup -- unavoidable since the persisted config is a plain
/// string, not a living `HotKey`).
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
    Some(HotKey::new(Some(modifiers_to_hotkey_modifiers(modifiers)), code))
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
    // Validate via a directly-constructed `HotKey` first (issue #29) -- a chord
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
/// live-re-apply status messages -- issue #33's
/// [`SettingsApp::pending_status_messages`] -- so neither is lost while the
/// overlay is open).
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
    /// pending live-re-apply status messages ([`SettingsApp::pending_status_messages`],
    /// issue #33) -- so a message surfaced by a Zone commit made
    /// just before "Edit zone" was clicked isn't lost when this
    /// session's `SettingsApp` is torn down; [`run_settings_window`] carries
    /// it forward as the next session's initial status messages.
    EditZone(SettingsState, Vec<String>),
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
/// Also threads issue #33's non-blocking live-re-apply status messages
/// across that same close-then-reopen boundary: `status_messages` starts
/// empty, is replaced wholesale by whatever [`SettingsExit::EditZone`]
/// carried out of the just-closed session (either its untouched
/// carried-forward messages, or a fresh set if the overlay's save fired
/// trigger 3's cascade), and is handed to the next [`run_settings_frame`]
/// call as its `initial_status_messages` -- so a status line surfaced right
/// before "Edit zone" was clicked survives the round trip through
/// the overlay instead of vanishing with the torn-down `SettingsApp`.
///
/// OS/GPU-facing: constructs a real egui/wgpu context for the lifetime of
/// each iteration and fully drops it before the next (or before returning).
/// Not exercised by this crate's automated tests -- see module docs.
pub fn run_settings_window(config_dir: &Path) -> Result<(), SettingsUiError> {
    let config = profile_store::load(config_dir).map_err(SettingsUiError::Config)?;
    let mut state = SettingsState::from_config(&config);
    let started_with_windows = autostart::is_enabled();
    state.start_with_windows = started_with_windows;
    let monitor = primary_monitor_geometry();
    let mut status_messages: Vec<String> = Vec::new();

    loop {
        match run_settings_frame(config_dir, state, monitor, status_messages)? {
            SettingsExit::Saved(config, start_with_windows) => {
                profile_store::save(config_dir, &config).map_err(SettingsUiError::Config)?;
                // Only touch the registry on an actual change, so saving
                // unrelated settings never re-points an existing entry at
                // whichever exe happens to be running (e.g. a dev build).
                if start_with_windows != started_with_windows {
                    autostart::set_enabled(start_with_windows).map_err(SettingsUiError::Autostart)?;
                }
                return Ok(());
            }
            SettingsExit::Cancelled => return Ok(()),
            SettingsExit::EditZone(mut latest_state, latest_status_messages) => {
                // Carried forward as-is unless the overlay actually commits a
                // new zone below (trigger 3 only fires on an accepted save,
                // per this issue's "commit only" rule) -- cancelling/failing
                // to open the overlay is not itself a commit, so whatever
                // status messages were already showing must survive it.
                status_messages = latest_status_messages;
                match zone_overlay::run_zone_editor_overlay(
                    latest_state.zone,
                    PinnedEdges::NONE,
                    latest_state.taskbar_hide_mode,
                ) {
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
                                eprintln!("failed to save the zone: {e}");
                                status_messages = vec![format!("couldn't save the zone: {e}")];
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("failed to open the zone editor overlay: {e}"),
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
/// [`SettingsApp::pending_status_messages`] (issue #33) -- carried over from
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
) -> Result<SettingsExit, SettingsUiError> {
    let exit: Rc<RefCell<Option<SettingsExit>>> = Rc::new(RefCell::new(None));
    let exit_for_app = Rc::clone(&exit);
    let config_dir = config_dir.to_path_buf();

    // `.with_position(...)` here only ever takes effect on a genuine
    // first-ever launch (no saved `WindowSettings` yet). Once eframe has
    // persisted a position, `egui_winit::WindowSettings::initialize_viewport_builder`
    // unconditionally calls `.with_position(...)` again with the *restored*
    // position, so this default is always overridden on subsequent launches
    // -- it never fights with the position/size/maximized/fullscreen
    // restore that eframe's own persistence machinery already handles.
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("GameFrame Settings")
            .with_inner_size(SETTINGS_WINDOW_SIZE)
            .with_position(centered_window_position(&monitor, SETTINGS_WINDOW_SIZE))
            .with_icon(window_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "GameFrame Settings",
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(SettingsApp {
                config_dir,
                state,
                monitor,
                exit: exit_for_app,
                pending_status_messages: initial_status_messages,
                keyboard_hook: None,
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
    egui::IconData { rgba: gameframe::icon::mark_rgba(SIZE), width: SIZE, height: SIZE }
}

/// The live egui app backing the Settings window. Torn down entirely when
/// [`run_settings_window`] returns -- nothing here outlives that call.
///
/// Also owns issue #33's transient live-re-apply status messages
/// ([`pending_status_messages`](SettingsApp::pending_status_messages)),
/// which -- unlike `state` -- deliberately don't survive a
/// [`SettingsExit::EditZone`] round trip unchanged: they're carried
/// forward by [`run_settings_window`] as plain `Vec<String>` data, not as
/// part of this struct, since this struct itself is fully torn down and
/// reconstructed by [`run_settings_frame`] on each session.
struct SettingsApp {
    /// Where the Zone is persisted immediately on Clear (the overlay-Save
    /// persist lives in [`run_settings_window`]).
    config_dir: std::path::PathBuf,
    state: SettingsState,
    monitor: MonitorGeometry,
    exit: Rc<RefCell<Option<SettingsExit>>>,
    /// Zero or more non-blocking status messages from the most recent
    /// commit-triggered live re-apply attempt (issue #33) -- e.g.
    /// [`live_apply_failure_message`] for a tracked game whose window
    /// couldn't be repositioned. Purely transient UI feedback: not part of
    /// [`SettingsState`] (never round-trips through `to_config`/persist),
    /// and replaced wholesale -- not accumulated -- on each new
    /// commit-triggered attempt, per this issue's acceptance criteria.
    pending_status_messages: Vec<String>,
    /// `Some(guard)` while `state.capturing` is `Some` (kept in sync each
    /// frame by [`Self::sync_keyboard_hook`]) -- suppresses the physical
    /// Ctrl+Escape combination so Windows doesn't steal it for the Start
    /// menu while a hotkey capture is listening (issue #40). `None`
    /// otherwise; dropping it (either via that sync, or via `SettingsApp`
    /// itself being torn down when the Settings window closes) uninstalls
    /// the hook, per [`keyboard_hook::KeyboardHookGuard`]'s RAII contract.
    keyboard_hook: Option<keyboard_hook::KeyboardHookGuard>,
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
            ui.label("No tracked games yet -- use \"Apply to Window\" from the tray menu to add one.");
            return;
        }

        let mut to_remove: Option<usize> = None;
        let last_index = self.state.tracked_games.len() - 1;
        for (index, game) in self.state.tracked_games.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.label(&game.display_name);

                ui.checkbox(&mut game.auto_apply, "Auto-apply");

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
            let (outline_rect, _response) = ui.allocate_exact_size(THUMBNAIL_SIZE, egui::Sense::hover());
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
                        outline_rect.min + egui::vec2(region.x * THUMBNAIL_SIZE.x, region.y * THUMBNAIL_SIZE.y),
                        egui::vec2(region.width * THUMBNAIL_SIZE.x, region.height * THUMBNAIL_SIZE.y),
                    );
                    painter.rect_filled(highlight_rect, 0.0, fill_color);
                }
                ZoneView::Unresolvable(_) => {
                    // Visible placeholder instead of silently drawing nothing.
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
                        ui.monospace(format!("({}, {}, {}x{})", zone.x, zone.y, zone.width, zone.height));
                    }
                    None => {
                        ui.label("No zone set");
                    }
                }
                if let ZoneView::Unresolvable(reason) = &view {
                    ui.colored_label(egui::Color32::RED, format!("Unavailable: {reason}"))
                        .on_hover_text("The zone doesn't fit the current monitor. Edit it to fix.");
                }
                ui.horizontal(|ui| {
                    let label = if self.state.zone.is_some() { "Edit zone" } else { "Draw zone" };
                    if ui.button(label).clicked() {
                        *self.exit.borrow_mut() =
                            Some(SettingsExit::EditZone(self.state.clone(), self.pending_status_messages.clone()));
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if self.state.zone.is_some() && ui.button("Clear").clicked() {
                        // Like an overlay Save, Clear persists immediately so
                        // disk and Settings never disagree about the Zone.
                        match persist_zone(&self.config_dir, None) {
                            Ok(_) => self.state.clear_zone(),
                            Err(e) => {
                                eprintln!("failed to clear the zone: {e}");
                                self.pending_status_messages = vec![format!("couldn't clear the zone: {e}")];
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
    /// hotkey" click and the hook actually going live, an issue #40 review
    /// finding). A no-op most calls, since it only ever acts when presence
    /// (`Option::is_some`) disagrees with `capturing`, not on every call a
    /// capture happens to still be active.
    ///
    /// While a capture is active, also polls
    /// [`keyboard_hook::take_suppressed_ctrl_escape`] and completes the
    /// capture directly when it reports the hook just swallowed a physical
    /// Ctrl+Escape press -- see
    /// [`SettingsState::complete_capture_with_ctrl_escape_if_suppressed`]'s
    /// doc comment for why that combo can never arrive as a normal
    /// `egui::Event::Key` (issue #40's CRITICAL review finding).
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
                // Known, intentional tradeoff (issue #40, not fixed -- out of
                // scope): if `install()` returns `None` (e.g. some
                // restricted/sandboxed environment), this branch retries
                // every call for the rest of the capture session, since
                // `self.keyboard_hook` stays `None`. Harmless but wasteful;
                // would need a separate "already tried and failed" flag to
                // avoid, not required by this issue's acceptance criteria.
            }
            self.state.complete_capture_with_ctrl_escape_if_suppressed(keyboard_hook::take_suppressed_ctrl_escape());
        } else {
            self.keyboard_hook = None;
        }
    }
}

impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.sync_keyboard_hook();

        egui::CentralPanel::default().show(ui, |ui| {
            // Issue #33's non-blocking status line(s): zero or more messages
            // from the most recent commit-triggered live re-apply attempt.
            // Rendered near the top so they're visible without scrolling
            // regardless of which commit point (currently only the overlay's
            // zone Save) produced them.
            if !self.pending_status_messages.is_empty() {
                for message in &self.pending_status_messages {
                    ui.colored_label(egui::Color32::YELLOW, message);
                }
                ui.separator();
            }

            ui.heading("Zone");
            self.zone_section(ui);
            ui.separator();
            ui.heading("Global hotkeys");
            self.hotkey_row(ui, "Toggle borderless:", HotkeyField::ToggleBorderless);
            self.hotkey_row(ui, "Toggle taskbar autohide:", HotkeyField::ToggleTaskbarAutohide);
            // Re-sync immediately after both hotkey rows: a "Set hotkey" click
            // just above sets `state.capturing = Some(..)` within this same
            // `ui()` call, but the sync at the top of this function already
            // ran against the *old* `None` state. Without this second call the
            // hook wouldn't install until the following frame, leaving a
            // one-frame window where physical Ctrl+Escape still reaches
            // Windows' Start-menu shortcut (issue #40 MEDIUM review finding).
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
            ui.heading("Games");
            self.games_section(ui);

            ui.separator();
            ui.heading("Startup");
            ui.checkbox(&mut self.state.start_with_windows, "Start with Windows");

            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    *self.exit.borrow_mut() = Some(SettingsExit::Saved(self.state.to_config(), self.state.start_with_windows));
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if ui.button("Cancel").clicked() {
                    *self.exit.borrow_mut() = Some(SettingsExit::Cancelled);
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut on_disk = Config::default();
        on_disk.zone = Some(Zone { x: 1, y: 2, width: 3, height: 4 });
        on_disk.hotkeys.toggle_borderless = Some("Ctrl+B".to_string());
        let new_zone = Zone { x: 100, y: 50, width: 800, height: 600 };

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
        let zone = Zone { x: 10, y: 20, width: 300, height: 200 };

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
        assert!(SettingsState::from_config(&config).hide_taskbar_on_apply);

        config.hide_taskbar_on_apply = false;
        let state = SettingsState::from_config(&config);
        assert!(!state.hide_taskbar_on_apply);
        assert!(!state.to_config().hide_taskbar_on_apply);

        let mut state = state;
        state.hide_taskbar_on_apply = true;
        assert!(state.to_config().hide_taskbar_on_apply);
    }

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
    fn key_to_code_maps_a_letter_key_to_its_physical_code_counterpart() {
        assert_eq!(key_to_code(egui::Key::B), Some(Code::KeyB));
    }

    #[test]
    fn build_captured_hotkey_constructs_a_hotkey_directly_from_modifiers_and_physical_key() {
        let modifiers = egui::Modifiers { ctrl: true, alt: true, ..egui::Modifiers::NONE };

        let hotkey = build_captured_hotkey(modifiers, egui::Key::B);

        assert_eq!(hotkey, Some(HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyB)));
    }

    #[test]
    fn build_captured_hotkey_returns_none_for_a_physical_modifier_only_press() {
        let modifiers = egui::Modifiers { ctrl: true, ..egui::Modifiers::NONE };

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
        // Every key GameFrame's capture button can actually complete with must also
        // successfully construct a `global_hotkey::hotkey::HotKey` -- issue #29's
        // acceptance criterion -- and the two must describe the same chord (same
        // modifiers, same physical key), not merely both succeed independently.
        let modifiers = egui::Modifiers { ctrl: true, shift: true, ..egui::Modifiers::NONE };

        let combo = format_captured_hotkey(modifiers, egui::Key::Q);
        let hotkey = build_captured_hotkey(modifiers, egui::Key::Q);

        assert_eq!(combo, Some("Ctrl+Shift+Q".to_string()));
        assert_eq!(hotkey, Some(HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyQ)));
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
            zone: Some(Zone { x: 10, y: 10, width: 200, height: 100 }),
            hotkeys: HotkeyBindings {
                toggle_borderless: Some("Ctrl+Alt+B".to_string()),
                toggle_taskbar_autohide: Some("Ctrl+Alt+T".to_string()),
            },
            tracked_games: Vec::new(),
            taskbar_hide_mode: TaskbarHideMode::HoverReveal,
            hide_taskbar_on_apply: true,
        };

        let state = SettingsState::from_config(&config);

        assert_eq!(state.zone, Some(Zone { x: 10, y: 10, width: 200, height: 100 }));
        assert_eq!(state.toggle_borderless, "Ctrl+Alt+B");
        assert_eq!(state.toggle_taskbar_autohide, "Ctrl+Alt+T");
        assert_eq!(state.taskbar_hide_mode, TaskbarHideMode::HoverReveal);
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

        assert_eq!(config.hotkeys.toggle_borderless, Some("Ctrl+Alt+B".to_string()));
        assert_eq!(config.hotkeys.toggle_taskbar_autohide, Some("Ctrl+Alt+D".to_string()));
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
        let zone = Zone { x: 1, y: 2, width: 3, height: 4 };
        let config = Config { zone: Some(zone), ..Config::default() };

        let round_tripped = SettingsState::from_config(&config).to_config();

        assert_eq!(round_tripped.zone, Some(zone));
        assert_eq!(SettingsState::from_config(&Config::default()).to_config().zone, None);
    }

    #[test]
    fn set_zone_sets_the_zone_returned_by_the_zone_editor_overlay() {
        let mut state = SettingsState::from_config(&Config::default());

        state.set_zone(Zone { x: 100, y: 50, width: 800, height: 600 });

        assert_eq!(state.zone, Some(Zone { x: 100, y: 50, width: 800, height: 600 }));
        assert_eq!(state.to_config().zone, Some(Zone { x: 100, y: 50, width: 800, height: 600 }));
    }

    #[test]
    fn set_zone_replaces_any_previously_set_zone_rather_than_appending() {
        let mut state = SettingsState::from_config(&Config::default());
        state.zone = Some(Zone { x: 0, y: 0, width: 100, height: 100 });

        state.set_zone(Zone { x: 200, y: 200, width: 300, height: 300 });

        assert_eq!(state.zone, Some(Zone { x: 200, y: 200, width: 300, height: 300 }));
    }

    #[test]
    fn clear_zone_removes_the_zone() {
        let mut state = SettingsState::from_config(&Config::default());
        state.zone = Some(Zone { x: 1, y: 2, width: 3, height: 4 });

        state.clear_zone();

        assert_eq!(state.zone, None);
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
    fn complete_capture_with_ctrl_escape_if_suppressed_records_the_combo_when_the_hook_swallowed_it() {
        // Mirrors `keyboard_hook::keyboard_hook_proc`'s CRITICAL fix: the hook
        // swallows the physical Ctrl+Escape press before it can ever arrive as
        // an `egui::Event::Key`, so this is the only path that can complete
        // the capture for that specific combo.
        let mut state = SettingsState::from_config(&Config::default());
        state.start_capture(HotkeyField::ToggleBorderless);

        state.complete_capture_with_ctrl_escape_if_suppressed(true);

        assert_eq!(state.toggle_borderless, "Ctrl+Escape");
        assert_eq!(state.capturing, None);
    }

    #[test]
    fn complete_capture_with_ctrl_escape_if_suppressed_is_a_no_op_when_the_hook_did_not_swallow_anything() {
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

        let modifiers = egui::Modifiers { ctrl: true, ..egui::Modifiers::NONE };
        state.handle_key_event(modifiers, egui::Key::Escape);

        assert_eq!(state.toggle_borderless, "Ctrl+Escape");
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
    fn centered_window_position_centers_a_window_within_the_monitor() {
        let monitor = test_monitor();

        let position = centered_window_position(&monitor, SETTINGS_WINDOW_SIZE);

        assert_eq!(position, [740.0, 260.0]);
    }

    #[test]
    fn centered_window_position_accounts_for_a_monitor_with_a_non_zero_origin() {
        let monitor = MonitorGeometry { x: -2560, y: 0, width: 2560, height: 1440 };

        let position = centered_window_position(&monitor, SETTINGS_WINDOW_SIZE);

        assert_eq!(position, [-1500.0, 440.0]);
    }

    #[test]
    fn zone_view_is_not_set_when_there_is_no_zone() {
        assert_eq!(zone_view(None, &test_monitor()), ZoneView::NotSet);
    }

    #[test]
    fn zone_view_normalizes_a_zone_that_fits_the_monitor() {
        let zone = Zone { x: 960, y: 0, width: 960, height: 1080 };

        let view = zone_view(Some(&zone), &test_monitor());

        assert_eq!(view, ZoneView::Resolved(NormalizedRegion { x: 0.5, y: 0.0, width: 0.5, height: 1.0 }));
    }

    #[test]
    fn zone_view_surfaces_a_reason_for_a_zone_that_does_not_fit_the_monitor() {
        // Simulates a Zone persisted while GameFrame ran on a different
        // (e.g. 4K) monitor: its origin lies outside `test_monitor`'s bounds.
        let zone = Zone { x: 5000, y: 5000, width: 200, height: 100 };

        let view = zone_view(Some(&zone), &test_monitor());

        assert!(matches!(view, ZoneView::Unresolvable(ref reason) if !reason.is_empty()), "got {view:?}");
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
        state.set_zone(Zone { x: 10, y: 10, width: 300, height: 200 });
        let to_persist = state.to_config();
        profile_store::save(&config_dir, &to_persist).unwrap();

        // "Reopen" Settings: reload from disk and rebuild the view-model.
        let reloaded_config = profile_store::load(&config_dir).unwrap();
        let reopened_state = SettingsState::from_config(&reloaded_config);

        assert_eq!(reopened_state.toggle_borderless, "Ctrl+Alt+B");
        assert_eq!(reopened_state.zone, Some(Zone { x: 10, y: 10, width: 300, height: 200 }));
    }
}
