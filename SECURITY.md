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
elevated only when the borderless hotkey's window manipulation was denied for
lack of rights and you accept the UAC prompt. It moves and restyles other
applications' windows, hides the taskbar, installs a low-level keyboard hook only
while you are binding a hotkey, and never injects into game processes or reads
their memory. It queries a process's executable path through standard Windows
APIs: when the foreground window changes (to auto-apply a tracked game's Zone),
when an applied window moves, on each borderless hotkey press, when listing
windows for the Apply to Window picker or a tracked game's re-apply and when you
pick a window there, while the game-cursor overlay checks the window under the
cursor, when matching a tracked game's executable against running processes, and
to log the foreground window for diagnostics on each taskbar hotkey press and
whenever an apply hides the taskbar. The loaded-module list is read only to
rank windows in the Apply to Window picker.
The only network traffic is the update check and the download that follows it.
GameFrame checks about 20 seconds after it starts (and again when you choose
"Check for Updates Now" in the tray or Settings). With the default setting
("Install updates automatically at startup") it then installs an available
update without asking; with "Ask me" it adds a tray item and installs only when
you choose it. (An elevated instance installs in neither mode, see below.) A
failed startup check is only logged. The check and the download use HTTPS through Windows' own WinHTTP, so the system certificate
store and proxy settings apply:

- The check is a GET of `api.github.com/repos/ranarn/gameframe/releases/latest`.
  A development build (version `0.0.0`) never makes it.
- The installer's download URL must be on `github.com`. The download follows
  redirects by hand (at most five), and every hop must be HTTPS on `github.com`
  or `release-assets.githubusercontent.com`; any other host is refused.
- The installer is hashed and compared with the SHA-256 digest the release API
  reports for it. This is an integrity check against a corrupted or altered
  download, not a publisher signature: the installer is not Authenticode-signed
  yet. A file that does not match is deleted and never run. The verified file
  is held open with a share mode that denies writing and deleting until its
  process has started, so it cannot be swapped in between. It runs silently and
  detached from a folder under `%LOCALAPPDATA%\GameFrame\updates`, which the
  next start deletes. That is a fixed, app-owned location that inherits your
  profile's access rules and doesn't depend on a redirectable `%TEMP%`.
- An elevated (administrator) instance never installs an update: it reports that
  one exists and asks you to restart GameFrame normally. Setup also does not
  start GameFrame (neither the update's `/RELAUNCH` nor the "Launch GameFrame"
  checkbox) when it runs elevated, so an update can't leave GameFrame elevated.
  If GameFrame cannot read its own token, it counts as elevated. With UAC turned
  off, an administrator's processes are always elevated, so GameFrame never
  installs updates itself there and setup never starts it: install new versions
  by running setup yourself and start GameFrame afterwards.
- GameFrame's own static imports are resolved from System32 only
  (`/DEPENDENTLOADFLAG:0x800` at link time), so a DLL planted next to the exe
  cannot replace them.
- The installer asks the running instance to quit by posting a `Quit` command to
  the instance's hidden message-only window; `gameframe.exe --quit` sends the
  same command from a second launch. `Quit` travels on a separate registered
  window message that is not let through UIPI, so a lower-integrity process
  cannot quit the instance, and a normal-integrity launch cannot quit an
  elevated one. Setup detects an elevated instance up front and, instead of
  using `taskkill`, asks you to exit GameFrame yourself (Retry/Cancel); a silent
  run fails.
  Opening Settings uses a message that is let through, since that is harmless.

Reports about any of these or the installer are in scope.