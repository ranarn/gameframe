Autostart + installer (branch feat/installer-autostart, 2026-09-30).

- `src/autostart.rs`: "Start with Windows" = HKCU `...\CurrentVersion\Run` value `GameFrame` (quoted exe path).
  The registry is the ONLY source of truth -- not in config.toml -- because installer, tray menu,
  Settings and Task Manager all change it. Always re-read `autostart::is_enabled()`, never cache.
- Task Manager "Disable" writes an override under `Explorer\StartupApproved\Run` (undocumented
  binary format; only sample seen on the dev machine was 12 zero bytes). We don't interpret it;
  enabling deletes the override instead. So a Task-Manager-disabled entry still reads as enabled.
- Tray `CheckMenuItem`: muda auto-flips the check on click (Windows backend) -> app toggles based on
  registry, then `Tray::sync_start_with_windows_check()` re-syncs. Also synced on tray Enter/Click and
  after Settings closes.
- Settings writes autostart on Save ONLY if the checkbox changed, so saving from a dev build never
  re-points the Run value at target\... exe.
- Tests use a scratch `HKCU\Software\GameFrameTest-<pid>-<name>` key via `RegistryLocation`, deleted
  with RegDeleteTreeW on drop.
- Installer: `installer/gameframe.iss` (Inno Setup 6, per-user, {autopf} = %LOCALAPPDATA%\Programs),
  built by `installer/build.ps1` (passes /DAppVersion from Cargo.toml). The .iss must keep the same
  value name `GameFrame` as autostart.rs. App has no graceful-close protocol, so install/uninstall
  `taskkill /F` it in [Code] (CloseApplications=no). Uninstall removes Run + StartupApproved values,
  keeps %APPDATA%\GameFrame config. AppId GUID must never change.
- `cargo build --release` fails with "Access is denied" if gameframe.exe is running from target\release.
