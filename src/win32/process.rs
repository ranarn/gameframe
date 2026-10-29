//! Process and window queries: elevation, the picker's window list, and
//! resolving a tracked game's running window.

use super::{Win32Error, get_window_region, is_window_hung_logged};
use crate::region_calc::MonitorGeometry;
use crate::wide::encode_wide;
use crate::window_filter;
use crate::window_filter::WindowRank;
use std::path::Path;
use windows_sys::Win32::Foundation::{CloseHandle, HWND, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, SW_SHOWNORMAL,
};

/// Resolves the current foreground window's `HWND`, via `GetForegroundWindow()`.
///
/// Returns `None` if there is no foreground window. Used directly by
/// [`crate::app::handle_toggle_borderless`], which needs the raw `HWND` to
/// look up and act on that window.
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs.
pub fn foreground_window() -> Option<HWND> {
    // Safety: `GetForegroundWindow` takes no arguments and performs no
    // writes; its result is null-checked immediately below before any other
    // call uses it.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() { None } else { Some(hwnd) }
}

/// Re-launches the current executable elevated (UAC "runas" prompt),
/// following `ShellExecuteW`'s documented `"runas"` verb contract.
///
/// Used when a window manipulation fails with an access-denied-style error
/// ([`Win32Error::is_access_denied`]) -- GameFrame runs unelevated by
/// default and only asks for elevation reactively, at the point a specific
/// operation actually needs it, rather than requiring admin rights upfront.
///
/// OS-facing (spawns a real elevated child process via a UAC prompt); not
/// exercised by this crate's automated tests -- see module docs.
pub fn relaunch_elevated() -> Result<(), Win32Error> {
    let exe_path = std::env::current_exe().map_err(Win32Error::Os)?;
    let verb = encode_wide("runas");
    let file = encode_wide(exe_path.to_string_lossy().as_ref());
    // The flag tells the child to wait for this (old) instance to release
    // the single-instance mutex instead of exiting on first contact.
    let params = encode_wide(crate::single_instance::ELEVATED_RELAUNCH_FLAG);

    // Safety: `verb`, `file` and `params` are valid, null-terminated UTF-16
    // buffers kept alive for the duration of this call; the remaining
    // pointer arguments (`hwnd`, `lpdirectory`) are null, which
    // `ShellExecuteW` documents as valid ("use current/none").
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // Per ShellExecuteW's documented contract, a return value greater than
    // 32 indicates success; anything else IS the failure code itself (e.g.
    // distinguishing a declined UAC prompt from other failures) -- unlike
    // most Win32 calls, ShellExecuteW is not documented to route failures
    // through `GetLastError()`, so the error must be built from `result`
    // rather than from a (potentially unrelated/misleading) `GetLastError()`
    // call.
    let code = result as isize;
    if code <= 32 {
        return Err(Win32Error::Os(std::io::Error::from_raw_os_error(
            code as i32,
        )));
    }
    Ok(())
}

/// Whether this process runs with an elevated (full administrator) token.
/// Fails closed: a failure to read the token counts as elevated (logged once),
/// since the only callers use this to refuse risky work.
pub fn is_process_elevated() -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    let elevated = query_token_elevation();
    if elevated.is_none() && !LOGGED.swap(true, Ordering::Relaxed) {
        log!("could not read the process token's elevation; treating it as elevated");
    }
    elevated.unwrap_or(true)
}

/// `Some(is_elevated)`, or `None` when the token can't be read.
fn query_token_elevation() -> Option<bool> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token = std::ptr::null_mut();
    // SAFETY: `token` is a valid out pointer; the pseudo-handle from
    // `GetCurrentProcess` needs no closing.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned = 0u32;
    // SAFETY: `elevation` is a writable TOKEN_ELEVATION of the size passed, and
    // `token` is a valid handle that is closed exactly once below.
    let ok = unsafe {
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
        CloseHandle(token);
        ok
    };
    (ok != 0).then_some(elevation.TokenIsElevated != 0)
}

/// Per-`EnumWindows`-call userdata smuggled through `EnumWindows`'s `LPARAM`
/// parameter: the listable windows gathered so far, plus a per-PID
/// process-level-signal memoization cache (see
/// [`window_filter::rank_window_for_hwnd`]) so a process owning multiple
/// top-level listable windows only pays for one launcher-match/graphics-
/// module gather instead of one per window -- the window-class signal is
/// deliberately excluded from this cache since it's a per-window, not
/// per-process, property.
struct EnumWindowsState {
    windows: Vec<(HWND, String, WindowRank)>,
    rank_cache: std::collections::HashMap<u32, window_filter::ProcessRankSignals>,
    /// Executable paths of the tracked games; their processes are never
    /// module-snapshotted.
    tracked_exes: Vec<std::path::PathBuf>,
}

/// Lists the pickable top-level windows, likely games first. Blocking and
/// slow (window walk, exe paths, module snapshots, Steam scan): call it from
/// a worker thread, never the UI thread. `tracked_exes` are the tracked games'
/// executable paths.
pub fn list_visible_windows(tracked_exes: Vec<std::path::PathBuf>) -> Vec<(HWND, String)> {
    let mut state = EnumWindowsState {
        windows: Vec::new(),
        rank_cache: std::collections::HashMap::new(),
        tracked_exes,
    };
    // Safety: `state` outlives this call (it's a local that isn't dropped
    // until after `EnumWindows` returns); it's cast to a raw pointer solely
    // to smuggle it through `EnumWindows`'s documented `LPARAM` userdata
    // parameter, and `enum_windows_callback` below casts it back to the
    // exact same type before dereferencing it.
    unsafe {
        EnumWindows(
            Some(enum_windows_callback),
            std::ptr::addr_of_mut!(state) as isize,
        );
    }
    // Layer 2: a stable sort
    // by descending `WindowRank` surfaces likely games first in the picker,
    // without ever removing a window ranked `WindowRank::Unknown` -- ranking
    // only ever reorders, it never filters. `Vec::sort_by_key` is a stable
    // sort, so windows that tie on rank keep the relative order `EnumWindows`
    // produced them in (roughly Z-order).
    state.windows.sort_by_key(|w| std::cmp::Reverse(w.2));
    state
        .windows
        .into_iter()
        .map(|(hwnd, title, _rank)| (hwnd, title))
        .collect()
}

unsafe extern "system" fn enum_windows_callback(hwnd: HWND, lparam: isize) -> i32 {
    // Safety: `lparam` is exactly the pointer `list_visible_windows` passed
    // in, valid for the duration of the `EnumWindows` call that is
    // synchronously invoking this callback.
    let state = unsafe { &mut *(lparam as *mut EnumWindowsState) };

    // A hung window can't be applied to anyway, and querying it may block.
    if is_window_hung_logged(hwnd) {
        return 1;
    }

    // An untitled window isn't something a user could sensibly pick by name.
    let title = window_text(hwnd);
    if window_filter::is_listable_title(&title)
        && let Ok(exe_path) = window_filter::eligibility_for_hwnd(hwnd)
    {
        let rank = window_filter::rank_window_for_hwnd(
            hwnd,
            &exe_path,
            &state.tracked_exes,
            &mut state.rank_cache,
        );
        state.windows.push((hwnd, title, rank));
    }
    1 // TRUE: keep enumerating.
}

/// Reads a window's title via `GetWindowTextLengthW`/`GetWindowTextW`,
/// returning an empty string if the window has no title (or either call
/// fails) rather than propagating an error -- an untitled window is simply
/// filtered out by [`crate::window_filter::is_listable_title`], not treated
/// as a hard failure of the whole enumeration.
pub(crate) fn window_text(hwnd: HWND) -> String {
    // Safety: `hwnd` is a live handle; `GetWindowTextLengthW` performs no
    // writes.
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }

    let mut buffer = vec![0u16; len as usize + 1];
    // Safety: `hwnd` is a live handle; `buffer` is a valid, correctly-sized
    // (`len + 1`, matching `nMaxCount`) out-buffer for `GetWindowTextW` to
    // write into, per its documented contract.
    let copied = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if copied <= 0 {
        return String::new();
    }

    String::from_utf16_lossy(&buffer[..copied as usize])
}

/// Picks a single window among `candidates` (each a live top-level window's
/// `HWND` paired with its screen area in pixels, gathered by
/// [`windows_for_pid`]) to treat as "the" window for the live
/// re-apply, when a tracked game's process owns more than one -- the pure
/// decision behind [`resolve_hwnd_for_pid`], separated out so it's testable
/// without a real `EnumWindows`/`GetWindowRect`/`GetForegroundWindow` call.
///
/// Disambiguation order: prefer the
/// window matching `foreground` (the live `GetForegroundWindow()` result), if
/// it's among `candidates`; otherwise the largest by `area`; otherwise the
/// first one gathered (a tie in area also falls through to "first found",
/// since ties are broken by strict-greater-than below).
///
/// Pure -- plain data in, plain data out -- so it's unit tested in complete
/// isolation from the real window enumeration/measurement.
fn choose_candidate_window(candidates: &[(HWND, i64)], foreground: Option<HWND>) -> Option<HWND> {
    if foreground.is_some_and(|fg| candidates.iter().any(|&(hwnd, _)| hwnd == fg)) {
        return foreground;
    }

    let mut best: Option<(HWND, i64)> = None;
    for &(hwnd, area) in candidates {
        let is_better = match best {
            Some((_, best_area)) => area > best_area,
            None => true,
        };
        if is_better {
            best = Some((hwnd, area));
        }
    }
    best.map(|(hwnd, _)| hwnd)
}

/// Cheap, case-insensitive pre-filter comparing a toolhelp snapshot entry's
/// bare file name (`PROCESSENTRY32W::szExeFile`, which never carries a
/// directory component) against `target_exe_path`'s own file name -- the
/// first of the two checks [`find_pid_by_exe_path`] applies before accepting
/// a PID match; a full-path confirmation via
/// [`crate::window_filter::process_exe_path`] still follows, so two
/// processes that merely share an executable *name* in different
/// directories are never confused for the same tracked game.
///
/// Pure string comparison -- no OS dependency -- so it's unit tested in
/// complete isolation from the real toolhelp snapshot.
fn exe_file_name_matches(candidate_file_name: &str, target_exe_path: &Path) -> bool {
    target_exe_path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|target_name| target_name.eq_ignore_ascii_case(candidate_file_name))
}

/// Per-[`windows_for_pid`]-call userdata smuggled through `EnumWindows`'s
/// `LPARAM` parameter -- the target PID to filter on, plus the listable
/// windows gathered so far. Analogous to [`EnumWindowsState`], but scoped to
/// a single PID rather than the whole desktop, and with no ranking (the
/// live re-apply doesn't need a "most game-like" ordering, only a set of
/// candidates to disambiguate via [`choose_candidate_window`]).
struct PidWindowsState {
    target_pid: u32,
    windows: Vec<HWND>,
    /// Why each of the process's other top-level windows was refused.
    refusals: Vec<window_filter::Refusal>,
}

unsafe extern "system" fn enum_windows_for_pid_callback(hwnd: HWND, lparam: isize) -> i32 {
    // Safety: `lparam` is exactly the pointer `windows_for_pid` passed in,
    // valid for the duration of the `EnumWindows` call that is synchronously
    // invoking this callback.
    let state = unsafe { &mut *(lparam as *mut PidWindowsState) };

    let mut window_pid: u32 = 0;
    // Safety: `hwnd` is a live handle supplied by `EnumWindows` itself;
    // `&mut window_pid` is a valid out-pointer for
    // `GetWindowThreadProcessId`'s documented contract.
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut window_pid);
    }
    if window_pid != state.target_pid {
        return 1; // TRUE: keep enumerating -- not this process's window.
    }

    // The same eligibility check the picker uses, minus its non-blank-title
    // requirement: a live re-apply target doesn't need a human-readable title.
    match window_filter::eligibility_for_hwnd(hwnd) {
        Ok(_) => state.windows.push(hwnd),
        Err(refusal) => state.refusals.push(refusal),
    }
    1 // TRUE: keep enumerating.
}

/// Enumerates every top-level window belonging to `target_pid` that passes
/// the same Layer 1 canonical-window filtering the "Apply to Window" picker
/// uses (see [`enum_windows_for_pid_callback`]) -- the candidate-gathering
/// step behind [`resolve_hwnd_for_pid`]. A separate, PID-scoped enumeration
/// rather than a `pid` filter parameter added to [`list_visible_windows`]
/// itself, leaving that function (and the picker it serves) independent of
/// this one.
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs.
fn windows_for_pid(target_pid: u32) -> (Vec<HWND>, Vec<window_filter::Refusal>) {
    let mut state = PidWindowsState {
        target_pid,
        windows: Vec::new(),
        refusals: Vec::new(),
    };
    // Safety: `state` outlives this call; see `list_visible_windows`'s
    // identical `EnumWindows` safety comment for why casting it through a
    // raw pointer here is sound.
    unsafe {
        EnumWindows(
            Some(enum_windows_for_pid_callback),
            std::ptr::addr_of_mut!(state) as isize,
        );
    }
    (state.windows, state.refusals)
}

/// Looks up the PID of a running process whose executable path is
/// `exe_path`, via a `TH32CS_SNAPPROCESS` toolhelp snapshot. Each entry's
/// bare file name (`PROCESSENTRY32W::szExeFile`) is first cheaply
/// pre-filtered via [`exe_file_name_matches`], then confirmed with a
/// full-path comparison via [`window_filter::process_exe_path`] before being
/// accepted -- guarding against two different processes that merely happen
/// to share an executable *name* in different install directories.
///
/// Returns the first live match found; `None` if no running process matches
/// (the common case for a tracked game that isn't currently running -- see
/// [`resolve_running_window`]'s doc comment).
///
/// The file-name pre-filter above is case-insensitive, but the full-path
/// confirmation that follows it is a plain, case-sensitive string comparison
/// against `exe_path`. Same accepted-risk rationale as
/// `profile_store::upsert_tracked_game`'s doc comment: both sides of the
/// comparison are resolved via the same OS API
/// (`QueryFullProcessImageNameW`, see [`window_filter::process_exe_path`]),
/// which returns a consistent casing run-to-run, so a casing mismatch
/// between the two is not expected in practice.
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs; [`exe_file_name_matches`] above is the pure decision this pre-filter
/// delegates to, and is what's actually unit tested.
fn find_pid_by_exe_path(exe_path: &Path) -> Option<u32> {
    /// Owns a toolhelp snapshot handle and closes it on every exit path.
    struct SnapshotGuard(windows_sys::Win32::Foundation::HANDLE);

    impl Drop for SnapshotGuard {
        fn drop(&mut self) {
            // SAFETY: the handle was returned by `CreateToolhelp32Snapshot`,
            // is owned solely by this guard and is closed exactly once here.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    // SAFETY: `0` (ignored for `TH32CS_SNAPPROCESS`) is the only argument;
    // `CreateToolhelp32Snapshot` takes no pointers.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if raw == INVALID_HANDLE_VALUE {
        return None;
    }
    let snapshot = SnapshotGuard(raw);

    // SAFETY: all-zero bytes are a valid value of this plain C struct.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

    // SAFETY: `snapshot.0` is the live handle just created above; `entry` is a
    // valid, correctly-sized out-pointer with `dwSize` set, per
    // `Process32FirstW`/`Process32NextW`'s documented contract.
    let mut has_entry = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
    while has_entry {
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        let file_name = String::from_utf16_lossy(&entry.szExeFile[..len]);

        if exe_file_name_matches(&file_name, exe_path)
            && window_filter::process_exe_path(entry.th32ProcessID).as_deref() == Some(exe_path)
        {
            return Some(entry.th32ProcessID);
        }

        // SAFETY: same as above.
        has_entry = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
    }
    None
}

/// Resolves `hwnd`'s current on-screen area in pixels (width * height), used
/// solely as [`choose_candidate_window`]'s size-disambiguation signal.
/// Returns `0` if the window's rect can't be read -- an unreadable window
/// simply never wins on size, it can still win via the foreground-match or
/// first-found rules.
fn window_area(hwnd: HWND) -> i64 {
    get_window_region(hwnd)
        .map(|r| r.width as i64 * r.height as i64)
        .unwrap_or(0)
}

/// Picks a single top-level window to treat as "the" window for `pid`,
/// gathering [`windows_for_pid`]'s candidates (paired with their
/// [`window_area`]) and the live [`foreground_window`] result, then deferring
/// the actual choice to the pure [`choose_candidate_window`].
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs; [`choose_candidate_window`] above is the pure decision this
/// delegates to, and is what's actually unit tested.
fn resolve_hwnd_for_pid(pid: u32) -> Option<HWND> {
    let (windows, refusals) = windows_for_pid(pid);
    if windows.is_empty() && !refusals.is_empty() {
        let mut reasons: Vec<String> = refusals.iter().map(ToString::to_string).collect();
        reasons.sort();
        reasons.dedup();
        log!(
            "no eligible window for process {pid}: {}",
            reasons.join("; ")
        );
    }
    let candidates: Vec<(HWND, i64)> = windows
        .into_iter()
        .map(|hwnd| (hwnd, window_area(hwnd)))
        .collect();
    choose_candidate_window(&candidates, foreground_window())
}

/// Resolves a running window for `exe_path`, for the live re-apply:
/// no live window handle is persisted in config, so each commit-triggered
/// re-apply must rediscover the tracked game's current window (if any) from
/// scratch via a fresh toolhelp snapshot + window enumeration.
///
/// Returns `None` if no running process matches `exe_path` (the game isn't
/// currently running -- callers must silently skip it: its setting still
/// saves normally and applies on its next launch/hotkey/tray-apply) or if one
/// does but it has no listable top-level window (e.g. still starting up with
/// no window shown yet).
///
/// OS-facing: not exercised by this crate's automated tests -- see module
/// docs.
pub(crate) fn resolve_running_window(exe_path: &Path) -> Option<HWND> {
    let pid = find_pid_by_exe_path(exe_path)?;
    resolve_hwnd_for_pid(pid)
}

/// Queries the primary monitor's geometry via a minimal Win32 call.
///
/// OS-facing -- not exercised by this crate's automated tests. Only the
/// primary monitor is supported (see `region_calc` module docs).
pub fn primary_monitor_geometry() -> MonitorGeometry {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

    // Safety: `GetSystemMetrics` is a simple, side-effect-free Win32 query
    // that takes a plain integer index and returns a plain integer; no
    // pointers or lifetimes are involved.
    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    // SAFETY: plain integer query with no pointers.
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    MonitorGeometry {
        x: 0,
        y: 0,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choose_candidate_window_returns_none_for_no_candidates() {
        assert_eq!(choose_candidate_window(&[], None), None);
    }

    #[test]
    fn choose_candidate_window_returns_the_sole_candidate() {
        let hwnd = 1usize as HWND;

        assert_eq!(choose_candidate_window(&[(hwnd, 100)], None), Some(hwnd));
    }

    #[test]
    fn choose_candidate_window_prefers_the_foreground_window_over_a_larger_candidate() {
        let foreground = 1usize as HWND;
        let larger = 2usize as HWND;

        let chosen =
            choose_candidate_window(&[(foreground, 10), (larger, 10_000)], Some(foreground));

        assert_eq!(chosen, Some(foreground));
    }

    #[test]
    fn choose_candidate_window_falls_back_to_the_largest_when_foreground_is_not_a_candidate() {
        let small = 1usize as HWND;
        let large = 2usize as HWND;
        let elsewhere = 3usize as HWND;

        let chosen = choose_candidate_window(&[(small, 10), (large, 10_000)], Some(elsewhere));

        assert_eq!(chosen, Some(large));
    }

    #[test]
    fn choose_candidate_window_falls_back_to_the_largest_when_there_is_no_foreground_window() {
        let small = 1usize as HWND;
        let large = 2usize as HWND;

        let chosen = choose_candidate_window(&[(small, 10), (large, 10_000)], None);

        assert_eq!(chosen, Some(large));
    }

    #[test]
    fn choose_candidate_window_prefers_the_first_found_when_areas_tie() {
        let first = 1usize as HWND;
        let second = 2usize as HWND;

        let chosen = choose_candidate_window(&[(first, 500), (second, 500)], None);

        assert_eq!(chosen, Some(first));
    }

    #[test]
    fn exe_file_name_matches_an_exact_case_insensitive_file_name() {
        let target = std::path::Path::new("C:\\Games\\Thing\\thing.exe");

        assert!(exe_file_name_matches("THING.EXE", target));
    }

    #[test]
    fn exe_file_name_matches_returns_false_for_a_different_file_name() {
        let target = std::path::Path::new("C:\\Games\\Thing\\thing.exe");

        assert!(!exe_file_name_matches("other.exe", target));
    }

    #[test]
    fn exe_file_name_matches_ignores_directory_components_of_the_target() {
        // The toolhelp snapshot's `szExeFile` is a bare file name, never a
        // full path -- this only ever compares against `target`'s own file
        // name, regardless of which directory it lives in.
        let target = std::path::Path::new("D:\\SomewhereElse\\thing.exe");

        assert!(exe_file_name_matches("thing.exe", target));
    }
}
