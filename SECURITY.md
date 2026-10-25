# Security policy

## Supported versions

Only the latest release receives fixes.

## Reporting a vulnerability

Please **do not open a public issue** for a security problem. Report it privately
through GitHub instead: the repository's **Security** tab → **Report a
vulnerability**.

Include what you found, how to reproduce it, and which GameFrame version you
used. You'll get a reply as soon as possible; once a fix is released, the report
is published as a security advisory with credit to you, unless you'd rather stay
anonymous.

## Scope

GameFrame runs unelevated by default as a per-user tray app. It relaunches itself
elevated only when a window manipulation was denied for lack of rights and you
accept the UAC prompt. It moves and restyles other applications' windows, hides
the taskbar, installs a low-level keyboard hook only while you are binding a
hotkey, and never injects into or reads the memory of game processes. Reports
about any of these or the installer are in scope.
