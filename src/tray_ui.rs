//! Owns the system tray icon and its five top-level context menu items
//! ("Show Settings", "Apply to Window", "Start with Windows", "Check for
//! Updates Now", "Exit"), plus an "Update to vX.Y.Z" item while an update is
//! offered (ask mode, see [`Tray::show_update_item`]), and the id -> action
//! mapping that lets `app`'s event loop recognize a click on "Exit" and start
//! the clean-shutdown path.
//!
//! "Apply to Window" is itself a live [`Submenu`] of currently-visible
//! top-level windows (see [`Tray::refresh_apply_to_window_menu`] for the
//! live-refresh mechanism/rationale) rather than a single clickable item --
//! selecting one applies the Zone's borderless+reposition
//! treatment to that specific window (see `crate::app`'s dispatch of a
//! decoded window-item id, [`decode_window_item_id`]).
//!
//! OS-facing construction ([`Tray::new`]) creates a real Win32 tray icon
//! and menu. Per `tray-icon`'s own docs, an event loop must already be
//! pumping on this thread for the icon/menu to actually work, and a
//! headless/CI sandbox has no interactive desktop session to render it
//! on -- so `Tray::new` is NOT exercised by this crate's automated
//! tests. The pure id -> action mapping ([`TrayMenuIds`]) that decides
//! what a click means -- including recognizing "Exit" -- is plain data
//! with no OS calls, so it's fully unit tested above in isolation from
//! that OS integration. Likewise, the [`encode_window_item_id`]/
//! [`decode_window_item_id`] id-encoding scheme used to recover a target
//! `HWND` from a window-picker item's `MenuEvent` is pure string
//! encode/decode logic, fully unit tested in isolation from any real menu
//! or window.

use tray_icon::menu::{CheckMenuItem, Menu, MenuId, MenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use windows_sys::Win32::Foundation::HWND;

/// The action represented by a tray menu click.
///
/// Deliberately has no `ApplyToWindow` variant: "Apply to Window" is a
/// [`Submenu`], and a click on a submenu's own title never fires a
/// `MenuEvent` for its id in `muda`'s Windows-only `WM_COMMAND` handling (and
/// this crate is Windows-only per `Cargo.toml`) -- only clicks on its
/// dynamically-generated window-picker items do, which are recognized by
/// [`decode_window_item_id`] instead (see `crate::app`'s dispatch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    ShowSettings,
    ToggleStartWithWindows,
    CheckForUpdates,
    /// The dynamic "Update to vX.Y.Z" item ([`Tray::show_update_item`]).
    InstallUpdate,
    Exit,
}

/// The tray icon's hover text when no update is downloading.
pub const DEFAULT_TOOLTIP: &str = "GameFrame";

const SHOW_SETTINGS_ID: &str = "gameframe-show-settings";
const APPLY_TO_WINDOW_ID: &str = "gameframe-apply-to-window";
const START_WITH_WINDOWS_ID: &str = "gameframe-start-with-windows";
const CHECK_FOR_UPDATES_ID: &str = "gameframe-check-for-updates";
const INSTALL_UPDATE_ID: &str = "gameframe-install-update";
const EXIT_ID: &str = "gameframe-exit";

/// Prefix for the dynamically-generated "Apply to Window" submenu items'
/// ids, distinguishing them from the five fixed top-level menu items above.
///
/// Id-encoding scheme: a window-picker item's id is this prefix followed by
/// the target `HWND`'s numeric value (`hwnd as isize`, decimal), e.g.
/// `"gameframe-apply-to-window-item:12345"`. `MenuEvent`/`MenuId` only carry
/// a string, so this is how `app`'s dispatch loop recovers which window a
/// click on one of these dynamically-built items meant -- see
/// [`encode_window_item_id`]/[`decode_window_item_id`].
const APPLY_TO_WINDOW_ITEM_ID_PREFIX: &str = "gameframe-apply-to-window-item:";

/// Encodes a target window handle into a [`MenuId`] for one "Apply to
/// Window" submenu item. See [`APPLY_TO_WINDOW_ITEM_ID_PREFIX`] for the
/// format; round-trips with [`decode_window_item_id`].
fn encode_window_item_id(hwnd: HWND) -> MenuId {
    MenuId::new(format!("{APPLY_TO_WINDOW_ITEM_ID_PREFIX}{}", hwnd as isize))
}

/// Recovers the target `HWND` encoded by [`encode_window_item_id`] from a
/// `MenuEvent`'s id, or `None` if `id` isn't one of this shape (e.g. it's one
/// of the five fixed top-level menu items, or garbage) -- lets `app`'s
/// dispatch loop tell "this is a window-picker click" apart from the fixed
/// actions ([`TrayMenuIds::action_for`]) without either mechanism knowing
/// about the other's id format.
pub fn decode_window_item_id(id: &MenuId) -> Option<HWND> {
    id.as_ref()
        .strip_prefix(APPLY_TO_WINDOW_ITEM_ID_PREFIX)?
        .parse::<isize>()
        .ok()
        .map(|value| value as HWND)
}

/// The five tray menu items' ids, and the mapping back from an id to
/// the [`TrayAction`] it represents. Pure data -- no OS calls -- so
/// it's testable without a live tray icon.
#[derive(Debug, Clone)]
pub struct TrayMenuIds {
    show_settings: MenuId,
    apply_to_window: MenuId,
    start_with_windows: MenuId,
    check_for_updates: MenuId,
    install_update: MenuId,
    exit: MenuId,
}

impl TrayMenuIds {
    pub fn new() -> Self {
        Self {
            show_settings: MenuId::new(SHOW_SETTINGS_ID),
            apply_to_window: MenuId::new(APPLY_TO_WINDOW_ID),
            start_with_windows: MenuId::new(START_WITH_WINDOWS_ID),
            check_for_updates: MenuId::new(CHECK_FOR_UPDATES_ID),
            install_update: MenuId::new(INSTALL_UPDATE_ID),
            exit: MenuId::new(EXIT_ID),
        }
    }

    /// Returns the action a menu-click event's id represents, or `None`
    /// if the id isn't one of this tray's own dispatchable menu items.
    ///
    /// Deliberately does not match against `self.apply_to_window`: that id
    /// belongs to the "Apply to Window" [`Submenu`] itself, which -- per
    /// [`TrayAction`]'s doc comment -- never fires a `MenuEvent` for its own
    /// id; the field still exists to give the submenu its id at construction
    /// time (see [`Tray::new`]).
    pub fn action_for(&self, id: &MenuId) -> Option<TrayAction> {
        if *id == self.show_settings {
            Some(TrayAction::ShowSettings)
        } else if *id == self.start_with_windows {
            Some(TrayAction::ToggleStartWithWindows)
        } else if *id == self.check_for_updates {
            Some(TrayAction::CheckForUpdates)
        } else if *id == self.install_update {
            Some(TrayAction::InstallUpdate)
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
    /// Building the tray icon image failed.
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
    icon: TrayIcon,
    menu: Menu,
    /// The Update to vX.Y.Z item while an update is offered.
    update_item: std::cell::RefCell<Option<MenuItem>>,
    ids: TrayMenuIds,
    apply_to_window_menu: Submenu,
    start_with_windows_item: CheckMenuItem,
}

impl Tray {
    /// Builds and shows the real OS tray icon with its five top-level menu
    /// items ("Show Settings", "Apply to Window", "Start with Windows",
    /// "Check for Updates Now", "Exit"), where "Apply to Window" is itself a [`Submenu`] populated with
    /// one item per currently-visible top-level window (see
    /// [`rebuild_apply_to_window_items`]).
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

        let apply_to_window_menu =
            Submenu::with_id(ids.apply_to_window.clone(), "Apply to Window", true);
        rebuild_apply_to_window_items(&apply_to_window_menu);
        menu.append(&apply_to_window_menu)
            .map_err(TrayUiError::Menu)?;

        let start_with_windows_item = CheckMenuItem::with_id(
            ids.start_with_windows.clone(),
            "Start with Windows",
            true,
            crate::autostart::is_enabled(),
            None,
        );
        menu.append(&start_with_windows_item)
            .map_err(TrayUiError::Menu)?;

        menu.append(&MenuItem::with_id(
            ids.check_for_updates.clone(),
            "Check for Updates Now",
            true,
            None,
        ))
        .map_err(TrayUiError::Menu)?;

        menu.append(&MenuItem::with_id(ids.exit.clone(), "Exit", true, None))
            .map_err(TrayUiError::Menu)?;

        let icon = app_tray_icon().map_err(TrayUiError::Icon)?;

        let tray_icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            .with_icon(icon)
            .with_tooltip(DEFAULT_TOOLTIP)
            .build()
            .map_err(TrayUiError::Tray)?;

        Ok(Self {
            icon: tray_icon,
            menu,
            update_item: std::cell::RefCell::new(None),
            ids,
            apply_to_window_menu,
            start_with_windows_item,
        })
    }

    /// The id -> action mapping for this tray's five fixed top-level menu
    /// items.
    pub fn ids(&self) -> &TrayMenuIds {
        &self.ids
    }

    /// Rebuilds the "Apply to Window" submenu's item list from the
    /// currently-visible top-level windows ([`crate::win32::list_visible_windows`]),
    /// discarding whatever was listed before.
    ///
    /// Live-refresh mechanism: `muda`/`tray-icon` (as vendored -- see
    /// `Cargo.toml`) expose no "menu about to show"/`WM_INITMENUPOPUP`-style
    /// event to hook a just-in-time rebuild to (confirmed by inspecting the
    /// vendored crate sources; there's no such event in either crate's
    /// public API, nor any `WM_INITMENUPOPUP` handling in `muda`'s Windows
    /// backend). Instead, [`crate::app::run_event_loop`] calls this method
    /// when it observes a [`tray_icon::TrayIconEvent::Enter`] or
    /// [`tray_icon::TrayIconEvent::Click`] event on
    /// [`tray_icon::TrayIconEvent::receiver`] -- i.e. right before the user
    /// is actually about to see the menu, since (per `tray-icon`'s Windows
    /// backend) the mouse must hover onto the tray icon (`Enter`) before a
    /// click on it (`Click`) can open the context menu. It is deliberately not
    /// called before every pumped message: `tray-icon` sets a 15ms `WM_TIMER`
    /// while the mouse merely rests near the icon, which would drive a full
    /// `EnumWindows` + per-window `GetWindowTextW` + submenu rebuild (real
    /// Win32 syscalls) up to ~66 times/second just from the cursor resting
    /// nearby -- needlessly expensive on this process's one and only message
    /// loop. A secondary, less certain consideration also favors keeping the
    /// refresh off the hot path: per the documented
    /// `GetWindowText`/`GetWindowTextW` contract, a *same-process* target
    /// window causes a `WM_GETTEXT` to be sent via `SendMessage` (which can
    /// block if that window's own message handling is stuck), whereas a
    /// *cross-process* target -- the common case when enumerating the whole
    /// desktop -- is documented to read the window's cached caption text
    /// directly instead, specifically so callers don't hang on an
    /// unresponsive foreign process. So the SendMessage-hang risk this
    /// refresh could hit is narrower than "any hung window anywhere on the
    /// desktop" -- it's closer to "one of GameFrame's own top-level windows
    /// (e.g. an open Settings window) getting stuck", which is a real but
    /// much smaller surface. Gating on `Enter`/`Click` avoids that residual
    /// risk too, at effectively no cost.
    ///
    /// OS-facing: enumerates real windows and mutates a real `muda` submenu.
    /// Not exercised by this crate's automated tests -- see module docs.
    pub fn refresh_apply_to_window_menu(&self) {
        rebuild_apply_to_window_items(&self.apply_to_window_menu);
    }

    /// Adds (or relabels) the Update to v<version> item above Check for
    /// Updates Now. A failure is logged; the menu stays as it was.
    ///
    /// OS-facing; not exercised by the automated tests (see module docs).
    pub fn show_update_item(&self, version: &str) {
        let label = format!("Update to v{version}");
        let mut slot = self.update_item.borrow_mut();
        if let Some(item) = slot.as_ref() {
            item.set_text(label);
            return;
        }
        let item = MenuItem::with_id(self.ids.install_update.clone(), label, true, None);
        // Just above Check for Updates Now, wherever that sits.
        let position = self
            .menu
            .items()
            .iter()
            .position(|i| *i.id() == self.ids.check_for_updates)
            .unwrap_or(0);
        match self.menu.insert(&item, position) {
            Ok(()) => *slot = Some(item),
            Err(e) => log!("couldn't add the update item to the tray menu: {e}"),
        }
    }

    /// Removes the Update to ... item, if shown.
    pub fn hide_update_item(&self) {
        if let Some(item) = self.update_item.borrow_mut().take()
            && let Err(e) = self.menu.remove(&item)
        {
            log!("couldn't remove the update item from the tray menu: {e}");
        }
    }

    /// Sets the tray icon's hover text.
    pub fn set_tooltip(&self, text: &str) {
        if let Err(e) = self.icon.set_tooltip(Some(text)) {
            log!("couldn't set the tray tooltip: {e}");
        }
    }

    /// Re-reads the registry ([`crate::autostart::is_enabled`], the single
    /// source of truth) into the "Start with Windows" check mark. `muda`
    /// flips the check itself on every click, so this is called after a
    /// toggle (to undo that flip if the registry write failed) and before
    /// the menu shows (in case Settings, the installer or Task Manager
    /// changed it in the meantime).
    pub fn sync_start_with_windows_check(&self) {
        self.start_with_windows_item
            .set_checked(crate::autostart::is_enabled());
    }
}

/// Clears `submenu`'s current items and re-populates it with one
/// [`MenuItem`] per currently-visible top-level window
/// ([`crate::win32::list_visible_windows`]), labeled with the window's title
/// and carrying an id encoding its `HWND` ([`encode_window_item_id`]) for
/// [`crate::app`]'s dispatch to recover on click.
fn rebuild_apply_to_window_items(submenu: &Submenu) {
    while submenu.remove_at(0).is_some() {}

    for (hwnd, title) in crate::win32::list_visible_windows() {
        let item = MenuItem::with_id(
            encode_window_item_id(hwnd),
            escape_menu_ampersand(&title),
            true,
            None,
        );
        // A single item failing to append (e.g. a transient allocation
        // failure) shouldn't abort populating the rest of the list.
        let _ = submenu.append(&item);
    }
}

/// Escapes a window title for safe display as a literal Win32 menu-item
/// label: Win32 menus treat a single `&` as an accelerator-mnemonic marker
/// (swallowed, with the next character underlined), so a real window title
/// containing one (e.g. "AT&T", "Tom & Jerry - YouTube") would otherwise
/// render mangled in the "Apply to Window" picker. `&&` is the documented
/// Win32 convention for a literal ampersand in a menu string. Pure string
/// logic -- no OS/menu dependency -- so it's unit testable in isolation.
fn escape_menu_ampersand(title: &str) -> String {
    title.replace('&', "&&")
}

/// GameFrame's tray icon: the corner-bracket mark from [`gameframe::icon`],
/// rendered at 32x32 so it stays crisp on high-DPI notification areas that
/// display tray icons larger than the classic 16x16.
fn app_tray_icon() -> Result<Icon, tray_icon::BadIcon> {
    const SIZE: u32 = 32;
    let rgba = gameframe::icon::mark_rgba(SIZE);
    Icon::from_rgba(rgba, SIZE, SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_for_maps_the_exit_id_to_the_exit_action() {
        let ids = TrayMenuIds::new();

        assert_eq!(
            ids.action_for(&MenuId::new(EXIT_ID)),
            Some(TrayAction::Exit)
        );
    }

    #[test]
    fn action_for_maps_the_start_with_windows_id_to_the_toggle_action() {
        let ids = TrayMenuIds::new();

        assert_eq!(
            ids.action_for(&MenuId::new(START_WITH_WINDOWS_ID)),
            Some(TrayAction::ToggleStartWithWindows)
        );
    }

    #[test]
    fn action_for_maps_the_check_for_updates_id_to_the_check_for_updates_action() {
        let ids = TrayMenuIds::new();

        assert_eq!(
            ids.action_for(&MenuId::new(CHECK_FOR_UPDATES_ID)),
            Some(TrayAction::CheckForUpdates)
        );
    }

    #[test]
    fn action_for_maps_the_install_update_id_to_the_install_update_action() {
        let ids = TrayMenuIds::new();

        assert_eq!(
            ids.action_for(&MenuId::new(INSTALL_UPDATE_ID)),
            Some(TrayAction::InstallUpdate)
        );
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
    fn action_for_returns_none_for_the_apply_to_window_submenus_own_id() {
        // The submenu's own id never fires a `MenuEvent` for itself (see
        // `TrayAction`'s doc comment) -- only its dynamically-generated
        // window-picker items do, via `decode_window_item_id`.
        let ids = TrayMenuIds::new();

        assert_eq!(ids.action_for(&MenuId::new(APPLY_TO_WINDOW_ID)), None);
    }

    #[test]
    fn action_for_returns_none_for_an_id_that_is_not_one_of_this_trays_menu_items() {
        let ids = TrayMenuIds::new();

        assert_eq!(ids.action_for(&MenuId::new("some-other-menu-item")), None);
    }

    #[test]
    fn decode_window_item_id_recovers_the_hwnd_encoded_by_encode_window_item_id() {
        let hwnd = 0x1234_5678 as HWND;

        let id = encode_window_item_id(hwnd);

        assert_eq!(decode_window_item_id(&id), Some(hwnd));
    }

    #[test]
    fn decode_window_item_id_rejects_an_id_missing_the_window_item_prefix() {
        assert_eq!(
            decode_window_item_id(&MenuId::new("not-a-window-item")),
            None
        );
    }

    #[test]
    fn decode_window_item_id_rejects_a_prefixed_id_with_a_non_numeric_suffix() {
        assert_eq!(
            decode_window_item_id(&MenuId::new(format!(
                "{APPLY_TO_WINDOW_ITEM_ID_PREFIX}not-a-number"
            ))),
            None
        );
    }

    #[test]
    fn decode_window_item_id_rejects_one_of_the_fixed_top_level_menu_ids() {
        assert_eq!(
            decode_window_item_id(&MenuId::new(APPLY_TO_WINDOW_ID)),
            None
        );
        assert_eq!(decode_window_item_id(&MenuId::new(EXIT_ID)), None);
        assert_eq!(decode_window_item_id(&MenuId::new(SHOW_SETTINGS_ID)), None);
    }

    #[test]
    fn escape_menu_ampersand_doubles_a_single_ampersand() {
        assert_eq!(escape_menu_ampersand("AT&T"), "AT&&T");
    }

    #[test]
    fn escape_menu_ampersand_leaves_a_title_without_ampersands_unchanged() {
        assert_eq!(escape_menu_ampersand("Notepad"), "Notepad");
    }

    #[test]
    fn escape_menu_ampersand_doubles_every_ampersand_in_a_title_with_multiple() {
        assert_eq!(
            escape_menu_ampersand("Tom & Jerry & Friends"),
            "Tom && Jerry && Friends"
        );
    }
}
