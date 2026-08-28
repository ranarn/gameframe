Language: Rust, edition 2024 (`Cargo.toml`). Package name `gameframe`, version 0.1.0.

Dependencies (issue #3, `profile_store`):
- `serde` (with `derive` feature) — for (de)serializing config types.
- `toml` — TOML read/write.
- Dev-dependency: `tempfile` — used for filesystem-isolated tests (unique temp dirs, no real
  `%APPDATA%` writes in tests).

Dependencies (issue #4, idle tray shell):
- `tray-icon` (0.24.1) + `muda` (0.19.3, re-exported by `tray-icon` as `tray_icon::menu`) — system
  tray icon and its context menu. Default features enable `gtk`/`libxdo` for Linux, but those are
  `[target.'cfg(target_os = "linux")'...]`-gated in the crates' own `Cargo.toml`s so nothing GTK
  builds on Windows — confirmed via `cargo build` (no gtk/cairo/glib compiled).
- `windows-sys` (0.61, features `Win32_UI_WindowsAndMessaging` + `Win32_Foundation`) — used
  directly (not via the `windows` crate) for the minimal Win32 message-pump
  (`GetMessageW`/`TranslateMessage`/`DispatchMessageW`) that `tray-icon` requires running on the
  same thread as the tray icon (its docs: "an event loop must be running on the thread, on
  Windows a win32 event loop"). No `winit`/`tao` dependency added — a raw message loop was enough
  and keeps the dep tree smaller; revisit if a later slice (settings UI / hotkeys) wants a real
  windowing crate's event loop instead.

Still-planned (not yet added) per README:
- `windows` crate — Win32 bindings for window manipulation, DWM, `SHAppBarMessage` (taskbar). Not
  needed yet — issue #4 only needed the message pump, covered by `windows-sys` directly.
- `egui` — settings UI; intended to be lazily instantiated only while open, not resident at idle.
- `global-hotkey` — hotkey bindings (hotkey *binding storage* exists in `profile_store` as plain
  `Option<String>` key-combo strings, but no registration/handling logic yet).
- Config format: TOML, stored under `%APPDATA%\GameFrame\` — but `profile_store` itself never
  resolves that path; it takes an injected `&Path` config directory. Resolving the real
  `%APPDATA%` path is the caller's job (not yet wired up — no consumer of `profile_store` exists
  yet; issue #4 explicitly did not wire it in, out of scope).

Target platform: Windows only. `tray_ui`/`app` (issue #4) are the first modules with real
Windows-only OS calls (Win32 menu/tray/message-loop APIs) — no `cfg(windows)` gating was added
since the whole crate already targets Windows only.
