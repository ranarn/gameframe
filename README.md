# GameFrame

A Windows system-tray utility that makes a game/app window borderless and repositions it to a
defined screen region, and toggles taskbar auto-hide via a global hotkey. Replaces two separate
background tools (Windowed Borderless Gaming + a standalone Hide Taskbar utility) with one process.

Design decisions: [`docs/adr/`](docs/adr/); background research: [`docs/research/`](docs/research/);
domain vocabulary: [`CONTEXT.md`](CONTEXT.md).

## Status

MVP complete. GameFrame runs as a tray-resident background process with:

- **Tray icon** with a right-click menu: **Show Settings**, **Apply to Window** (a submenu listing
  currently open, listable windows — picking one applies the Zone to it), and
  **Exit**.
- **Global hotkeys**:
  - Borderless toggle — strips the window border/titlebar from the current foreground window and
    moves/resizes it to the Zone; pressing it again restores the window's original
    style and position.
  - Taskbar auto-hide toggle — flips the Windows taskbar between always-visible and auto-hide.
  - Game cursor toggle — turns the game cursor (below) on/off.
- **Game cursor** — a large, high-contrast pointer for games whose own cursor gets lost (a
  YoloMouse replacement that never touches the game process): seven shapes, or "Game's own" —
  an enlarged copy of the game's own cursor, with border and glow following its outline —
  plus size, color, opacity, border and glow, shown only while the mouse is over a tracked
  game, drawn as a click-through overlay. Shake the mouse to make it pulse. See
  `docs/adr/0004-game-cursor-without-injection.md`.
- **Log file** — `%APPDATA%\GameFrame\gameframe.log` (capped at 1 MB, previous file kept as
  `gameframe.old.log`): errors, plus the taskbar's real state on every taskbar-hotkey press.
- **Settings window** (`egui`, opened on demand from the tray menu, not resident at idle) for
  drawing/editing the Zone (the single screen region games are placed into) and
  binding the two global hotkeys.
- **Config persistence** — the Zone and hotkey bindings are saved as TOML to
  `%APPDATA%\GameFrame\config.toml` and reloaded on the next launch.

## Tech stack

- Rust (edition 2024)
- `windows-sys` — Win32 bindings (window manipulation, DWM, `SHAppBarMessage` for taskbar)
- `tray-icon` + `muda` — system tray icon and menu
- `egui` + `eframe` — Settings UI, lazily instantiated only while open (not resident at idle)
- `global-hotkey` — global hotkey registration
- `serde` + `toml` — config (de)serialization

## Building and running

Requires a Rust toolchain (edition 2024) targeting `x86_64-pc-windows-msvc`.

```powershell
# Debug run, for development
cargo run

# Run the test suite
cargo test

# Release build
cargo build --release
```

The release build produces a single self-contained executable at
`target\release\gameframe.exe` — no separate runtime DLLs are required (the CRT is statically
linked via `.cargo\config.toml`; the binary depends only on standard Windows system DLLs already
present on any Windows install).

### Installer

```powershell
# One-time: install Inno Setup 7
winget install JRSoftware.InnoSetup.7

# Build target\installer\GameFrame-Setup-<version>.exe
.\installer\build.ps1
```

The installer (`installer\gameframe.iss`) installs per user, with no admin prompt, to
`%LOCALAPPDATA%\Programs\GameFrame`. It adds a Start-menu shortcut and an entry under
Settings > Apps for uninstalling, and offers a "Start GameFrame with Windows" checkbox. Running a
newer installer upgrades in place, and your config in `%APPDATA%\GameFrame` is kept, including on
uninstall.

**Start with Windows** is the `GameFrame` value under
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. The installer checkbox, the tray menu's
"Start with Windows" item and the Settings window's checkbox all read and write that same value,
so they always agree.

The release profile (`[profile.release]` in `Cargo.toml`) is tuned for binary size
(`opt-level = "z"`, LTO, single codegen unit, `panic = "abort"`, symbols stripped), since `egui`/
`eframe`/`wgpu`/`winit` pull in a sizeable dependency tree. Note: `panic = "abort"` skips
unwind-based `Drop` cleanup process-wide if the process ever panics unexpectedly — e.g.
`tray-icon`'s Windows `Drop` impl that removes the tray icon on drop would not run in that case.
This is a common, reasonable size-optimization trade-off, not a defect.

### Measured results

Measured on a Windows 11 development sandbox (not dedicated gaming hardware):

- **Binary size**: `gameframe.exe` is ~8.25 MB (8,650,240 bytes) — under the 10 MB budget.
- **Runtime DLLs**: none alongside the executable; `dumpbin /dependents` shows only standard
  Windows system DLLs (`kernel32.dll`, `user32.dll`, `gdi32.dll`, `ole32.dll`, `shell32.dll`,
  `dwmapi.dll`, `comctl32.dll`, `uxtheme.dll`, etc.) — no `VCRUNTIME140.dll` or other VC++
  redistributable dependency, thanks to static CRT linking.
- **Idle memory**: with only the tray icon and hotkey listeners active (no Settings window open),
  steady-state working set was ~13.6 MB (14,299,136 bytes), measured via `Get-Process` a few
  seconds after launch once memory stopped climbing. This is informational rather than a hard
  gate, but it is **above** the ≤5 MB soft budget by roughly 8.6 MB — cause not yet identified.
  Note that `src/app.rs::run()` only constructs the tray icon and registers hotkeys before entering
  the message loop; `eframe`/the Settings window is not instantiated until the user clicks "Show
  Settings," so GPU/windowing runtime state is not the explanation. The overage is more likely
  attributable to the baseline Rust/CRT process footprint plus the `tray-icon` and `global-hotkey`
  runtime dependencies, but that has not been confirmed by profiling. Re-measuring on real
  end-user hardware, and profiling to pin down the actual source, is recommended.

## Support

GameFrame is free and open source. If it's useful to you and you'd like to support its
development, you can [donate via PayPal](https://www.paypal.com/donate/?hosted_button_id=YPDPUVMSDQLJQ) —
entirely voluntary, and every feature stays free either way.

## License

Copyright (C) 2026 Jonas Ranerfors

GameFrame is free software: you can redistribute it and/or modify it under the terms of
the GNU General Public License as published by the Free Software Foundation, either
version 3 of the License, or (at your option) any later version. See [`LICENSE`](LICENSE).

Security issues: see [`SECURITY.md`](SECURITY.md).
