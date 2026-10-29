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
failed startup check is only logged (except a refused signature, see below). The check and the download use HTTPS through Windows' own WinHTTP, so the system certificate
store and proxy settings apply:

- The check is a GET of `api.github.com/repos/ranarn/gameframe/releases/latest`.
  A development build (version `0.0.0`) never makes it.
- The installer's download URL must be on `github.com`. The download follows
  redirects by hand (at most five), and every hop must be HTTPS on `github.com`
  or `release-assets.githubusercontent.com`; any other host is refused.
- Authenticity: every release installer is signed with the maintainer's
  [Minisign](https://jedisct1.github.io/minisign/) (Ed25519) key in the release
  workflow, which attaches the detached `GameFrame-Setup-<version>.exe.minisig`
  next to it. GameFrame embeds the matching public key (`PUBLIC_KEY` in
  `src/updater.rs`) and, before it runs an installer, downloads the `.minisig`
  (same host rules as the installer) and verifies the file against it. The
  signature carries a trusted comment, covered by it, that names the exact
  installer file (`file:GameFrame-Setup-<version>.exe`), and GameFrame requires
  it to match the release it is installing, so a genuinely signed older
  installer cannot be re-published as a newer release (replay or downgrade). A
  release with no signature, a signature that does not verify, a signature made
  for another release, or a key mismatch is refused: the download is deleted and
  nothing runs. There is no unsigned fallback. The refusal is never only in the
  log. A check or install you start in Settings shows why in the update row; an
  automatic install or the startup check shows one tray balloon ("... can't be
  verified and was not installed"); GameFrame tries again only at its next
  start. Scope of the protection: the
  signing key is held only as a secret of the protected `release` environment, so
  someone who can merely change release assets or the release page (a leaked
  token with write access, another compromised workflow) cannot get a file
  accepted. It does not protect against a takeover of the maintainer's GitHub
  account: the maintainer is the sole required reviewer and a repository admin,
  so whoever controls the account can run the release workflow and approve the
  signing, or relax the environment's rules. (The installer is not
  Authenticode-signed, so Windows SmartScreen and UAC still show an unknown
  publisher.) Releases published before this check was added do not verify
  signatures; it applies to every update installed by a version that contains it.
- Integrity: the installer is also hashed and compared with the SHA-256 digest
  the release API reports for it, which catches a corrupted download. The digest
  comes from the same GitHub response as the download URL, so on its own it
  proves nothing about who published the file. A file that does not match is
  deleted and never run. The verified file
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

## Release signing (maintainer)

Updates are only accepted when signed with the release key, so before the first
signed release:

1. Generate the key pair with [minisign](https://jedisct1.github.io/minisign/)
   0.12 or newer, on a trusted machine, with a password:
   `minisign -G -p gameframe.pub -s gameframe.key`. Keep `gameframe.key` and the
   password offline (a password manager); never commit them.
2. In the repository, create the GitHub Environment `release`
   (Settings -> Environments) with **required reviewers**, set **Deployment
   branches and tags** to **Selected branches and tags** -> `main` (so a
   workflow on another branch can never read the secrets), and add two
   environment secrets there: `MINISIGN_SECRET_KEY` (the full contents of
   `gameframe.key`) and `MINISIGN_PASSWORD` (its password; leave empty only for
   a key made with `-W`).
3. Replace the placeholder `PUBLIC_KEY` in `src/updater.rs` with the second line
   of `gameframe.pub` (the base64 string starting `RW`), commit it, and let it
   reach `main` before running the Release workflow. The workflow refuses to
   publish while the placeholder is in place, and it checks after signing that
   the signature verifies against that exact constant (or NEXT_PUBLIC_KEY, see below).
4. Enable **immutable releases** (Settings -> General -> Releases), so a
   published release and its assets can no longer be replaced. This setting is
   the guarantee: the workflow also refuses to re-upload to a release that is
   already published on a re-run, but a draft published between its check and
   its upload would slip past that check.

### Rotating the signing key

GameFrame trusts `PUBLIC_KEY` and, when set, `NEXT_PUBLIC_KEY` (both in
`src/updater.rs`; the second is empty normally). The release workflow accepts a
signature that verifies against either. A running install only knows the keys of
its own version, so rotate in two releases:

1. Generate the new key pair (step 1). Release N: set `NEXT_PUBLIC_KEY` to the
   new public key, keep the old secrets. N is signed with the old key and
   installs accept it; from then on they also trust the new key.
2. Once N is out, replace the `MINISIGN_SECRET_KEY` / `MINISIGN_PASSWORD`
   environment secrets with the new key. Release N+1: set `PUBLIC_KEY` to the
   new key and `NEXT_PUBLIC_KEY` back to the empty string. It is signed with the
   new key; installs of N accept it.

Limitation: an install older than N never learned the new key and refuses N+1
(the updater only offers the latest release). Keep releasing N+1 only after
those installs have updated through N, or have those users install the new
version manually. If the old key is already compromised, there is no trusted
channel left: every install has to be replaced by hand.