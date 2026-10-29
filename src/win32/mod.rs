//! Window manipulation for the borderless-toggle mechanic: stripping/restoring
//! a window's border/title bar, resizing/repositioning it, and disabling
//! Windows 11's rounded-corner rendering while it's borderless. Also holds
//! the OS primitives of the independent taskbar-hide mechanic: every call
//! site that hides the taskbar goes through a mode-aware entry point in
//! [`crate::taskbar`] (`hide_taskbar`/`set_taskbar_hide_absolute`/
//! `toggle_taskbar_hide`), which keeps the crash-recovery marker
//! ([`crate::taskbar_recovery`]) and calls the unmarked primitives here
//! (this module imports neither). It picks between two techniques per
//! [`crate::profile_store::TaskbarHideMode`] -- `FullyHidden`
//! (`ShowWindow(Shell_TrayWnd, SW_HIDE)`, no hover-reveal) or `HoverReveal`
//! (the native `SHAppBarMessage`/`ABS_AUTOHIDE` mechanism). Also owns the
//! "Apply to Window" picker's `EnumWindows` call
//! ([`list_visible_windows`]/`enum_windows_callback`), which calls into
//! [`crate::window_filter`] for the actual two-layer game-window
//! filtering/ranking decision logic.
//!
//! Split the same way as [`crate::settings_ui`]:
//! - Pure decision logic ([`strip_border_style`], [`restore_with_store`],
//!   [`Win32Error::is_access_denied`], [`Win32Error::from_hresult`],
//!   [`plan_taskbar_hide`], `apply_autohide_state`) has no
//!   `HWND`/OS dependency and is fully unit tested in the submodules. [`crate::window_filter`]
//!   follows the identical pure/OS-facing split for the window-filtering/ranking
//!   logic.
//! - The actual Win32 calls in the submodules (`GetForegroundWindow`, `IsIconic`,
//!   `SetWindowLongPtrW`, `SetWindowPos`, `DwmSetWindowAttribute`,
//!   `ShellExecuteW`, `FindWindowW`, `SHAppBarMessage`, `ShowWindow`,
//!   `EnumWindows`) are OS-facing -- they need a real foreground window, a
//!   real shell taskbar, or a real running process to query, which a
//!   headless/CI sandbox cannot provide -- so they are NOT exercised by this
//!   crate's automated tests.
//!
//! `crate::taskbar::toggle_taskbar_hide` keeps no remembered state at all -- it reads the
//! taskbar's real OS-level state fresh on every press (see its doc comment
//! for why) -- completely separate from the borderless mechanic's
//! [`RestoreState`]; the two toggles share no state.
//!
//! Submodules, all re-exported here so callers keep using `crate::win32::<item>`:
//! - `error`: [`Win32Error`].
//! - `style`: style bits, window bounds and the DWM corner preference.
//! - `placed`: [`PlacedWindows`], what is remembered per window handle.
//! - `drift`: the in-game-resize watcher's decisions.
//! - `hung`: hung-window detection and its logging.
//! - `borderless`: [`apply_borderless_to_window`] and [`restore_window`].
//! - `taskbar`: the taskbar-hide primitives.
//! - `process`: elevation, window enumeration and resolving a game's window.

mod borderless;
mod drift;
mod error;
mod hung;
mod placed;
mod process;
mod style;
mod taskbar;

pub use borderless::*;
pub use drift::*;
pub use error::*;
pub(crate) use hung::*;
pub use placed::*;
pub use process::*;
pub use style::*;
pub use taskbar::*;
