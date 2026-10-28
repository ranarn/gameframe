//! The tray menu's "Apply to Window" picker's two-layer game-window
//! filtering/ranking:
//!
//! - Layer 1 ([`eligibility_for_hwnd`]) hard-filters system-process, dialog,
//!   cloaked, owned and tool windows out of the list entirely.
//! - Layer 2 ([`rank_window`]) only ever reorders the remaining list by how
//!   confidently a window looks like a game (launcher install match, then
//!   loaded graphics module, then engine window class) -- it never hides a
//!   window, so an unrecognized one stays fully pickable.
//!
//! Split the same way as [`crate::win32`]:
//! - Pure decision logic ([`check_eligibility`], [`check_window_traits`],
//!   [`check_executable`], [`rank_window`],
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
use windows_sys::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, TH32CS_SNAPMODULE,
    TH32CS_SNAPMODULE32,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GA_ROOTOWNER, GW_OWNER, GWL_EXSTYLE, GetAncestor, GetClassNameW, GetLastActivePopup, GetWindow,
    GetWindowLongPtrW, GetWindowThreadProcessId, IsWindowVisible, WS_EX_APPWINDOW,
    WS_EX_DLGMODALFRAME, WS_EX_TOOLWINDOW,
};

/// The Layer 1 canonical-window boolean formula: the visibility, cloaking,
/// owner-chain and tool-window rules [`eligibility_for_hwnd`] builds on.
///
/// A window is canonical if:
/// - it's currently visible;
/// - it isn't cloaked (`DWMWA_CLOAKED` -- DWM-hidden windows, e.g. a
///   suspended UWP/Store-app window on another virtual desktop, that still
///   report `IsWindowVisible() == TRUE` with a stale title; see Raymond
///   Chen's "The Old New Thing");
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
/// Pure logic -- no `HWND`/OS dependency.
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

/// Executable file names (compared case-insensitively) whose windows must
/// never be tracked, auto-applied to or stripped: reshaping them breaks the
/// desktop (taskbar, Start menu, desktop, File Explorer windows) or the shell
/// hosts around it.
///
/// `ApplicationFrameHost.exe` is on the list on purpose: it is the process
/// that owns the top-level frame of every UWP/Store app, so a tracked entry
/// keyed on it would match every such app's window and not just one game.
/// Packaged UWP games are therefore unsupported.
const SYSTEM_PROCESS_DENYLIST: &[&str] = &[
    "explorer.exe",
    "ShellExperienceHost.exe",
    "StartMenuExperienceHost.exe",
    "SearchHost.exe",
    "ShellHost.exe",
    "SearchApp.exe",
    "TextInputHost.exe",
    "LockApp.exe",
    "ApplicationFrameHost.exe",
    "dwm.exe",
    "csrss.exe",
    "winlogon.exe",
    "taskmgr.exe",
    "sihost.exe",
];

/// The window class of standard Win32 dialog boxes.
const DIALOG_WINDOW_CLASS: &str = "#32770";

/// The per-window signals [`eligibility_for_hwnd`] decides on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WindowTraits<'a> {
    pub is_visible: bool,
    pub is_cloaked: bool,
    /// The window has an owner (`GetWindow(hwnd, GW_OWNER)` is non-null).
    pub is_owned: bool,
    pub is_root_owner_or_last_active_popup: bool,
    pub has_tool_window_style: bool,
    pub has_app_window_style: bool,
    /// `WS_EX_DLGMODALFRAME`, the extended style of dialog-box frames.
    pub has_dialog_frame_style: bool,
    pub class_name: &'a str,
}

/// Why [`eligibility_for_hwnd`] refused a window; its `Display` text is what
/// gets logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    UnknownExecutable,
    OwnExecutable,
    SystemProcess(String),
    DialogWindow,
    OwnedWindow,
    NotMainWindow,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownExecutable => f.write_str(
                "the window's executable could not be read (an elevated or protected process?)",
            ),
            Self::OwnExecutable => f.write_str("the window belongs to GameFrame itself"),
            Self::SystemProcess(name) => write!(f, "{name} is a system process"),
            Self::DialogWindow => f.write_str("the window is a dialog"),
            Self::OwnedWindow => f.write_str("the window is owned by another window"),
            Self::NotMainWindow => f.write_str("the window is not a visible main window"),
        }
    }
}

/// Refuses, in order: a window whose executable is unknown; GameFrame's own
/// windows; windows of a [`SYSTEM_PROCESS_DENYLIST`] process.
///
/// Pure logic -- no `HWND`/OS dependency.
fn check_executable(
    exe_file_name: Option<&str>,
    own_exe_file_name: Option<&str>,
) -> Result<(), Refusal> {
    let Some(exe_file_name) = exe_file_name else {
        return Err(Refusal::UnknownExecutable);
    };
    if own_exe_file_name.is_some_and(|own| own.eq_ignore_ascii_case(exe_file_name)) {
        return Err(Refusal::OwnExecutable);
    }
    if SYSTEM_PROCESS_DENYLIST
        .iter()
        .any(|denied| denied.eq_ignore_ascii_case(exe_file_name))
    {
        return Err(Refusal::SystemProcess(exe_file_name.to_owned()));
    }
    Ok(())
}

/// Refuses dialogs (the `#32770` class or `WS_EX_DLGMODALFRAME`, even with
/// `WS_EX_APPWINDOW`), owned windows without `WS_EX_APPWINDOW` and anything
/// that is not [`is_canonical_window`] (hidden, cloaked, tool windows,
/// non-representative popups).
///
/// Pure logic -- no `HWND`/OS dependency.
fn check_window_traits(window: &WindowTraits<'_>) -> Result<(), Refusal> {
    if window.class_name == DIALOG_WINDOW_CLASS || window.has_dialog_frame_style {
        return Err(Refusal::DialogWindow);
    }
    if window.is_owned && !window.has_app_window_style {
        return Err(Refusal::OwnedWindow);
    }
    if !is_canonical_window(
        window.is_visible,
        window.is_cloaked,
        window.is_root_owner_or_last_active_popup,
        window.has_tool_window_style,
        window.has_app_window_style,
    ) {
        return Err(Refusal::NotMainWindow);
    }
    Ok(())
}

/// Whether a window title is worth offering in the "Apply to Window" picker:
/// an untitled window isn't something a user could sensibly pick by name.
pub(crate) fn is_listable_title(title: &str) -> bool {
    !title.trim().is_empty()
}

/// GameFrame's own executable file name, resolved once.
fn own_exe_file_name() -> Option<&'static str> {
    static NAME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|path| path.file_name().map(|n| n.to_string_lossy().into_owned()))
    })
    .as_deref()
}

/// Gathers `hwnd`'s signals and its owning process's executable path (resolved
/// once), then decides whether the window may be tracked, auto-applied to or
/// stripped: [`check_window_traits`] first, then [`check_executable`]. On
/// success returns the executable path so callers need not resolve it again.
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs.
pub(crate) fn eligibility_for_hwnd(hwnd: HWND) -> Result<std::path::PathBuf, Refusal> {
    let ex_style = window_ex_style(hwnd);
    let class_name = window_class_name(hwnd);
    // Safety: `hwnd` is a live handle; `IsWindowVisible`/`GetWindow` perform
    // no writes.
    let (is_visible, is_owned) = unsafe {
        (
            IsWindowVisible(hwnd) != 0,
            !GetWindow(hwnd, GW_OWNER).is_null(),
        )
    };
    let window = WindowTraits {
        is_visible,
        is_cloaked: is_window_cloaked(hwnd),
        is_owned,
        is_root_owner_or_last_active_popup: is_root_owner_or_last_active_popup(hwnd),
        has_tool_window_style: ex_style & WS_EX_TOOLWINDOW != 0,
        has_app_window_style: ex_style & WS_EX_APPWINDOW != 0,
        has_dialog_frame_style: ex_style & WS_EX_DLGMODALFRAME != 0,
        class_name: &class_name,
    };
    check_eligibility(&window, own_exe_file_name(), || exe_path_for_window(hwnd))
}

/// The decision behind [`eligibility_for_hwnd`]: [`check_window_traits`] first,
/// then, only for a window that passes, `resolve_exe` and [`check_executable`].
/// Returns the resolved executable path.
///
/// The cheap window checks run first: resolving the executable opens the
/// process, which is wasted on the many windows that fail them anyway.
///
/// Pure logic -- no `HWND`/OS dependency; `resolve_exe` is the OS-facing part.
fn check_eligibility(
    window: &WindowTraits<'_>,
    own_exe_file_name: Option<&str>,
    resolve_exe: impl FnOnce() -> Option<std::path::PathBuf>,
) -> Result<std::path::PathBuf, Refusal> {
    check_window_traits(window)?;
    let exe_path = resolve_exe().ok_or(Refusal::UnknownExecutable)?;
    let exe_file_name = exe_path.file_name().map(|n| n.to_string_lossy());
    check_executable(exe_file_name.as_deref(), own_exe_file_name)?;
    Ok(exe_path)
}

/// Layer 2 of the game-window filtering: how confidently a window looks
/// like a game, from weakest to strongest signal (declaration order doubles
/// as `Ord`/`PartialOrd` ranking, lowest variant first) -- used only to sort
/// the "Apply to Window" picker's list and/or badge an entry; unlike
/// [`eligibility_for_hwnd`]'s Layer-1 hard filters, a low (or [`Unknown`]) rank
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
pub(crate) fn rank_window(
    is_launcher_match: bool,
    has_graphics_module: bool,
    has_known_engine_window_class: bool,
) -> WindowRank {
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

/// The Layer-2 window-class heuristic list: a small, explicitly-known set of
/// game-engine window classes, used only as the weakest tie-breaker signal in
/// [`rank_window`] -- never as an exclusion.
const KNOWN_ENGINE_WINDOW_CLASSES: &[&str] =
    &["UnrealWindow", "Direct3DWindowClass", "YYGameMakerYY"];

/// Prefixes for engine window classes that vary per-app (SDL/GLFW embed the
/// hosting app's own name after a fixed prefix), checked in addition to
/// [`KNOWN_ENGINE_WINDOW_CLASSES`]'s exact-match list.
const KNOWN_ENGINE_WINDOW_CLASS_PREFIXES: &[&str] = &["SDL_app", "GLFW"];

/// Whether `class_name` (a window's `GetClassNameW` result) matches one of
/// the small, explicitly-known game-engine window classes. Pure string matching -- no OS
/// dependency -- so it's unit testable in isolation from the real
/// `GetClassNameW` call that supplies `class_name`.
fn is_known_engine_window_class(class_name: &str) -> bool {
    KNOWN_ENGINE_WINDOW_CLASSES.contains(&class_name)
        || KNOWN_ENGINE_WINDOW_CLASS_PREFIXES
            .iter()
            .any(|prefix| class_name.starts_with(prefix))
}

/// The Layer-2 loaded-graphics-module list: DLLs whose presence in a
/// process's already-loaded module list is a medium-confidence "this process
/// talks to a graphics API" signal for [`rank_window`] -- never an exclusion.
const KNOWN_GRAPHICS_MODULES: &[&str] = &[
    "d3d9.dll",
    "d3d11.dll",
    "dxgi.dll",
    "d3d12.dll",
    "opengl32.dll",
    "vulkan-1.dll",
];

/// Whether any of `loaded_module_names` (a process's already-loaded module
/// list, as gathered via `CreateToolhelp32Snapshot(TH32CS_SNAPMODULE |
/// TH32CS_SNAPMODULE32, ...)`) matches one of the known graphics-API DLLs.
/// Case-insensitive, since Windows module names aren't case-sensitive. Pure
/// string matching -- no OS dependency -- so it's unit testable in isolation
/// from the real toolhelp snapshot that supplies `loaded_module_names`.
fn has_known_graphics_module(loaded_module_names: &[String]) -> bool {
    loaded_module_names.iter().any(|loaded| {
        KNOWN_GRAPHICS_MODULES
            .iter()
            .any(|known| loaded.eq_ignore_ascii_case(known))
    })
}

/// Whether `exe_path` (a window's owning process's executable path, as
/// resolved via `QueryFullProcessImageNameW`) sits inside one of
/// `known_install_dirs` -- a cached snapshot of known launcher (Steam, via
/// the `steamlocate` crate) install directories. The strongest of the three
/// Layer-2 signals: a match
/// essentially never false-positives, since it doesn't depend on window
/// styles or graphics APIs at all.
///
/// Pure path matching -- no OS/filesystem dependency -- so it's unit testable
/// in isolation from the real `steamlocate`/`QueryFullProcessImageNameW`
/// calls that supply `exe_path`/`known_install_dirs`.
fn matches_known_install(
    exe_path: &std::path::Path,
    known_install_dirs: &[std::path::PathBuf],
) -> bool {
    known_install_dirs
        .iter()
        .any(|install_dir| exe_path.starts_with(install_dir))
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

/// A comparison key for an executable path: lowercased, with a `\\?\` verbatim
/// prefix removed, so the stored and the live spelling of one file match.
fn path_key(path: &std::path::Path) -> String {
    let text = path.to_string_lossy().to_lowercase();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
}

/// The process-level Layer-2 signals for `exe_path`.
///
/// A tracked game (`tracked_exes` holds the tracked games' executable paths)
/// is never module-snapshotted: that opens a handle to a process that may be
/// protected by anti-cheat, and the user already told GameFrame it is a game,
/// so it ranks as a launcher match without any lookup. For any other process
/// the Steam install directories (`steam_dirs`) and the loaded modules
/// (`modules`) are gathered lazily, only if called.
///
/// Pure: the two OS lookups come in as closures, so the skip is unit tested.
fn process_rank_signals(
    exe_path: &std::path::Path,
    tracked_exes: &[std::path::PathBuf],
    steam_dirs: impl FnOnce() -> Vec<std::path::PathBuf>,
    modules: impl FnOnce() -> Vec<String>,
) -> ProcessRankSignals {
    let exe_key = path_key(exe_path);
    if tracked_exes
        .iter()
        .any(|tracked| path_key(tracked) == exe_key)
    {
        return ProcessRankSignals {
            is_launcher_match: true,
            has_graphics_module: false,
        };
    }
    ProcessRankSignals {
        is_launcher_match: matches_known_install(exe_path, &steam_dirs()),
        has_graphics_module: has_known_graphics_module(&modules()),
    }
}

/// Gathers `hwnd`'s Layer-2 signals and combines them via [`rank_window`].
/// `exe_path` is the already-resolved executable of `hwnd`'s process.
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
pub(crate) fn rank_window_for_hwnd(
    hwnd: HWND,
    exe_path: &std::path::Path,
    tracked_exes: &[std::path::PathBuf],
    cache: &mut std::collections::HashMap<u32, ProcessRankSignals>,
) -> WindowRank {
    let mut pid: u32 = 0;
    // Safety: `hwnd` is a live handle; `&mut pid` is a valid out-pointer for
    // `GetWindowThreadProcessId`'s documented contract.
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }

    let process_signals = *cache.entry(pid).or_insert_with(|| {
        process_rank_signals(exe_path, tracked_exes, known_steam_install_dirs, || {
            loaded_module_names(pid)
        })
    });

    let has_known_engine_class = is_known_engine_window_class(&window_class_name(hwnd));

    rank_window(
        process_signals.is_launcher_match,
        process_signals.has_graphics_module,
        has_known_engine_class,
    )
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
/// `pub(crate)` (rather than private) since the PID<->HWND resolution
/// (`crate::win32::find_pid_by_exe_path`) also calls this directly, to
/// confirm a toolhelp snapshot's cheap file-name pre-filter
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
    Some(std::path::PathBuf::from(String::from_utf16_lossy(
        &buffer[..size as usize],
    )))
}

/// Resolves the executable path of the process that owns `hwnd`, via
/// `GetWindowThreadProcessId` + [`process_exe_path`]. Shared by the
/// borderless-hotkey path and the tray's "Apply to Window" path, so the
/// PID/exe-path resolution glue lives in one place.
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
/// transiently inconsistent)
/// -- treated as "unknown," not "not a game," since this only ever feeds a
/// ranking signal, never an exclusion.
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs; the decision that consumes this value
/// ([`has_known_graphics_module`]) is unit tested in isolation.
fn loaded_module_names(pid: u32) -> Vec<String> {
    // Safety: `pid` is a plain integer; `CreateToolhelp32Snapshot` takes no
    // pointers.
    let snapshot =
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }

    let mut names = Vec::new();
    // SAFETY: all-zero bytes are a valid value of this plain C struct.
    let mut entry: MODULEENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;

    // Safety: `snapshot` is the live handle just created above; `entry` is a
    // valid, correctly-sized out-pointer with `dwSize` set, per
    // `Module32FirstW`/`Module32NextW`'s documented contract.
    let mut has_entry = unsafe { Module32FirstW(snapshot, &mut entry) } != 0;
    while has_entry {
        let len = entry
            .szModule
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szModule.len());
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
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<SteamInstallDirsSnapshot>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

/// How long a cached Steam-install-directory scan is trusted before
/// [`known_steam_install_dirs`] rescans -- installed games don't change
/// per-frame, so this is refreshed lazily (e.g. roughly once per tray-menu
/// open at typical interaction cadence) rather than on every
/// `crate::win32::list_visible_windows` call.
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
    let mut guard = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((cached_at, dirs)) = guard.as_ref()
        && cached_at.elapsed() < STEAM_INSTALL_DIRS_CACHE_TTL
    {
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
/// algorithm: walks to
/// `hwnd`'s root owner via `GetAncestor(hwnd, GA_ROOTOWNER)`, then finds that
/// owner's current last-active-visible-popup via `GetLastActivePopup` --
/// `hwnd` qualifies only if it *is* that computed representative window. This
/// collapses owner/owned window chains (e.g. a game's real window plus
/// invisible helper windows) down to one representative per application.
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs; the decision that consumes this value ([`check_window_traits`]) is
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
/// desktop). Per Raymond Chen's "The Old New Thing", a failed/erroring
/// `DwmGetWindowAttribute` call is treated the same as "not cloaked" --
/// cloaking is an opt-in extra signal, not something whose absence should be
/// assumed on query failure.
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs; the decision that consumes this value ([`check_window_traits`]) is
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

    /// Runs the production decision with an executable of the given file name
    /// (`None` = unresolvable).
    fn eligibility(
        exe_file_name: Option<&str>,
        own_exe_file_name: Option<&str>,
        window: &WindowTraits<'_>,
    ) -> Result<(), Refusal> {
        check_eligibility(window, own_exe_file_name, || {
            exe_file_name.map(|name| std::path::Path::new("C:\\games").join(name))
        })
        .map(|_| ())
    }

    #[test]
    fn is_canonical_window_accepts_a_visible_window() {
        assert!(is_canonical_window(true, false, true, false, false));
    }

    #[test]
    fn is_canonical_window_rejects_an_invisible_window() {
        assert!(!is_canonical_window(false, false, true, false, false));
    }

    #[test]
    fn is_canonical_window_rejects_a_cloaked_window() {
        assert!(!is_canonical_window(true, true, true, false, false));
    }

    #[test]
    fn is_canonical_window_rejects_an_owned_window_that_is_not_its_owners_last_active_popup() {
        assert!(!is_canonical_window(true, false, false, false, false));
    }

    #[test]
    fn is_canonical_window_accepts_a_non_root_owner_window_when_app_window_style_is_set() {
        assert!(is_canonical_window(true, false, false, false, true));
    }

    #[test]
    fn is_canonical_window_rejects_a_tool_window() {
        assert!(!is_canonical_window(true, false, true, true, false));
    }

    #[test]
    fn is_canonical_window_accepts_a_tool_window_when_app_window_style_is_also_set() {
        assert!(is_canonical_window(true, false, true, true, true));
    }

    #[test]
    fn is_canonical_window_app_window_style_overrides_both_a_non_root_owner_and_a_tool_window_together()
     {
        // The strongest form of the documented WS_EX_APPWINDOW override:
        // forces inclusion even when BOTH other Layer-1 rejections
        // (non-representative owner-chain window AND tool-window style)
        // would otherwise apply simultaneously.
        assert!(is_canonical_window(true, false, false, true, true));
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
        assert_eq!(
            rank_window(false, true, false),
            WindowRank::GraphicsModuleLoaded
        );
    }

    #[test]
    fn rank_window_with_a_known_engine_window_class_ranks_below_a_graphics_module() {
        assert_eq!(
            rank_window(false, false, true),
            WindowRank::EngineWindowClass
        );
    }

    #[test]
    fn rank_window_prioritizes_launcher_match_over_every_other_signal() {
        assert_eq!(rank_window(true, true, true), WindowRank::LauncherMatch);
    }

    #[test]
    fn rank_window_prioritizes_graphics_module_over_engine_window_class() {
        assert_eq!(
            rank_window(false, true, true),
            WindowRank::GraphicsModuleLoaded
        );
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
        assert!(!has_known_graphics_module(&[
            "kernel32.dll".to_string(),
            "user32.dll".to_string()
        ]));
    }

    #[test]
    fn matches_known_install_accepts_an_exe_inside_a_known_install_dir() {
        let install_dir = std::path::PathBuf::from(
            r"C:\Program Files (x86)\Steam\steamapps\common\Graveyard Keeper",
        );
        let exe_path = install_dir.join("GraveyardKeeper.exe");
        assert!(matches_known_install(&exe_path, &[install_dir]));
    }

    #[test]
    fn matches_known_install_rejects_an_exe_outside_every_known_install_dir() {
        let install_dir = std::path::PathBuf::from(
            r"C:\Program Files (x86)\Steam\steamapps\common\Graveyard Keeper",
        );
        let exe_path =
            std::path::PathBuf::from(r"C:\Users\Player\Downloads\SomeIndieGame\game.exe");
        assert!(!matches_known_install(&exe_path, &[install_dir]));
    }

    #[test]
    fn a_tracked_game_is_never_module_snapshotted_or_steam_scanned() {
        let exe = std::path::PathBuf::from(r"D:\Games\Foo\foo.exe");
        let signals = process_rank_signals(
            &exe,
            std::slice::from_ref(&exe),
            || panic!("steam scan for a tracked game"),
            || panic!("module snapshot of a tracked game"),
        );
        assert!(signals.is_launcher_match);
        assert!(!signals.has_graphics_module);
    }

    #[test]
    fn a_tracked_game_matches_despite_case_and_verbatim_prefix() {
        let tracked = std::path::PathBuf::from(r"\\?\D:\Games\Foo\Foo.exe");
        let exe = std::path::PathBuf::from(r"d:\games\foo\foo.EXE");
        let signals = process_rank_signals(
            &exe,
            &[tracked],
            || panic!("steam scan for a tracked game"),
            || panic!("module snapshot of a tracked game"),
        );
        assert!(signals.is_launcher_match);
    }

    #[test]
    fn an_untracked_process_is_still_module_ranked() {
        let exe = std::path::PathBuf::from(r"D:\Games\Bar\bar.exe");
        let other = std::path::PathBuf::from(r"D:\Games\Foo\foo.exe");
        let signals =
            process_rank_signals(&exe, &[other], Vec::new, || vec!["d3d11.dll".to_string()]);
        assert!(!signals.is_launcher_match);
        assert!(signals.has_graphics_module);
    }

    #[test]
    fn is_listable_title_accepts_a_non_blank_title() {
        assert!(is_listable_title("Notepad"));
    }

    #[test]
    fn is_listable_title_rejects_an_empty_title() {
        assert!(!is_listable_title(""));
    }

    #[test]
    fn is_listable_title_rejects_a_whitespace_only_title() {
        assert!(!is_listable_title("   "));
    }

    fn game_window() -> WindowTraits<'static> {
        WindowTraits {
            is_visible: true,
            is_cloaked: false,
            is_owned: false,
            is_root_owner_or_last_active_popup: true,
            has_tool_window_style: false,
            has_app_window_style: false,
            has_dialog_frame_style: false,
            class_name: "UnrealWindow",
        }
    }

    #[test]
    fn eligibility_accepts_a_normal_top_level_game_window() {
        assert_eq!(
            eligibility(Some("game.exe"), Some("gameframe.exe"), &game_window()),
            Ok(())
        );
    }

    #[test]
    fn eligibility_refuses_every_denylisted_exe_case_insensitively() {
        for denied in SYSTEM_PROCESS_DENYLIST {
            for name in [
                denied.to_string(),
                denied.to_uppercase(),
                denied.to_lowercase(),
            ] {
                assert_eq!(
                    eligibility(Some(&name), None, &game_window()),
                    Err(Refusal::SystemProcess(name.clone())),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn eligibility_denylist_covers_the_shell_and_core_system_processes() {
        for required in [
            "explorer.exe",
            "ShellExperienceHost.exe",
            "StartMenuExperienceHost.exe",
            "SearchHost.exe",
            "ShellHost.exe",
            "SearchApp.exe",
            "TextInputHost.exe",
            "LockApp.exe",
            "dwm.exe",
            "csrss.exe",
            "winlogon.exe",
            "taskmgr.exe",
            "sihost.exe",
        ] {
            assert!(
                eligibility(Some(required), None, &game_window()).is_err(),
                "{required}"
            );
        }
    }

    #[test]
    fn eligibility_refuses_gameframes_own_exe_case_insensitively() {
        assert_eq!(
            eligibility(Some("GAMEFRAME.EXE"), Some("gameframe.exe"), &game_window()),
            Err(Refusal::OwnExecutable)
        );
    }

    #[test]
    fn eligibility_refuses_a_window_with_an_unknown_exe() {
        assert_eq!(
            eligibility(None, None, &game_window()),
            Err(Refusal::UnknownExecutable)
        );
    }

    #[test]
    fn eligibility_refuses_an_owned_window() {
        let window = WindowTraits {
            is_owned: true,
            ..game_window()
        };
        assert_eq!(
            eligibility(Some("game.exe"), None, &window),
            Err(Refusal::OwnedWindow)
        );
    }

    #[test]
    fn eligibility_refuses_a_tool_window() {
        let window = WindowTraits {
            has_tool_window_style: true,
            ..game_window()
        };
        assert_eq!(
            eligibility(Some("game.exe"), None, &window),
            Err(Refusal::NotMainWindow)
        );
    }

    #[test]
    fn eligibility_refuses_a_dialog_class_window() {
        let window = WindowTraits {
            class_name: "#32770",
            ..game_window()
        };
        assert_eq!(
            eligibility(Some("game.exe"), None, &window),
            Err(Refusal::DialogWindow)
        );
    }

    #[test]
    fn eligibility_refuses_a_dialog_frame_window_even_when_owned_with_app_window_style() {
        let window = WindowTraits {
            is_owned: true,
            has_app_window_style: true,
            has_dialog_frame_style: true,
            ..game_window()
        };
        assert_eq!(
            eligibility(Some("game.exe"), None, &window),
            Err(Refusal::DialogWindow)
        );
    }

    #[test]
    fn eligibility_refuses_a_hidden_or_cloaked_window() {
        for window in [
            WindowTraits {
                is_visible: false,
                ..game_window()
            },
            WindowTraits {
                is_cloaked: true,
                ..game_window()
            },
        ] {
            assert_eq!(
                eligibility(Some("game.exe"), None, &window),
                Err(Refusal::NotMainWindow)
            );
        }
    }

    #[test]
    fn eligibility_accepts_an_owned_non_dialog_window_with_app_window_style() {
        let window = WindowTraits {
            is_owned: true,
            has_app_window_style: true,
            ..game_window()
        };
        assert_eq!(eligibility(Some("game.exe"), None, &window), Ok(()));
    }

    #[test]
    fn eligibility_refuses_a_non_representative_popup() {
        let window = WindowTraits {
            is_root_owner_or_last_active_popup: false,
            ..game_window()
        };
        assert_eq!(
            eligibility(Some("game.exe"), None, &window),
            Err(Refusal::NotMainWindow)
        );
    }

    #[test]
    fn eligibility_reports_a_dialog_before_a_system_process() {
        let window = WindowTraits {
            class_name: "#32770",
            ..game_window()
        };
        assert_eq!(
            eligibility(Some("explorer.exe"), None, &window),
            Err(Refusal::DialogWindow)
        );
    }
}
