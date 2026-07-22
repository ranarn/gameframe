//! Owns the system tray icon and its three-item context menu ("Show
//! Settings", "Apply to Window", "Exit"), plus the id -> action mapping
//! that lets `app`'s event loop recognize a click on "Exit" and start
//! the clean-shutdown path.
//!
//! OS-facing construction ([`Tray::new`]) creates a real Win32 tray icon
//! and menu. Per `tray-icon`'s own docs, an event loop must already be
//! pumping on this thread for the icon/menu to actually work, and a
//! headless/CI sandbox has no interactive desktop session to render it
//! on -- so `Tray::new` is NOT exercised by this crate's automated
//! tests. The pure id -> action mapping ([`TrayMenuIds`]) that decides
//! what a click means -- including recognizing "Exit" -- is plain data
//! with no OS calls, so it's fully unit tested above in isolation from
//! that OS integration.

use tray_icon::menu::{Menu, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// The action represented by a tray menu click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    ShowSettings,
    ApplyToWindow,
    Exit,
}

const SHOW_SETTINGS_ID: &str = "gameframe-show-settings";
const APPLY_TO_WINDOW_ID: &str = "gameframe-apply-to-window";
const EXIT_ID: &str = "gameframe-exit";

/// The three tray menu items' ids, and the mapping back from an id to
/// the [`TrayAction`] it represents. Pure data -- no OS calls -- so
/// it's testable without a live tray icon.
#[derive(Debug, Clone)]
pub struct TrayMenuIds {
    show_settings: MenuId,
    apply_to_window: MenuId,
    exit: MenuId,
}

impl TrayMenuIds {
    pub fn new() -> Self {
        Self {
            show_settings: MenuId::new(SHOW_SETTINGS_ID),
            apply_to_window: MenuId::new(APPLY_TO_WINDOW_ID),
            exit: MenuId::new(EXIT_ID),
        }
    }

    /// Returns the action a menu-click event's id represents, or `None`
    /// if the id isn't one of this tray's own menu items.
    pub fn action_for(&self, id: &MenuId) -> Option<TrayAction> {
        if *id == self.show_settings {
            Some(TrayAction::ShowSettings)
        } else if *id == self.apply_to_window {
            Some(TrayAction::ApplyToWindow)
        } else if *id == self.exit {
            Some(TrayAction::Exit)
        } else {
            None
        }
    }
}

impl Default for TrayMenuIds {
    fn default() -> Self {
        Self::new()
    }
}

/// Reasons building the OS tray icon or its menu can fail.
#[derive(Debug)]
pub enum TrayUiError {
    /// Building the placeholder tray icon image failed.
    Icon(tray_icon::BadIcon),
    /// Appending an item to the tray's context menu failed.
    Menu(tray_icon::menu::Error),
    /// Creating the OS tray icon itself failed.
    Tray(tray_icon::Error),
}

impl std::fmt::Display for TrayUiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrayUiError::Icon(e) => write!(f, "failed to build the tray icon image: {e}"),
            TrayUiError::Menu(e) => write!(f, "failed to build the tray menu: {e}"),
            TrayUiError::Tray(e) => write!(f, "failed to create the OS tray icon: {e}"),
        }
    }
}

impl std::error::Error for TrayUiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TrayUiError::Icon(e) => Some(e),
            TrayUiError::Menu(e) => Some(e),
            TrayUiError::Tray(e) => Some(e),
        }
    }
}

/// The live system tray icon and its context menu.
///
/// Reference-counted internally by `tray_icon::TrayIcon`; the OS icon
/// (and its hidden message-only window) is removed when the last
/// instance is dropped -- so simply letting a `Tray` go out of scope
/// after "Exit" is selected is the entire clean-shutdown path for the
/// icon itself.
pub struct Tray {
    _icon: TrayIcon,
    ids: TrayMenuIds,
}

impl Tray {
    /// Builds and shows the real OS tray icon with its three-item menu.
    ///
    /// OS-facing: requires a live Win32 event loop pumping on this
    /// thread to actually render/function (see module docs) and is not
    /// exercised by this crate's automated tests.
    pub fn new() -> Result<Self, TrayUiError> {
        let ids = TrayMenuIds::new();

        let menu = Menu::new();
        menu.append(&MenuItem::with_id(
            ids.show_settings.clone(),
            "Show Settings",
            true, // opens the egui Settings window -- see `crate::settings_ui`
            None,
        ))
        .map_err(TrayUiError::Menu)?;
        menu.append(&MenuItem::with_id(
            ids.apply_to_window.clone(),
            "Apply to Window",
            false, // stubbed in this slice -- disabled until wired up
            None,
        ))
        .map_err(TrayUiError::Menu)?;
        menu.append(&MenuItem::with_id(ids.exit.clone(), "Exit", true, None))
            .map_err(TrayUiError::Menu)?;

        let icon = placeholder_icon().map_err(TrayUiError::Icon)?;

        let tray_icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(icon)
            .with_tooltip("GameFrame")
            .build()
            .map_err(TrayUiError::Tray)?;

        Ok(Self { _icon: tray_icon, ids })
    }

    /// The id -> action mapping for this tray's menu items.
    pub fn ids(&self) -> &TrayMenuIds {
        &self.ids
    }
}

/// A small solid-color placeholder icon. A real designed icon asset is
/// out of scope for this slice.
fn placeholder_icon() -> Result<Icon, tray_icon::BadIcon> {
    const SIZE: u32 = 16;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for _ in 0..(SIZE * SIZE) {
        rgba.extend_from_slice(&[0x2b, 0x6c, 0xb0, 0xff]);
    }
    Icon::from_rgba(rgba, SIZE, SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_for_maps_the_exit_id_to_the_exit_action() {
        let ids = TrayMenuIds::new();

        assert_eq!(ids.action_for(&MenuId::new(EXIT_ID)), Some(TrayAction::Exit));
    }

    #[test]
    fn action_for_maps_the_show_settings_id_to_the_show_settings_action() {
        let ids = TrayMenuIds::new();

        assert_eq!(
            ids.action_for(&MenuId::new(SHOW_SETTINGS_ID)),
            Some(TrayAction::ShowSettings)
        );
    }

    #[test]
    fn action_for_maps_the_apply_to_window_id_to_the_apply_to_window_action() {
        let ids = TrayMenuIds::new();

        assert_eq!(
            ids.action_for(&MenuId::new(APPLY_TO_WINDOW_ID)),
            Some(TrayAction::ApplyToWindow)
        );
    }

    #[test]
    fn action_for_returns_none_for_an_id_that_is_not_one_of_this_trays_menu_items() {
        let ids = TrayMenuIds::new();

        assert_eq!(ids.action_for(&MenuId::new("some-other-menu-item")), None);
    }
}
