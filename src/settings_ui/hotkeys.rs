//! Hotkey capture: egui keys to GameFrame's hotkey vocabulary, and the
//! hotkey rows.

use super::{HotkeyField, SettingsApp};
use crate::keyboard_hook;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};

/// `true` for egui's eight physical modifier-key variants (Shift/Control/Alt/
/// Super, left or right). A press of one of these alone must never complete
/// a hotkey capture -- only combined with a subsequent non-modifier key.
pub(super) fn is_physical_modifier_key(key: egui::Key) -> bool {
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
pub(super) fn format_captured_hotkey(modifiers: egui::Modifiers, key: egui::Key) -> Option<String> {
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

impl SettingsApp<'_> {
    pub(super) fn hotkey_row(&mut self, ui: &mut egui::Ui, label: &str, field: HotkeyField) {
        let is_capturing = self.state.capturing == Some(field);

        ui.horizontal(|ui| {
            ui.label(label);

            if is_capturing {
                ui.colored_label(ui.visuals().warn_fg_color, "Press a key... (Esc to cancel)");
            } else {
                let bound = self.state.binding(field);
                ui.monospace(if bound.is_empty() { "Not bound" } else { bound });
            }
            if let Some(reason) = self.state.hotkey_failure(field) {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!("⚠ Not working: {reason}"),
                );
            }

            let set_hotkey_response = ui.button(if is_capturing { "Cancel" } else { "Set hotkey" });
            if set_hotkey_response.clicked() {
                if is_capturing {
                    self.state.cancel_capture();
                } else {
                    self.state.start_capture(field);
                    // The tray process must stop running hotkey actions now,
                    // not with the next frame's tick.
                    crate::settings_link::report_context(self.state.tick_context());
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
                    ui.visuals().error_fg_color,
                    "That key isn't supported -- try another.",
                );
            }
            if let Some(error) = &self.state.duplicate_hotkey_error {
                ui.colored_label(ui.visuals().error_fg_color, error);
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
    /// [`super::SettingsState::complete_capture_with_ctrl_escape_if_suppressed`]'s
    /// doc comment for why that combo can never arrive as a normal
    /// `egui::Event::Key`.
    ///
    /// Doesn't need to separately cover the Settings window closing while
    /// still capturing -- that path is instead covered by `keyboard_hook`
    /// simply being a field of `SettingsApp` itself: when the window
    /// closes, the whole struct (and this field with it) drops regardless
    /// of what state it was left in, uninstalling the hook exactly the
    /// same way.
    pub(super) fn sync_keyboard_hook(&mut self) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
