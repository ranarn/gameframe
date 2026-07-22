# GameFrame

A Windows system-tray utility that makes a game/app window borderless and repositions it to a defined screen region, and toggles taskbar auto-hide via a global hotkey. Replaces two separate background tools (Windowed Borderless Gaming + a standalone Hide Taskbar utility) with one process.

Status: early scaffold, not yet implemented.

Full problem statement, research, and design decisions: [`Project Ideas/gameframe.md`](../Project%20Ideas/gameframe.md).

## Tech stack

- Rust
- `windows` crate — Win32 bindings (window manipulation, DWM, `SHAppBarMessage` for taskbar)
- `tray-icon` + `muda` — system tray
- `egui` — settings UI, lazily instantiated only while open (not resident at idle)
- `global-hotkey` — hotkey bindings
- TOML config in `%APPDATA%\GameFrame\`
