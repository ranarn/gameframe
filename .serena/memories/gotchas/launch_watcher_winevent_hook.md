## `SetWinEventHook` launch detection (issue #32)

### Feature-flag correction (important — corrects the issue's own brief/research doc)
`docs/research/idle-game-launch-detection.md` and issue #32's brief both
assumed `SetWinEventHook`/`UnhookWinEvent`/`HWINEVENTHOOK`/`WINEVENTPROC`
live in `windows_sys::Win32::UI::WindowsAndMessaging` (the same module as
`EVENT_SYSTEM_FOREGROUND`/`WINEVENT_OUTOFCONTEXT`). **Verified empirically
against the locally-cached windows-sys 0.61.2 source
(`C:\Users\<user>\.cargo\registry\src\index.crates.io-*\windows-sys-0.61.2\src\Windows\Win32\UI\Accessibility\mod.rs`)
to be wrong**: those four symbols actually live in
`windows_sys::Win32::UI::Accessibility`, gated by a separate Cargo feature,
`Win32_UI_Accessibility`, which was not enabled before this issue and has
been added to `Cargo.toml`. The event constants
(`EVENT_SYSTEM_FOREGROUND`, `WINEVENT_OUTOFCONTEXT`) *do* remain in
`Win32_UI_WindowsAndMessaging`, already enabled — only the hook
registration/teardown functions and their types were misplaced in the
research. If a future issue needs another `SetWindowsHookEx`/WinEvent-family
API, check `Win32::UI::Accessibility` first, not `WindowsAndMessaging`.

### Module split
- `src/launch_watcher.rs` (new): owns `register`/`unregister`
  (`SetWinEventHook`/`UnhookWinEvent`) and the raw `win_event_callback`
  (`unsafe extern "system" fn`, the exact `WINEVENTPROC` signature — no
  user-data/lparam slot, unlike `EnumWindows`'s callback). The config
  directory is stashed once in a `static WATCHED_CONFIG_DIR: OnceLock<PathBuf>`
  set from `register` (called once, from `app::run`, before
  `run_event_loop`'s `GetMessageW` pump starts — same thread, per
  `SetWinEventHook`'s documented message-loop requirement). The callback
  reloads config fresh from disk on every event via
  `app::handle_auto_apply_launch` — no cached `Config`.
- `src/app.rs`: `resolve_auto_apply(config, exe_path) -> Option<(TrackedGame,
  Preset)>` (pure, unit tested, `pub(crate)`) wraps `resolve_tracked_apply`
  plus an `auto_apply` flag check — the launch-detection-specific gate the
  hotkey/tray paths deliberately don't apply. `handle_auto_apply_launch`
  (OS-facing, not unit tested, `pub(crate)`) mirrors
  `handle_apply_to_window`'s one-shot apply shape exactly (border-apply via
  `win32::apply_borderless_to_window`, then taskbar-autohide-set gated on
  border-apply success) but with no upsert/save step — a game must already
  be tracked with `auto_apply: true` to reach an apply at all.
- `app::run()` calls `launch_watcher::register(&config_dir)` before
  `run_event_loop`, and `launch_watcher::unregister(hook)` after it returns
  (Exit path) if registration succeeded.

### Empirical validation performed (manual usertest, per this crate's convention)
Hand-edited `%APPDATA%\GameFrame\config.toml` to track a real classic Win32
exe (`C:\Windows\System32\winver.exe` — "About Windows"), ran the real
`gameframe.exe` idling in the tray, and launched `winver.exe` via
`Start-Process` while temporarily adding `eprintln!` debug tracing (since
reverted) to confirm the actual event flow. Confirmed via direct Win32
probing (`GetWindowLongPtr`/`GetWindowRect` from PowerShell):

- **The hook fires reliably** on every real foreground-window change,
  including for an *intermediate* window (Windows Terminal briefly became
  foreground between test runs) that correctly produced a "no auto-apply
  match" no-op rather than a false positive.
- **End-to-end success**: launching winver.exe with no hotkey press stripped
  its `WS_CAPTION` border and moved/resized it to exactly the Right-preset
  region (confirmed via `GetWindowRect` matching the primary monitor's right
  half) — fully automatic, zero hotkey/tray interaction.
- **First attempt failed silently** due to a **path-casing mismatch**
  (`C:\WINDOWS\system32\winver.exe` in my hand-written config vs. the
  actually-resolved `C:\Windows\System32\winver.exe` from
  `QueryFullProcessImageNameW`) — this is a live confirmation of the
  already-documented "`PathBuf` `Eq` case-sensitivity is an accepted risk"
  judgment call from issue #31 (see `mem:gotchas/tracked_games_apply_paths`,
  judgment call #5). Real tracked-game paths are always captured from a live
  resolved process (never hand-typed), so this is expected not to bite in
  practice — but it's now empirically confirmed as the exact failure mode if
  it ever did.
- **Not validated**: real Steam/Epic/GOG launcher-then-game sequences (where
  a launcher's window is briefly foreground before the actual game window
  replaces it) — no such launcher was available to test against in this
  session. The one intermediate-window case observed (Windows Terminal
  interposing between two `winver.exe` launches) behaved correctly (no false
  match), which is *some* evidence `EVENT_SYSTEM_FOREGROUND` alone handles
  "another unrelated window transiently in front" correctly, but doesn't
  confirm the specific launcher-hands-off-to-game-process pattern real
  storefronts use. Remains an open manual-usertest item per the issue's own
  acceptance criterion framing.
- **Packaged/MSIX app gotcha discovered incidentally**: modern Windows 11
  ships several classic tools (Notepad, Paint, Calculator) as packaged MSIX
  apps with an App Execution Alias stub at the classic path (e.g.
  `C:\WINDOWS\system32\notepad.exe`). Launching via that stub spawns a
  *different* real process at a `C:\Program Files\WindowsApps\...` path, and
  Notepad in particular is single-instance (reuses an existing window/tab
  rather than creating a fresh process). Not a GameFrame bug, but worth
  knowing if a future issue writes a usertest script against "notepad.exe"
  expecting a fresh classic process each launch — use a genuine classic
  Win32 exe (`winver.exe`, `regedit.exe`) for that instead.

See `mem:gotchas/tracked_games_apply_paths` for the pre-existing tracked-game
apply-path conventions this issue builds directly on, and
`docs/research/idle-game-launch-detection.md` for the full approach
comparison (WMI vs. toolhelp poll vs. WinEvent hook) that motivated choosing
`SetWinEventHook`.
