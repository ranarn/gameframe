## Window-filtering/ranking logic lives in `src/window_filter.rs`, not `src/win32.rs`

Issue #27 (game-window detection/filtering for "Apply to Window") originally
landed all of its Layer 1 (hard filters: cloaked/owner-chain/tool-window) and
Layer 2 (ranking-only: Steam launcher match, loaded graphics module, engine
window class) logic inline in `src/win32.rs`, growing it past 1000 lines. A
review finding (fixed same issue) extracted it into its own module,
`src/window_filter.rs`, registered via `mod window_filter;` in `main.rs`.

Boundary:
- `src/window_filter.rs` owns: `is_listable_window` (Layer 1 pure predicate,
  `pub(crate)`), `WindowRank`/`rank_window` (Layer 2 pure decision, `pub`/
  `pub(crate)`), the engine-window-class and graphics-module heuristics, the
  cached `steamlocate` Steam-install-dir scan (60s TTL,
  `STEAM_INSTALL_DIRS_CACHE_TTL`), and the OS-facing glue that gathers
  Layer-1/Layer-2 signals per `HWND` (`window_ex_style`,
  `is_root_owner_or_last_active_popup`, `is_window_cloaked`,
  `rank_window_for_hwnd`). All of these OS-facing fns are `pub(crate)` so
  `win32.rs` can call them.
- `src/win32.rs` keeps `list_visible_windows`/`enum_windows_callback` (they
  own the real `EnumWindows` call) and calls into `window_filter::*` for the
  actual filtering/ranking decision per candidate window.
- `rank_window_for_hwnd(hwnd, cache: &mut HashMap<u32, WindowRank>)` takes an
  explicit per-PID memoization cache so a process owning multiple top-level
  listable windows only pays for one `OpenProcess`/`CreateToolhelp32Snapshot`/
  module-walk per `list_visible_windows()` call, not one per window. The
  cache lives in a private `EnumWindowsState { windows, rank_cache }` struct
  in `win32.rs`, smuggled through `EnumWindows`'s `LPARAM` exactly like the
  old bare `Vec` was.
- `loaded_module_names` snapshots with `TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32`
  (not `TH32CS_SNAPMODULE` alone) -- a 64-bit caller needs the `_32` flag
  too to reliably enumerate a 32-bit (WOW64) target's modules; many
  older/indie games are still 32-bit, and this is exactly the class of game
  the loaded-graphics-module signal exists to help rank.

Both modules follow the same pure/OS-facing split convention as
`is_listable_window`/`list_visible_windows` always did: pure decision logic
is fully unit tested (`window_filter::tests`, `win32::tests`); the real Win32
calls (`EnumWindows`, `DwmGetWindowAttribute`, `CreateToolhelp32Snapshot`,
`OpenProcess`, the `steamlocate` scan, `GetAncestor`/`GetLastActivePopup`)
are NOT exercised by automated tests -- manual usertest only, per this
crate's established convention (see `mem:core`'s note on `tray_ui`/`app` for
the same pattern applied earlier).

Layer 2 ranking NEVER excludes a window from the list -- `list_visible_windows`
sorts by descending `WindowRank` (stable sort) but never filters on rank; an
unrecognized (`WindowRank::Unknown`) window always remains pickable, just
un-prioritized. Only Layer 1 (`is_listable_window`) can exclude.
