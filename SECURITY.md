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
when an applied window is moved or closed, on each borderless hotkey press, on
each taskbar hotkey press (to log the foreground window for diagnostics), while
the game-cursor overlay checks the window under the cursor, when listing windows
for the picker or a tracked game's re-apply, and when matching a tracked game's
executable against running processes. The loaded-module list is read only to
rank windows in the Apply to Window picker.
The only network traffic is the update check and the download that follows it.
GameFrame checks about 20 seconds after it starts (and again when you choose
"Check for Updates Now" in the tray or Settings). With the default setting
("Install updates automatically at startup") it then installs an available
update without asking; with "Ask me" it adds a tray item and installs only when
you choose it. A failed startup check is only logged. The check and the
download use HTTPS through Windows' own WinHTTP, so the system certificate
store and proxy settings apply:

- The check is a GET of `api.github.com/repos/ranarn/gameframe/releases/latest`.
  A development build (version `0.0.0`) never makes it.
- The installer's download URL must be on `github.com`. The download follows
  redirects by hand (at most five), and every hop must be HTTPS on `github.com`
  or `release-assets.githubusercontent.com`; any other host is refused.
- The installer is hashed and compared with the SHA-256 digest the release API
  reports for it. A file that does not match is deleted and never run. The
  verified file is held open with a share mode that denies writing and deleting
  until its process has started, so it cannot be swapped in between. It runs
  silently and detached from a temp folder, which the next start deletes.
- The installer asks the running instance to quit by posting a `Quit` command to
  the instance's hidden message-only window; `gameframe.exe --quit` sends the
  same command from a second launch. `Quit` travels on a separate registered
  window message that is not let through UIPI, so a lower-integrity process
  cannot quit the instance, and a normal-integrity launch cannot quit an
  elevated one (the installer then falls back to `taskkill` after a timeout).
  Opening Settings uses a message that is let through, since that is harmless.

Reports about any of these or the installer are in scope.