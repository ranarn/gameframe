//! The tray menu's "Apply to Window" picker's two-layer game-window
//! filtering/ranking (see `docs/research/game-window-filtering.md` for the
//! full rationale):
//!
//! - Layer 1 ([`is_listable_window`]) hard-filters cloaked/owned/tool windows
//!   out of the list entirely.
//! - Layer 2 ([`rank_window`]) only ever reorders the remaining list by how
//!   confidently a window looks like a game (launcher install match, then
//!   loaded graphics module, then engine window class) -- it never hides a
//!   window, so an unrecognized one stays fully pickable.
//!
//! Split the same way as [`crate::win32`]:
//! - Pure decision logic ([`is_listable_window`], [`rank_window`],
//!   [`is_known_engine_window_class`], [`has_known_graphics_module`],
//!   `matches_known_install`) has no `HWND`/OS dependency and is fully unit
//!   tested below.
//! - The OS-facing glue further down (`DwmGetWindowAttribute`,
//!   `GetAncestor`/`GetLastActivePopup`, `GetWindowLongPtrW`,
//!   `CreateToolhelp32Snapshot`, `OpenProcess`/`QueryFullProcessImageNameW`,
//!   the `steamlocate` scan) needs a real window/process to query, which a
//!   headless/CI sandbox cannot provide -- so it is NOT exercised by this
//!   crate's automated tests.
//!
//! `crate::win32::list_visible_windows`/`enum_windows_callback` own the real
//! `EnumWindows` call and call into this module's functions per candidate
//! window.

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32,
};
use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GA_ROOTOWNER, GetAncestor, GetClassNameW, GetLastActivePopup, GetWindowLongPtrW, GetWindowThreadProcessId,
    GWL_EXSTYLE,
};

/// Decides whether a window is worth offering in the tray menu's "Apply to
/// Window" picker -- Layer 1 of the game-window filtering documented in
/// `docs/research/game-window-filtering.md`: hard filters that unconditionally
/// exclude a window, as opposed to Layer 2's ranking-only signals (see
/// [`rank_window`]) which never hide a window.
///
/// A window is listable if it has a non-blank title (an invisible or
/// untitled window isn't something a user could sensibly pick by name) and
/// is otherwise [`is_canonical_window`] -- see that function for the shared
/// Layer 1 boolean formula (visible, not cloaked, root-owner/last-active-
/// popup collapsing, tool-window exclusion, app-window override).
///
/// Pure logic -- no `HWND`/OS dependency -- so it's unit testable separately
/// from the real `EnumWindows` callback (`crate::win32::list_visible_windows`)
/// that calls it; the OS-facing signals themselves are gathered by
/// [`is_window_cloaked`], [`is_root_owner_or_last_active_popup`], and
/// [`window_ex_style`].
pub(crate) fn is_listable_window(
    title: &str,
    is_visible: bool,
    is_cloaked: bool,
    is_root_owner_or_last_active_popup: bool,
    has_tool_window_style: bool,
    has_app_window_style: bool,
) -> bool {
    !title.trim().is_empty()
        && is_canonical_window(
            is_visible,
            is_cloaked,
            is_root_owner_or_last_active_popup,
            has_tool_window_style,
            has_app_window_style,
        )
}

/// The Layer 1 canonical-window boolean formula shared by [`is_listable_window`]
/// (the tray menu's "Apply to Window" picker, which additionally requires a
/// non-blank title on top -- see that function) and
/// `crate::win32::enum_windows_for_pid_callback` (issue #33's PID-scoped
/// window resolution, which doesn't need a human-readable title -- it's
/// machine-driven, matching an already-known process rather than being
/// picked by name).
///
/// A window is canonical if:
/// - it's currently visible;
/// - it isn't cloaked (`DWMWA_CLOAKED` -- DWM-hidden windows, e.g. a
///   suspended UWP/Store-app window on another virtual desktop, that still
///   report `IsWindowVisible() == TRUE` with a stale title; see the Old New
///   Thing post cited in the research doc);
/// - it is its own root owner's current last-active-visible-popup (per the
///   documented Alt-Tab algorithm: `GetAncestor(hwnd, GA_ROOTOWNER)` +
///   `GetLastActivePopup`), which collapses owner/owned window chains (a
///   game's real window plus invisible helper windows) down to one entry;
/// - it doesn't carry `WS_EX_TOOLWINDOW` (floating tool palettes/utility
///   windows, excluded from the taskbar/Alt-Tab by convention);
///
/// -- *unless* `WS_EX_APPWINDOW` is set, which forces inclusion regardless of
/// the owner-chain/tool-window rules above, per the documented Extended
/// Window Styles semantics.
///
/// Pure logic -- no `HWND`/OS dependency -- extracted so both call sites'
/// Layer 1 rules stay a single source of truth instead of two independently
/// maintained copies (issue #33's iteration-3 review fix: the PID-scoped
/// callback had re-implemented this formula inline, minus the title check,
/// with no test coverage of its own for the duplicate).
pub(crate) fn is_canonical_window(
    is_visible: bool,
    is_cloaked: bool,
    is_root_owner_or_last_active_popup: bool,
    has_tool_window_style: bool,
    has_app_window_style: bool,
) -> bool {
    is_visible
        && !is_cloaked
        && (has_app_window_style || (is_root_owner_or_last_active_popup && !has_tool_window_style))
}

/// Layer 2 of the game-window filtering documented in
/// `docs/research/game-window-filtering.md`: how confidently a window looks
/// like a game, from weakest to strongest signal (declaration order doubles
/// as `Ord`/`PartialOrd` ranking, lowest variant first) -- used only to sort
/// the "Apply to Window" picker's list and/or badge an entry; unlike
/// [`is_listable_window`]'s Layer-1 hard filters, a low (or [`Unknown`]) rank
/// never removes a window from the list. An unrecognized window (no launcher
/// match, no graphics DLL loaded, no known engine window class) always
/// remains pickable at [`WindowRank::Unknown`].
///
/// [`Unknown`]: WindowRank::Unknown
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WindowRank {
    /// No Layer-2 signal matched -- still fully pickable, just unranked.
    Unknown,
    /// The window's class name matches a small, explicitly-known list of
    /// game-engine window classes (weakest tie-breaker; see
    /// [`is_known_engine_window_class`]).
    EngineWindowClass,
    /// The window's owning process already has a known graphics-API module
    /// (Direct3D/DXGI/OpenGL/Vulkan) loaded (medium confidence; see
    /// [`has_known_graphics_module`]).
    GraphicsModuleLoaded,
    /// The window's owning process's executable path matches a known
    /// launcher install location (strongest signal; see
    /// [`matches_known_install`]).
    LauncherMatch,
}

/// Combines the three Layer-2 signals (documented priority order: launcher
/// install match, then loaded-graphics-module, then engine window class) into
/// a single [`WindowRank`] for sorting/labeling the "Apply to Window" picker.
/// Never used to exclude a window -- see [`WindowRank`]'s docs.
///
/// Pure logic -- no OS dependency -- so it's unit testable in isolation from
/// the OS-facing signal gathering ([`matches_known_install`] callers,
/// [`has_known_graphics_module`] callers, [`is_known_engine_window_class`]
/// callers).
pub(crate) fn rank_window(is_launcher_match: bool, has_graphics_module: bool, has_known_engine_window_class: bool) -> WindowRank {
    if is_launcher_match {
        WindowRank::LauncherMatch
    } else if has_graphics_module {
        WindowRank::GraphicsModuleLoaded
    } else if has_known_engine_window_class {
        WindowRank::EngineWindowClass
    } else {
        WindowRank::Unknown
    }
}

/// The Layer-2 window-class heuristic list from
/// `docs/research/game-window-filtering.md`: a small, explicitly-known set of
/// game-engine window classes, used only as the weakest tie-breaker signal in
/// [`rank_window`] -- never as an exclusion.
const KNOWN_ENGINE_WINDOW_CLASSES: &[&str] = &["UnrealWindow", "Direct3DWindowClass", "YYGameMakerYY"];

/// Prefixes for engine window classes that vary per-app (SDL/GLFW embed the
/// hosting app's own name after a fixed prefix), checked in addition to
/// [`KNOWN_ENGINE_WINDOW_CLASSES`]'s exact-match list.
const KNOWN_ENGINE_WINDOW_CLASS_PREFIXES: &[&str] = &["SDL_app", "GLFW"];

/// Whether `class_name` (a window's `GetClassNameW` result) matches one of
/// the small, explicitly-known game-engine window classes documented in
/// `docs/research/game-window-filtering.md`. Pure string matching -- no OS
/// dependency -- so it's unit testable in isolation from the real
/// `GetClassNameW` call that supplies `class_name`.
fn is_known_engine_window_class(class_name: &str) -> bool {
    KNOWN_ENGINE_WINDOW_CLASSES.contains(&class_name)
        || KNOWN_ENGINE_WINDOW_CLASS_PREFIXES.iter().any(|prefix| class_name.starts_with(prefix))
}

/// The Layer-2 loaded-graphics-module list from
/// `docs/research/game-window-filtering.md`: DLLs whose presence in a
/// process's already-loaded module list is a medium-confidence "this process
/// talks to a graphics API" signal for [`rank_window`] -- never an exclusion.
const KNOWN_GRAPHICS_MODULES: &[&str] = &["d3d9.dll", "d3d11.dll", "dxgi.dll", "d3d12.dll", "opengl32.dll", "vulkan-1.dll"];

/// Whether any of `loaded_module_names` (a process's already-loaded module
/// list, as gathered via `CreateToolhelp32Snapshot(TH32CS_SNAPMODULE |
/// TH32CS_SNAPMODULE32, ...)`) matches one of the known graphics-API DLLs.
/// Case-insensitive, since Windows module names aren't case-sensitive. Pure
/// string matching -- no OS dependency -- so it's unit testable in isolation
/// from the real toolhelp snapshot that supplies `loaded_module_names`.
fn has_known_graphics_module(loaded_module_names: &[String]) -> bool {
    loaded_module_names
        .iter()
        .any(|loaded| KNOWN_GRAPHICS_MODULES.iter().any(|known| loaded.eq_ignore_ascii_case(known)))
}

/// Whether `exe_path` (a window's owning process's executable path, as
/// resolved via `QueryFullProcessImageNameW`) sits inside one of
/// `known_install_dirs` -- a cached snapshot of known launcher (Steam, via
/// the `steamlocate` crate) install directories. The strongest of the three
/// Layer-2 signals per `docs/research/game-window-filtering.md`: a match
/// essentially never false-positives, since it doesn't depend on window
/// styles or graphics APIs at all.
///
/// Pure path matching -- no OS/filesystem dependency -- so it's unit testable
/// in isolation from the real `steamlocate`/`QueryFullProcessImageNameW`
/// calls that supply `exe_path`/`known_install_dirs`.
fn matches_known_install(exe_path: &std::path::Path, known_install_dirs: &[std::path::PathBuf]) -> bool {
    known_install_dirs.iter().any(|install_dir| exe_path.starts_with(install_dir))
}

/// The two Layer-2 signals that are genuinely process-level -- an executable
/// path and a process's loaded-module list don't vary per top-level window --
/// as opposed to the window-class signal, which is a per-*window* property.
/// [`rank_window_for_hwnd`] caches only this pair per PID; see its docs for
/// why the window-class signal must never be memoized alongside them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProcessRankSignals {
    is_launcher_match: bool,
    has_graphics_module: bool,
}

/// Gathers `hwnd`'s Layer-2 signals and combines them via [`rank_window`].
/// `cache` memoizes only the two genuinely process-level signals
/// ([`ProcessRankSignals`]: launcher-install match, loaded graphics module)
/// per PID for the duration of a single `crate::win32::list_visible_windows()`
/// call, so a process owning multiple top-level listable windows only pays
/// for one `OpenProcess`/`CreateToolhelp32Snapshot`/module-walk instead of one
/// per window.
///
/// The window-class signal is deliberately *not* included in that cache: it's
/// a per-window property (`GetClassNameW` on `hwnd` itself), not a per-process
/// one, so it's always freshly recomputed for every call -- otherwise a
/// process whose first-enumerated window has a generic class would silently
/// poison the rank of a later window from the same PID that has a
/// recognized engine window class (e.g. `UnrealWindow`). Recomputing it is
/// cheap (pure string matching over a single `GetClassNameW` call), so this
/// costs nothing worth caching.
///
/// OS-facing glue -- not exercised by this crate's automated tests -- see
/// module docs; [`rank_window`] itself (the decision logic) is unit tested in
/// isolation.
pub(crate) fn rank_window_for_hwnd(hwnd: HWND, cache: &mut std::collections::HashMap<u32, ProcessRankSignals>) -> WindowRank {
    let mut pid: u32 = 0;
    // Safety: `hwnd` is a live handle; `&mut pid` is a valid out-pointer for
    // `GetWindowThreadProcessId`'s documented contract.
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }

    let process_signals = *cache.entry(pid).or_insert_with(|| {
        let is_launcher_match = process_exe_path(pid)
            .map(|exe_path| matches_known_install(&exe_path, &known_steam_install_dirs()))
            .unwrap_or(false);
        let has_graphics_module = has_known_graphics_module(&loaded_module_names(pid));
        ProcessRankSignals { is_launcher_match, has_graphics_module }
    });

    let has_known_engine_class = is_known_engine_window_class(&window_class_name(hwnd));

    rank_window(process_signals.is_launcher_match, process_signals.has_graphics_module, has_known_engine_class)
}

/// Reads a window's class name via `GetClassNameW`, returning an empty
/// string (matched by no [`is_known_engine_window_class`] entry) on failure
/// rather than propagating an error -- an unreadable class name just means no
/// window-class ranking signal, not a hard failure.
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs.
fn window_class_name(hwnd: HWND) -> String {
    let mut buffer = vec![0u16; 256];
    // Safety: `hwnd` is a live handle; `buffer` is a valid, correctly-sized
    // out-buffer for `GetClassNameW` to write into, per its documented
    // contract.
    let len = unsafe { GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..len as usize])
}

/// Resolves a process's executable path via `OpenProcess` (with the minimal
/// `PROCESS_QUERY_LIMITED_INFORMATION` access right, which doesn't require
/// elevation for most processes) + `QueryFullProcessImageNameW`. Returns
/// `None` on any failure (e.g. a protected/elevated process GameFrame can't
/// query without its own elevation) -- treated as "no launcher-match signal
/// available," never as "not a game."
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs; the decision that consumes this value ([`matches_known_install`]) is
/// unit tested in isolation.
///
/// `pub(crate)` (rather than private) since issue #33's PID<->HWND
/// resolution (`crate::win32::find_pid_by_exe_path`) also calls this
/// directly, to confirm a toolhelp snapshot's cheap file-name pre-filter
/// match with a full-path comparison before accepting a PID.
pub(crate) fn process_exe_path(pid: u32) -> Option<std::path::PathBuf> {
    // Safety: `pid` is a plain integer; `OpenProcess` takes no pointers.
    let handle: HANDLE = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }

    let mut buffer = vec![0u16; 1024];
    let mut size = buffer.len() as u32;
    // Safety: `handle` is a live handle just opened above; `buffer` is a
    // valid out-buffer whose length is passed via `size`, per
    // `QueryFullProcessImageNameW`'s documented contract.
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size) };
    // Safety: `handle` is a valid handle owned solely by this function; no
    // other reference to it survives past this point.
    unsafe {
        CloseHandle(handle);
    }

    if ok == 0 {
        return None;
    }
    Some(std::path::PathBuf::from(String::from_utf16_lossy(&buffer[..size as usize])))
}

/// Resolves the executable path of the process that owns `hwnd`, via
/// `GetWindowThreadProcessId` + [`process_exe_path`]. Shared plumbing for
/// both the borderless-hotkey path's foreground-window resolution
/// ([`crate::win32::foreground_window_exe_path`], which is just
/// `exe_path_for_window(GetForegroundWindow())`) and the tray's "Apply to
/// Window" path's explicit-`hwnd` resolution -- so the PID/exe-path
/// resolution glue lives in one place rather than being duplicated at both
/// call sites.
///
/// Returns `None` if `GetWindowThreadProcessId` can't resolve a PID for
/// `hwnd`, or if [`process_exe_path`] itself fails (see its own doc comment)
/// -- either way, treated as "no exe-path signal available", never as a hard
/// error.
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs.
pub(crate) fn exe_path_for_window(hwnd: HWND) -> Option<std::path::PathBuf> {
    let mut pid: u32 = 0;
    // Safety: `hwnd` is a live handle; `&mut pid` is a valid out-pointer for
    // `GetWindowThreadProcessId`'s documented contract.
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    if pid == 0 {
        return None;
    }
    process_exe_path(pid)
}

/// Enumerates a process's currently-loaded module names via
/// `CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid)` +
/// `Module32FirstW`/`Module32NextW`. `TH32CS_SNAPMODULE32` is OR'd in
/// alongside `TH32CS_SNAPMODULE` because, per Microsoft's documented
/// behavior, a 64-bit caller snapshotting a 32-bit (WOW64) target process's
/// modules with `TH32CS_SNAPMODULE` alone can fail or return incomplete
/// results -- and many older/indie games (exactly the class of game this
/// Layer-2 signal exists to help rank, since they often lack a launcher-
/// install match) still ship 32-bit executables. Returns an empty list on any
/// failure (documented as possible if the target's loader state is
/// transiently inconsistent -- see `docs/research/game-window-filtering.md`)
/// -- treated as "unknown," not "not a game," since this only ever feeds a
/// ranking signal, never an exclusion.
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs; the decision that consumes this value
/// ([`has_known_graphics_module`]) is unit tested in isolation.
fn loaded_module_names(pid: u32) -> Vec<String> {
    // Safety: `pid` is a plain integer; `CreateToolhelp32Snapshot` takes no
    // pointers.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }

    let mut names = Vec::new();
    let mut entry: MODULEENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;

    // Safety: `snapshot` is the live handle just created above; `entry` is a
    // valid, correctly-sized out-pointer with `dwSize` set, per
    // `Module32FirstW`/`Module32NextW`'s documented contract.
    let mut has_entry = unsafe { Module32FirstW(snapshot, &mut entry) } != 0;
    while has_entry {
        let len = entry.szModule.iter().position(|&c| c == 0).unwrap_or(entry.szModule.len());
        names.push(String::from_utf16_lossy(&entry.szModule[..len]));
        // Safety: same as above.
        has_entry = unsafe { Module32NextW(snapshot, &mut entry) } != 0;
    }

    // Safety: `snapshot` is a valid handle owned solely by this function; no
    // other reference to it survives past this point.
    unsafe {
        CloseHandle(snapshot);
    }
    names
}

/// When a Steam-install-directory scan ran, and what it found.
type SteamInstallDirsSnapshot = (std::time::Instant, Vec<std::path::PathBuf>);

/// The cached snapshot of known Steam install directories, refreshed lazily
/// (see [`known_steam_install_dirs`]) rather than on every
/// `crate::win32::list_visible_windows` call.
fn steam_install_dirs_cache() -> &'static std::sync::Mutex<Option<SteamInstallDirsSnapshot>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<SteamInstallDirsSnapshot>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

/// How long a cached Steam-install-directory scan is trusted before
/// [`known_steam_install_dirs`] rescans -- installed games don't change
/// per-frame, so this is refreshed lazily (e.g. roughly once per tray-menu
/// open at typical interaction cadence) rather than on every
/// `crate::win32::list_visible_windows` call, per
/// `docs/research/game-window-filtering.md`.
const STEAM_INSTALL_DIRS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// Returns the cached snapshot of known Steam install directories (via the
/// `steamlocate` crate), scanning again only if nothing is cached yet or the
/// cached scan is older than [`STEAM_INSTALL_DIRS_CACHE_TTL`].
///
/// OS-facing (filesystem/registry scan via `steamlocate`): not exercised by
/// this crate's automated tests -- see module docs; the decision that
/// consumes this value ([`matches_known_install`]) is unit tested in
/// isolation.
fn known_steam_install_dirs() -> Vec<std::path::PathBuf> {
    let cache = steam_install_dirs_cache();
    let mut guard = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((cached_at, dirs)) = guard.as_ref()
        && cached_at.elapsed() < STEAM_INSTALL_DIRS_CACHE_TTL {
            return dirs.clone();
        }

    let dirs = scan_steam_install_dirs();
    *guard = Some((std::time::Instant::now(), dirs.clone()));
    dirs
}

/// Performs the actual `steamlocate` scan: locates the Steam installation,
/// walks every library, and resolves every installed app's install
/// directory. Returns an empty list if Steam isn't installed or any step
/// fails -- a missing/unreadable Steam install just means no launcher-match
/// signal is available, not a hard error.
fn scan_steam_install_dirs() -> Vec<std::path::PathBuf> {
    let Ok(steam_dir) = steamlocate::SteamDir::locate() else {
        return Vec::new();
    };
    let Ok(libraries) = steam_dir.libraries() else {
        return Vec::new();
    };

    libraries
        .filter_map(Result::ok)
        .flat_map(|library| {
            let install_dirs: Vec<std::path::PathBuf> = library
                .apps()
                .filter_map(Result::ok)
                .map(|app| library.resolve_app_dir(&app))
                .collect();
            install_dirs
        })
        .collect()
}

/// Reads a window's current `GWL_EXSTYLE` value (the extended style bits,
/// e.g. `WS_EX_TOOLWINDOW`/`WS_EX_APPWINDOW`) -- distinct from `GWL_STYLE`.
/// Treats a zero return as a genuine all-bits-clear ex-style rather than an
/// error, since none of this module's Layer-1/Layer-2 callers need to
/// distinguish that from a failed call (worst case, a window with an
/// unreadable ex-style is treated as having neither `WS_EX_TOOLWINDOW` nor
/// `WS_EX_APPWINDOW` set).
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs.
pub(crate) fn window_ex_style(hwnd: HWND) -> u32 {
    // Safety: `hwnd` is a live handle; `GetWindowLongPtrW` performs no writes
    // and takes no pointers besides `hwnd` itself.
    (unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) }) as u32
}

/// Implements the documented Alt-Tab "which windows appear in the list?"
/// algorithm (see `docs/research/game-window-filtering.md`): walks to
/// `hwnd`'s root owner via `GetAncestor(hwnd, GA_ROOTOWNER)`, then finds that
/// owner's current last-active-visible-popup via `GetLastActivePopup` --
/// `hwnd` qualifies only if it *is* that computed representative window. This
/// collapses owner/owned window chains (e.g. a game's real window plus
/// invisible helper windows) down to one representative per application.
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs; the decision that consumes this value ([`is_listable_window`]) is
/// unit tested in isolation.
pub(crate) fn is_root_owner_or_last_active_popup(hwnd: HWND) -> bool {
    // Safety: `hwnd` is a live handle; `GetAncestor`/`GetLastActivePopup`
    // perform no writes and take no pointers besides their `HWND` arguments.
    unsafe {
        let root_owner = GetAncestor(hwnd, GA_ROOTOWNER);
        GetLastActivePopup(root_owner) == hwnd
    }
}

/// Queries `DWMWA_CLOAKED` for `hwnd` -- true if DWM is currently hiding the
/// window from the screen despite it reporting `IsWindowVisible() == TRUE`
/// (e.g. a suspended UWP/Store-app window sitting on another virtual
/// desktop). Per the Old New Thing post cited in
/// `docs/research/game-window-filtering.md`, a failed/erroring
/// `DwmGetWindowAttribute` call is treated the same as "not cloaked" --
/// cloaking is an opt-in extra signal, not something whose absence should be
/// assumed on query failure.
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs; the decision that consumes this value ([`is_listable_window`]) is
/// unit tested in isolation.
pub(crate) fn is_window_cloaked(hwnd: HWND) -> bool {
    let mut is_cloaked: i32 = 0;
    // Safety: `hwnd` is a live handle; `&mut is_cloaked` is a valid pointer to
    // a correctly-sized (`i32`) buffer matching `cbAttribute`, per
    // `DwmGetWindowAttribute`'s documented contract for this attribute.
    let hresult = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as u32,
            &mut is_cloaked as *mut i32 as *mut core::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        )
    };
    hresult >= 0 && is_cloaked != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_listable_window_accepts_a_visible_window_with_a_non_empty_title() {
        assert!(is_listable_window("Notepad", true, false, true, false, false));
    }

    #[test]
    fn is_listable_window_rejects_an_invisible_window() {
        assert!(!is_listable_window("Notepad", false, false, true, false, false));
    }

    #[test]
    fn is_listable_window_rejects_a_visible_window_with_an_empty_title() {
        assert!(!is_listable_window("", true, false, true, false, false));
    }

    #[test]
    fn is_listable_window_rejects_a_visible_window_whose_title_is_only_whitespace() {
        assert!(!is_listable_window("   ", true, false, true, false, false));
    }

    #[test]
    fn is_listable_window_rejects_a_cloaked_window_even_with_a_visible_non_blank_title() {
        assert!(!is_listable_window("Notepad", true, true, true, false, false));
    }

    #[test]
    fn is_listable_window_rejects_an_owned_window_that_is_not_its_owners_last_active_popup() {
        assert!(!is_listable_window("Helper", true, false, false, false, false));
    }

    #[test]
    fn is_listable_window_accepts_a_non_root_owner_window_when_app_window_style_is_set() {
        assert!(is_listable_window("Overlay", true, false, false, false, true));
    }

    #[test]
    fn is_listable_window_rejects_a_tool_window() {
        assert!(!is_listable_window("Toolbar", true, false, true, true, false));
    }

    #[test]
    fn is_listable_window_accepts_a_tool_window_when_app_window_style_is_also_set() {
        assert!(is_listable_window("Toolbar", true, false, true, true, true));
    }

    #[test]
    fn is_listable_window_app_window_style_overrides_both_a_non_root_owner_and_a_tool_window_together() {
        // The strongest form of the documented WS_EX_APPWINDOW override:
        // forces inclusion even when BOTH other Layer-1 rejections
        // (non-representative owner-chain window AND tool-window style)
        // would otherwise apply simultaneously.
        assert!(is_listable_window("Overlay", true, false, false, true, true));
    }

    #[test]
    fn rank_window_with_no_signals_is_unknown() {
        assert_eq!(rank_window(false, false, false), WindowRank::Unknown);
    }

    #[test]
    fn rank_window_with_a_launcher_install_match_is_the_top_rank() {
        assert_eq!(rank_window(true, false, false), WindowRank::LauncherMatch);
    }

    #[test]
    fn rank_window_with_a_loaded_graphics_module_ranks_below_a_launcher_match() {
        assert_eq!(rank_window(false, true, false), WindowRank::GraphicsModuleLoaded);
    }

    #[test]
    fn rank_window_with_a_known_engine_window_class_ranks_below_a_graphics_module() {
        assert_eq!(rank_window(false, false, true), WindowRank::EngineWindowClass);
    }

    #[test]
    fn rank_window_prioritizes_launcher_match_over_every_other_signal() {
        assert_eq!(rank_window(true, true, true), WindowRank::LauncherMatch);
    }

    #[test]
    fn rank_window_prioritizes_graphics_module_over_engine_window_class() {
        assert_eq!(rank_window(false, true, true), WindowRank::GraphicsModuleLoaded);
    }

    #[test]
    fn window_rank_orders_from_weakest_to_strongest_signal() {
        assert!(WindowRank::Unknown < WindowRank::EngineWindowClass);
        assert!(WindowRank::EngineWindowClass < WindowRank::GraphicsModuleLoaded);
        assert!(WindowRank::GraphicsModuleLoaded < WindowRank::LauncherMatch);
    }

    #[test]
    fn is_known_engine_window_class_recognizes_unreal() {
        assert!(is_known_engine_window_class("UnrealWindow"));
    }

    #[test]
    fn is_known_engine_window_class_rejects_an_unrecognized_class() {
        assert!(!is_known_engine_window_class("Chrome_WidgetWin_1"));
    }

    #[test]
    fn is_known_engine_window_class_recognizes_sdl_and_glfw_prefixed_classes() {
        assert!(is_known_engine_window_class("SDL_app123"));
        assert!(is_known_engine_window_class("GLFW30"));
    }

    #[test]
    fn has_known_graphics_module_detects_dxgi() {
        assert!(has_known_graphics_module(&["dxgi.dll".to_string()]));
    }

    #[test]
    fn has_known_graphics_module_is_false_when_no_known_module_is_loaded() {
        assert!(!has_known_graphics_module(&["kernel32.dll".to_string(), "user32.dll".to_string()]));
    }

    #[test]
    fn matches_known_install_accepts_an_exe_inside_a_known_install_dir() {
        let install_dir = std::path::PathBuf::from(r"C:\Program Files (x86)\Steam\steamapps\common\Graveyard Keeper");
        let exe_path = install_dir.join("GraveyardKeeper.exe");
        assert!(matches_known_install(&exe_path, &[install_dir]));
    }

    #[test]
    fn matches_known_install_rejects_an_exe_outside_every_known_install_dir() {
        let install_dir = std::path::PathBuf::from(r"C:\Program Files (x86)\Steam\steamapps\common\Graveyard Keeper");
        let exe_path = std::path::PathBuf::from(r"C:\Users\Player\Downloads\SomeIndieGame\game.exe");
        assert!(!matches_known_install(&exe_path, &[install_dir]));
    }
}
