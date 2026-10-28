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

/// The `uID` `tray-icon` gives the shell for its icon: the crate numbers its
/// icons from 1 with a process-wide counter and GameFrame creates exactly one
/// (`tray-icon` 0.26 has no accessor for it). A wrong value only makes the
/// balloon fail, which [`crate::notice::notify`] answers with a message box.
///
/// Verified against `tray-icon` 0.26.1: `counter.rs` starts its process-wide
/// `COUNTER` at 1, `next()` is a `fetch_add`, and the result is the `uID` that
/// `register_tray_icon` passes to `Shell_NotifyIconW`. Holds only while
/// `Tray::new` runs once per process; recheck on a `tray-icon` bump.
const TRAY_ICON_UID: u32 = 1;

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
    /// The "Update to vX.Y.Z" item while an update is offered.
    update_item: std::cell::RefCell<Option<MenuItem>>,
    ids: TrayMenuIds,
    apply_to_window_menu: Submenu,
    /// Builds the "Apply to Window" list on a worker thread.
    picker: crate::window_picker::WindowPicker,
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
        // Starts empty: the window list is built on a worker thread
        // (`crate::window_picker`), never on this one.
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
            .with_tooltip(crate::version::current_tooltip())
            .build()
            .map_err(TrayUiError::Tray)?;

        Ok(Self {
            icon: tray_icon,
            menu,
            update_item: std::cell::RefCell::new(None),
            ids,
            apply_to_window_menu,
            picker: crate::window_picker::WindowPicker::new(),
            start_with_windows_item,
        })
    }

    /// The id -> action mapping for this tray's five fixed top-level menu
    /// items.
    pub fn ids(&self) -> &TrayMenuIds {
        &self.ids
    }

    /// Asks for a fresh "Apply to Window" window list. The list is built on a
    /// worker thread ([`crate::window_picker`]); the submenu keeps showing the
    /// previous list until [`Self::apply_finished_window_refresh`] swaps the
    /// new one in. Never blocks the UI thread.
    ///
    /// `muda`/`tray-icon` (as vendored) expose no "menu about to show"
    /// event, so [`crate::app::run_event_loop`] calls this on a
    /// [`tray_icon::TrayIconEvent::Enter`] or
    /// [`tray_icon::TrayIconEvent::Click`] -- the hover that precedes a click.
    /// It is not called before every pumped message: `tray-icon` sets a 15ms
    /// `WM_TIMER` while the cursor rests near the icon. At most one refresh
    /// runs at a time.
    ///
    /// Known limit: the tray popup's modal menu loop does not run the event
    /// loop, so a menu that is already open keeps the previous list; the
    /// hover normally started the refresh early enough for the next open.
    ///
    /// OS-facing; not exercised by this crate's automated tests -- see module
    /// docs.
    pub fn refresh_apply_to_window_menu(&self, config_dir: &std::path::Path) {
        self.picker.request(config_dir.to_path_buf());
    }

    /// Rebuilds the "Apply to Window" submenu if a background refresh
    /// ([`Self::refresh_apply_to_window_menu`]) has finished since the last
    /// call. Cheap; the event loop calls it on every pass.
    pub fn apply_finished_window_refresh(&self) {
        if let Some(entries) = self.picker.take_latest() {
            rebuild_apply_to_window_items(&self.apply_to_window_menu, &entries);
        }
    }

    /// Adds (or relabels) the "Update to v<version>" item above "Check for
    /// Updates Now". A failure is logged; the menu stays as it was.
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
        // Just above "Check for Updates Now", wherever that sits.
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

    /// Removes the "Update to ..." item, if shown.
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

    /// Tells the user `text` right now: a balloon on this icon (see
    /// [`crate::notice`]), a message box if the OS refuses it. Never blocks.
    pub fn notify(&self, text: &str) {
        crate::notice::notify(self.icon.window_handle(), TRAY_ICON_UID, text);
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
/// [`MenuItem`] per picker entry (built off-thread by
/// [`crate::window_picker`]), labeled with the window's title
/// and carrying an id encoding its `HWND` ([`encode_window_item_id`]) for
/// [`crate::app`]'s dispatch to recover on click.
fn rebuild_apply_to_window_items(submenu: &Submenu, entries: &[crate::window_picker::PickerEntry]) {
    while submenu.remove_at(0).is_some() {}

    for (hwnd, title) in entries {
        let item = MenuItem::with_id(
            encode_window_item_id(*hwnd as HWND),
            escape_menu_ampersand(title),
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
