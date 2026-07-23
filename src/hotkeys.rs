//! Global hotkey registration/dispatch, built on the `global-hotkey` crate.
//!
//! Split the same way as [`crate::win32`]:
//! - Parsing a stored hotkey-binding string and resolving which action a
//!   registered hotkey's id maps back to is pure logic with no OS dependency,
//!   fully unit tested below.
//! - Actually registering a hotkey with the OS
//!   ([`global_hotkey::GlobalHotKeyManager`]) is OS-facing: per the crate's
//!   own docs it requires a live Win32 event loop already pumping on this
//!   thread, which a headless/CI sandbox can't provide -- so [`Hotkeys::register`]
//!   itself is NOT exercised by this crate's automated tests.

use crate::profile_store::HotkeyBindings;
use global_hotkey::hotkey::{HotKey, HotKeyParseError};
use global_hotkey::GlobalHotKeyManager;

/// Actions GameFrame can bind a global hotkey to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    ToggleBorderless,
    /// Toggles taskbar auto-hide on/off via `SHAppBarMessage` -- see
    /// [`crate::win32::toggle_taskbar_autohide`]. Fully independent of
    /// [`HotkeyAction::ToggleBorderless`]'s state; either can be pressed
    /// without the other having been used first.
    ToggleTaskbarAutohide,
}

/// A hotkey binding resolved from its stored string into the parsed
/// [`HotKey`] the `global-hotkey` crate needs, paired with the action it
/// should trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedBinding {
    pub action: HotkeyAction,
    pub hotkey: HotKey,
}

/// Resolves every bound hotkey (the borderless toggle and/or the taskbar
/// auto-hide toggle -- either, both, or neither) from `bindings` into
/// [`ResolvedBinding`]s.
///
/// Free-text hotkey strings (as entered via Settings -- issue #5's Settings
/// UI does no format validation) are parsed with `global_hotkey::hotkey::HotKey`'s
/// own `FromStr` implementation, which already accepts the `"Ctrl+Alt+B"`-style
/// syntax Settings stores (modifiers joined by `+`, main key last -- see the
/// `global-hotkey` crate's own `hotkey` module docs). An unbound hotkey
/// (`None`) is simply skipped rather than erroring -- pressing keys on a
/// fresh install where nothing is bound must not do anything (and can't
/// crash, since nothing gets registered). The two bindings are resolved
/// fully independently of each other -- either, both, or neither may be
/// bound, in any combination.
pub fn resolve_bindings(bindings: &HotkeyBindings) -> Result<Vec<ResolvedBinding>, HotKeyParseError> {
    let mut resolved = Vec::new();
    if let Some(raw) = &bindings.toggle_borderless {
        let hotkey: HotKey = raw.parse()?;
        resolved.push(ResolvedBinding { action: HotkeyAction::ToggleBorderless, hotkey });
    }
    if let Some(raw) = &bindings.toggle_taskbar_autohide {
        let hotkey: HotKey = raw.parse()?;
        resolved.push(ResolvedBinding { action: HotkeyAction::ToggleTaskbarAutohide, hotkey });
    }
    Ok(resolved)
}

/// Returns the [`HotkeyAction`] a registered hotkey id maps to, or `None` if
/// `id` doesn't match any of `bindings` -- e.g. because nothing is bound, or
/// the id belongs to some other hotkey entirely.
pub fn action_for(bindings: &[ResolvedBinding], id: u32) -> Option<HotkeyAction> {
    bindings.iter().find(|b| b.hotkey.id() == id).map(|b| b.action)
}

/// Reasons registering GameFrame's global hotkeys can fail: either the
/// stored binding string wasn't valid `global-hotkey` syntax, or the OS
/// itself refused the registration (e.g. another application already holds
/// that exact key combination).
#[derive(Debug)]
pub enum HotkeysError {
    Parse(HotKeyParseError),
    Register(global_hotkey::Error),
}

impl std::fmt::Display for HotkeysError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HotkeysError::Parse(e) => write!(f, "invalid hotkey binding: {e}"),
            HotkeysError::Register(e) => write!(f, "failed to register hotkey with the OS: {e}"),
        }
    }
}

impl std::error::Error for HotkeysError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HotkeysError::Parse(e) => Some(e),
            HotkeysError::Register(e) => Some(e),
        }
    }
}

/// Owns the live global-hotkey registration for GameFrame's bound actions.
///
/// OS-facing: [`Hotkeys::register`] creates a real `GlobalHotKeyManager`,
/// which per the `global-hotkey` crate's own docs requires a live Win32
/// event loop pumping on this thread (see [`crate::app::run_event_loop`]) --
/// a headless/CI sandbox has no such loop, so this is NOT exercised by this
/// crate's automated tests. The id -> action lookup it delegates to
/// ([`action_for`]) is exercised above in isolation from the OS call.
pub struct Hotkeys {
    // Kept alive for its `Drop` impl, which unregisters every hotkey and
    // tears down the manager's hidden window -- never read directly.
    _manager: GlobalHotKeyManager,
    bindings: Vec<ResolvedBinding>,
}

impl Hotkeys {
    /// Parses and registers every bound hotkey in `bindings` with the OS.
    /// With nothing bound, still creates the manager but registers nothing
    /// -- pressing keys triggers no action and cannot crash, since no
    /// `WM_HOTKEY` for any id will ever be delivered.
    pub fn register(bindings: &HotkeyBindings) -> Result<Self, HotkeysError> {
        let resolved = resolve_bindings(bindings).map_err(HotkeysError::Parse)?;

        let manager = GlobalHotKeyManager::new().map_err(HotkeysError::Register)?;
        for binding in &resolved {
            manager.register(binding.hotkey).map_err(HotkeysError::Register)?;
        }

        Ok(Self { _manager: manager, bindings: resolved })
    }

    /// The action a received [`global_hotkey::GlobalHotKeyEvent`]'s id maps
    /// to, or `None` if it doesn't match any of GameFrame's registered
    /// hotkeys.
    pub fn action_for(&self, id: u32) -> Option<HotkeyAction> {
        action_for(&self.bindings, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_bindings_with_no_hotkeys_bound_resolves_to_an_empty_list() {
        let bindings = HotkeyBindings { toggle_borderless: None, toggle_taskbar_autohide: None };

        let resolved = resolve_bindings(&bindings).unwrap();

        assert_eq!(resolved, vec![]);
    }

    #[test]
    fn resolve_bindings_parses_a_bound_toggle_borderless_hotkey() {
        let bindings = HotkeyBindings {
            toggle_borderless: Some("Ctrl+Alt+B".to_string()),
            toggle_taskbar_autohide: None,
        };

        let resolved = resolve_bindings(&bindings).unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].action, HotkeyAction::ToggleBorderless);
        assert_eq!(resolved[0].hotkey, "Ctrl+Alt+B".parse::<HotKey>().unwrap());
    }

    #[test]
    fn resolve_bindings_parses_a_bound_toggle_taskbar_autohide_hotkey() {
        let bindings = HotkeyBindings {
            toggle_borderless: None,
            toggle_taskbar_autohide: Some("Ctrl+Alt+T".to_string()),
        };

        let resolved = resolve_bindings(&bindings).unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].action, HotkeyAction::ToggleTaskbarAutohide);
        assert_eq!(resolved[0].hotkey, "Ctrl+Alt+T".parse::<HotKey>().unwrap());
    }

    #[test]
    fn resolve_bindings_resolves_both_bindings_independently_when_both_are_bound() {
        let bindings = HotkeyBindings {
            toggle_borderless: Some("Ctrl+Alt+B".to_string()),
            toggle_taskbar_autohide: Some("Ctrl+Alt+T".to_string()),
        };

        let resolved = resolve_bindings(&bindings).unwrap();

        assert_eq!(resolved.len(), 2);
        assert!(resolved
            .iter()
            .any(|b| b.action == HotkeyAction::ToggleBorderless
                && b.hotkey == "Ctrl+Alt+B".parse::<HotKey>().unwrap()));
        assert!(resolved
            .iter()
            .any(|b| b.action == HotkeyAction::ToggleTaskbarAutohide
                && b.hotkey == "Ctrl+Alt+T".parse::<HotKey>().unwrap()));
    }

    #[test]
    fn resolve_bindings_with_an_unparsable_hotkey_string_returns_an_error() {
        let bindings = HotkeyBindings {
            toggle_borderless: Some("not a real hotkey format!!".to_string()),
            toggle_taskbar_autohide: None,
        };

        let result = resolve_bindings(&bindings);

        assert!(result.is_err());
    }

    #[test]
    fn action_for_finds_the_action_bound_to_a_matching_hotkey_id() {
        let hotkey: HotKey = "Ctrl+Alt+B".parse().unwrap();
        let bindings = vec![ResolvedBinding { action: HotkeyAction::ToggleBorderless, hotkey }];

        assert_eq!(action_for(&bindings, hotkey.id()), Some(HotkeyAction::ToggleBorderless));
    }

    #[test]
    fn action_for_returns_none_when_no_binding_matches_the_id() {
        let hotkey: HotKey = "Ctrl+Alt+B".parse().unwrap();
        let bindings = vec![ResolvedBinding { action: HotkeyAction::ToggleBorderless, hotkey }];

        assert_eq!(action_for(&bindings, 999_999), None);
    }

    #[test]
    fn action_for_dispatches_to_the_correct_action_when_both_hotkeys_are_bound() {
        // Independence check: with both bound, pressing one hotkey's id must
        // resolve to its own action only, never the other's, and vice versa.
        let borderless_hotkey: HotKey = "Ctrl+Alt+B".parse().unwrap();
        let taskbar_hotkey: HotKey = "Ctrl+Alt+T".parse().unwrap();
        let bindings = vec![
            ResolvedBinding { action: HotkeyAction::ToggleBorderless, hotkey: borderless_hotkey },
            ResolvedBinding { action: HotkeyAction::ToggleTaskbarAutohide, hotkey: taskbar_hotkey },
        ];

        assert_eq!(action_for(&bindings, borderless_hotkey.id()), Some(HotkeyAction::ToggleBorderless));
        assert_eq!(action_for(&bindings, taskbar_hotkey.id()), Some(HotkeyAction::ToggleTaskbarAutohide));
    }

    #[test]
    fn action_for_on_an_empty_binding_list_returns_none_without_panicking() {
        // The fresh-install / nothing-bound case: pressing any key must not
        // crash and must trigger no action.
        assert_eq!(action_for(&[], 42), None);
    }

    #[test]
    fn hotkeys_error_implements_display_and_std_error() {
        fn assert_is_std_error<E: std::error::Error>(_: &E) {}

        let parse_err = match "not a real hotkey!!".parse::<HotKey>() {
            Err(e) => HotkeysError::Parse(e),
            Ok(_) => panic!("expected an invalid hotkey string to fail parsing"),
        };

        assert_is_std_error(&parse_err);
        assert!(parse_err.to_string().starts_with("invalid hotkey binding: "));
    }
}
