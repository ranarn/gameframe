# GameFrame

GameFrame is a small Windows tray app for playing games in a borderless window. Draw a Zone on
your screen once, and GameFrame strips the border from your games and places them in it,
automatically when they launch or with a hotkey. It can hide the taskbar while you play and show
a large, easy-to-see cursor over your games.

## Install

1. Download `GameFrame-Setup-<version>.exe` from the
   [latest release](https://github.com/ranarn/gameframe/releases/latest).
2. Run it. GameFrame installs for your user only (no admin prompt) and can start with Windows.

Requires 64-bit Windows 10 or 11. To upgrade, run a newer installer; your settings are kept.
Uninstall from Settings > Apps.

**Upgrading to 1.0:** configs from the pre-Zone preset era (the ones with a `[presets]` table) are no longer migrated and are reset to defaults on first start; the old file is kept as a backup next to the config (`config.toml.bak`, or `config.toml.bak.N` if one already exists). Configs from 0.x releases that already use the Zone are unaffected.

## Getting started

1. Right-click the GameFrame icon in the system tray and choose **Show Settings**.
2. Under **Zone, hotkeys & taskbar**, click **Draw zone** and drag out the area your games should
   fill. Edges snap to the screen border. Click **Save**.
3. Start a game, then pick its window under **Apply to Window** in the tray menu. The game is now
   a *tracked game*: its border is removed and it is placed in the Zone, now and every time it
   launches.

## Features

- **Zone** — one screen area, drawn on the primary monitor, that every tracked game is placed in.
  Changing it re-applies it to every running tracked game.
- **Tracked games** — listed under **Games** in Settings, each with its own **Auto-apply** (place
  the game in the Zone when it launches) and **Game cursor** switch.
- **Taskbar hiding** — off by default; optionally hide the taskbar while a tracked game is in the
  Zone, either fully hidden or Windows' hover-to-reveal auto-hide. The taskbar comes back when the
  game closes or GameFrame exits.
- **Game cursor** — a large, high-contrast pointer for games whose own cursor is easy to lose:
  several shapes, or an enlarged copy of the game's own cursor, with size, color, opacity, border
  and glow. Shown only over tracked games; shake the mouse to make it pulse. It never touches the
  game process.
- **Global hotkeys** — bind keys in Settings to toggle borderless on the foreground window, toggle
  taskbar auto-hide, and toggle the game cursor. No hotkeys are bound by default.
- **Start with Windows** — from the tray menu, Settings or the installer.

Settings are stored in `%APPDATA%\GameFrame\config.toml`, and a log in
`%APPDATA%\GameFrame\gameframe.log`.

## Known limitations

- The installer is not code-signed, so Windows SmartScreen may warn the first time you run it.
  Choose **More info**, then **Run anyway**.
- The Zone works on a single monitor only. The "fully hidden" taskbar mode does not hide the
  taskbars of secondary monitors.
- A game running as administrator can't be applied from the tray or by auto-apply. The
  borderless hotkey offers to restart GameFrame elevated.
- UWP/packaged games (windows hosted by `ApplicationFrameHost`) and windows with dialog frames are
  refused by design.

## Building from source

Requires a stable Rust toolchain targeting `x86_64-pc-windows-msvc`.

```powershell
cargo run              # debug build
cargo test             # tests
cargo build --release  # target\release\gameframe.exe
```

The release build is a single self-contained executable; the C runtime is statically linked.

To build the installer, install [Inno Setup 7](https://jrsoftware.org/isinfo.php)
(`winget install JRSoftware.InnoSetup.7`) and
[cargo-about](https://github.com/EmbarkStudios/cargo-about)
(generates the third-party license notices; `build.ps1` checks the installed version against the one pinned in
`.github/actions/install-cargo-about/action.yml` and prints the exact `cargo install` command if it differs), then run `.\installer\build.ps1`. The result is
`target\installer\GameFrame-Setup-<version>.exe`.

## Support

GameFrame is free and open source. If it's useful to you and you'd like to support its
development, you can [donate via PayPal](https://www.paypal.com/donate/?hosted_button_id=YPDPUVMSDQLJQ) —
entirely voluntary, and every feature stays free either way.

Found a bug? [Open an issue](https://github.com/ranarn/gameframe/issues/new/choose). Security
problems: see [`SECURITY.md`](SECURITY.md).

## License

Copyright (C) 2026 Jonas Ranerfors

GameFrame is free software: you can redistribute it and/or modify it under the terms of
the GNU General Public License as published by the Free Software Foundation, either
version 3 of the License, or (at your option) any later version. See [`LICENSE`](LICENSE).
