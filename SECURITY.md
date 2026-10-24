# Security policy

## Supported versions

Only the latest release receives fixes. GameFrame updates itself, so staying on
the latest version is the expected setup.

## Reporting a vulnerability

Please **do not open a public issue** for a security problem. Report it privately
through GitHub instead: the repository's **Security** tab → **Report a
vulnerability**.

Include what you found, how to reproduce it, and which GameFrame version you
used. You'll get a reply as soon as possible; once a fix is released, the report
is published as a security advisory with credit to you, unless you'd rather stay
anonymous.

## Scope

GameFrame runs unelevated as a per-user tray app. It moves and restyles other
applications' windows, hides the taskbar, installs a low-level keyboard hook only
while you are binding a hotkey, and never injects into or reads the memory of
game processes. Reports about any of these, the installer, or the update
mechanism are in scope.
